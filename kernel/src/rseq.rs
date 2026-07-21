//! Restartable sequences 用户 ABI 处理。
//!
//! `sched` 只记录事件并执行纯判定；本模块在当前任务地址空间中读取 `struct rseq`
//! 和 `struct rseq_cs`，校验 abort signature，然后通过 HAL 修改架构无关的用户 PC。

use alloc::sync::Arc;

use errno::Errno;
use general::mm::{copy_from_user, copy_to_user, user_vm_layout};
use hal::user_context::UserTrapFrame;
use sched::{RseqCs, RseqResumeAction, Task, UserContextRef, validate_signature};

const RSEQ_CS_PTR_OFFSET: usize = 8;
const RSEQ_FLAGS_OFFSET: usize = 16;
const RSEQ_CS_SIZE: usize = 32;
const RSEQ_SIGNATURE_SIZE: usize = 4;

fn read_u32(user: usize) -> Result<u32, Errno> {
    let mut raw = [0u8; 4];
    copy_from_user(user, &mut raw).map_err(|error| error.as_errno())?;
    Ok(u32::from_ne_bytes(raw))
}

fn read_u64(user: usize) -> Result<u64, Errno> {
    let mut raw = [0u8; 8];
    copy_from_user(user, &mut raw).map_err(|error| error.as_errno())?;
    Ok(u64::from_ne_bytes(raw))
}

fn write_u64(user: usize, value: u64) -> Result<(), Errno> {
    copy_to_user(user, &value.to_ne_bytes()).map_err(|error| error.as_errno())
}

fn parse_rseq_cs(user: usize) -> Result<RseqCs, Errno> {
    let mut raw = [0u8; RSEQ_CS_SIZE];
    copy_from_user(user, &mut raw).map_err(|error| error.as_errno())?;

    let read32 = |offset: usize| {
        let mut bytes = [0u8; 4];
        bytes.copy_from_slice(&raw[offset..offset + 4]);
        u32::from_ne_bytes(bytes)
    };
    let read64 = |offset: usize| {
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&raw[offset..offset + 8]);
        u64::from_ne_bytes(bytes)
    };
    let to_usize = |value: u64| usize::try_from(value).map_err(|_| Errno::EINVAL);

    Ok(RseqCs {
        version: read32(0),
        flags: read32(4),
        start_ip: to_usize(read64(8))?,
        post_commit_offset: to_usize(read64(16))?,
        abort_ip: to_usize(read64(24))?,
    })
}

fn user_address_limit() -> Result<usize, Errno> {
    let layout = user_vm_layout().ok_or(Errno::ENOSYS)?;
    let vdso_end = layout
        .vdso_base
        .checked_add(layout.page_size.saturating_mul(2))
        .ok_or(Errno::EINVAL)?;
    Ok(layout
        .user_mmap_limit
        .max(layout.default_stack_top)
        .max(vdso_end))
}

/// 消费当前任务尚未处理的 rseq 事件，并在需要时重写返回 PC。
pub(crate) fn prepare_user_return(task: &Arc<Task>, user_ctx: UserContextRef) -> Result<(), Errno> {
    if user_ctx.is_none() {
        return Err(Errno::ENOSYS);
    }
    let registration = task.rseq_registration();
    let events = task.rseq_events();
    if !registration.registered || events.is_empty() {
        return Ok(());
    }

    let cs_ptr_addr = registration
        .ptr
        .checked_add(RSEQ_CS_PTR_OFFSET)
        .ok_or(Errno::EFAULT)?;
    let flags_addr = registration
        .ptr
        .checked_add(RSEQ_FLAGS_OFFSET)
        .ok_or(Errno::EFAULT)?;
    let cs_ptr_raw = read_u64(cs_ptr_addr)?;
    if cs_ptr_raw == 0 {
        task.clear_rseq_events(events);
        return Ok(());
    }
    let cs_ptr = usize::try_from(cs_ptr_raw).map_err(|_| Errno::EINVAL)?;
    let rseq_flags = read_u32(flags_addr)?;
    let critical = parse_rseq_cs(cs_ptr)?;
    let frame = UserTrapFrame::from_context(user_ctx.as_usize());
    let action = sched::rseq::decide_resume(
        frame.pc(),
        user_address_limit()?,
        rseq_flags,
        critical,
        events,
    )
    .map_err(|_| Errno::EINVAL)?;

    let signature_addr = critical
        .abort_ip
        .checked_sub(RSEQ_SIGNATURE_SIZE)
        .ok_or(Errno::EINVAL)?;
    validate_signature(registration.signature, read_u32(signature_addr)?)
        .map_err(|_| Errno::EINVAL)?;

    match action {
        RseqResumeAction::Keep => {}
        RseqResumeAction::Clear => write_u64(cs_ptr_addr, 0)?,
        RseqResumeAction::AbortTo(abort_ip) => {
            write_u64(cs_ptr_addr, 0)?;
            let mut frame = frame;
            frame.set_pc(abort_ip);
            frame.apply_to_context(user_ctx.as_usize());
        }
    }
    task.clear_rseq_events(events);
    Ok(())
}
