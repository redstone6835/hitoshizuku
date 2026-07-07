//! ELM 私有系统调用粘合层。

use alloc::vec;
use alloc::vec::Vec;

use elm_model::{ELM_MGR_MAX_INPUT, ElmCtlCommand};
use errno::Errno;
use general::mm::{copy_from_user, copy_to_user};
use general::syscall::SyscallContext;
use sched::ids::Capability;

use super::{event, mgr_channel, snapshot, with_core};

pub(crate) fn sys_elm_ctl(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let command = ElmCtlCommand::from_raw(ctx.args[0]).ok_or(Errno::EINVAL)?;
    let input_user = ctx.args[1];
    let input_len = ctx.args[2];
    let output_user = ctx.args[3];
    let output_len = ctx.args[4];

    match command {
        ElmCtlCommand::CoreQuery => {
            let info = with_core(|core| core.core_info());
            write_plain(output_user, output_len, &info)
        }
        ElmCtlCommand::SnapshotRead => {
            let bytes = with_core(|core| snapshot::snapshot_bytes(core));
            write_bytes(output_user, output_len, &bytes)
        }
        ElmCtlCommand::EventRead => {
            let record = with_core(|core| event::read_next_event(core))?;
            write_plain(output_user, output_len, &record)
        }
        ElmCtlCommand::EventAck => {
            let sequence = read_user_u64(input_user, input_len)?;
            with_core(|core| event::ack_event(core, sequence));
            Ok(0)
        }
        ElmCtlCommand::MgrCall => {
            require_sys_admin(ctx)?;
            let input = read_input_bytes(input_user, input_len, ELM_MGR_MAX_INPUT)?;
            let response = mgr_channel::dispatch_mgr_call(&input);
            write_bytes(output_user, output_len, &response)
        }
        ElmCtlCommand::DebugDump => {
            require_sys_admin(ctx)?;
            let bytes = with_core(|core| core.debug_dump_bytes());
            write_bytes(output_user, output_len, &bytes)
        }
    }
}

fn require_sys_admin(ctx: &SyscallContext<'_>) -> Result<(), Errno> {
    if ctx.task().credentials().has_cap(Capability::SysAdmin) {
        Ok(())
    } else {
        Err(Errno::EPERM)
    }
}

fn read_user_u64(user: usize, len: usize) -> Result<u64, Errno> {
    if user == 0 {
        return Err(Errno::EFAULT);
    }
    if len < 8 {
        return Err(Errno::EINVAL);
    }
    let mut raw = [0u8; 8];
    copy_from_user(user, &mut raw).map_err(|err| err.as_errno())?;
    Ok(u64::from_le_bytes(raw))
}

fn read_input_bytes(user: usize, len: usize, max_len: usize) -> Result<Vec<u8>, Errno> {
    if len > max_len {
        return Err(Errno::EMSGSIZE);
    }
    if len == 0 {
        return Ok(Vec::new());
    }
    if user == 0 {
        return Err(Errno::EFAULT);
    }
    let mut bytes = vec![0; len];
    copy_from_user(user, &mut bytes).map_err(|err| err.as_errno())?;
    Ok(bytes)
}

fn write_plain<T>(user: usize, len: usize, value: &T) -> Result<usize, Errno> {
    write_bytes(user, len, plain_bytes(value))
}

fn write_bytes(user: usize, len: usize, bytes: &[u8]) -> Result<usize, Errno> {
    if bytes.is_empty() {
        return Ok(0);
    }
    if user == 0 {
        return Err(Errno::EFAULT);
    }
    if len < bytes.len() {
        return Err(Errno::EMSGSIZE);
    }
    copy_to_user(user, bytes).map_err(|err| err.as_errno())?;
    Ok(bytes.len())
}

fn plain_bytes<T>(value: &T) -> &[u8] {
    // 安全性：调用点只传入 ELM 控制面 `#[repr(C)]` 固定布局结构，
    // 这些结构不包含内核指针，用户态只按字节解析。
    unsafe {
        core::slice::from_raw_parts((value as *const T).cast::<u8>(), core::mem::size_of::<T>())
    }
}
