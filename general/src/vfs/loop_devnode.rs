//! loop devtmpfs 兼容节点适配层。
//!
//! 本模块是 Linux loop ABI 的唯一解释点：`/dev/loop-control`、`/dev/loopN`
//! 的 ioctl 命令号、`struct loop_info64` 布局、用户指针拷贝和 fd 解析都停留在
//! VFS 边界。底层 [`crate::dev::loopdev`] 只接收 typed backing 与 typed status。

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::fmt::Write;
use core::mem::{MaybeUninit, size_of};
use core::ops::ControlFlow;

use errno::Errno;
use vfs::error::{VfsError, VfsResult};
use vfs::fdtable::Fd;
use vfs::file::{DirEntry, File, FileOps, IoctlCmd, OpenOptions, PollEvents};
use vfs::inode::{Inode, InodeOps};
use vfs::stat::FileType;
use vfs::sync::Spinlock;

use crate::dev::block::BlockDevice;
use crate::dev::enumerate::DEVICES;
use crate::dev::function::{
    BlockFunction, CustomDevNodeKind, CustomDevNodeSpec, DevNodeSpec, DeviceFunction,
    FunctionRegistryError,
};
use crate::dev::loopdev::{
    LoopAttachOptions, LoopBacking, LoopBackingError, LoopDeviceBundle, LoopDriver, LoopError,
    LoopFlags, LoopStatus,
};
use crate::mm::{copy_from_user, copy_to_user};
use crate::vfs::devtmpfs::{
    DevTmpfsCustomNodeAdapter, DevTmpfsCustomNodeAdapterRegistration, DevTmpfsStaticNode,
    DevTmpfsStaticNodeRegistration, bind_dynamic_devnodes, register_custom_devnode_adapter,
    register_static_dev_node, unbind_dynamic_devnodes,
};

const LOOP_CONTROL_NODE_NAME: &str = "loop-control";
const LOOP_DEVNODE_OWNER: &str = "loop-devnode";
const LOOP_ADAPTER_NAME: &str = "loop";

const LO_NAME_SIZE: usize = 64;
const LO_KEY_SIZE: usize = 32;

const LO_FLAGS_READ_ONLY: u32 = 1;
const LO_FLAGS_AUTOCLEAR: u32 = 4;
const LO_FLAGS_PARTSCAN: u32 = 8;
const LO_FLAGS_DIRECT_IO: u32 = 16;

const LOOP_SET_FD: usize = 0x4c00;
const LOOP_CLR_FD: usize = 0x4c01;
const LOOP_SET_STATUS64: usize = 0x4c04;
const LOOP_GET_STATUS64: usize = 0x4c05;
const LOOP_SET_CAPACITY: usize = 0x4c07;
const LOOP_SET_DIRECT_IO: usize = 0x4c08;
const LOOP_SET_BLOCK_SIZE: usize = 0x4c09;
const LOOP_CONFIGURE: usize = 0x4c0a;

const LOOP_CTL_ADD: usize = 0x4c80;
const LOOP_CTL_REMOVE: usize = 0x4c81;
const LOOP_CTL_GET_FREE: usize = 0x4c82;
const ENXIO: Errno = Errno::Other(6);

/// Linux `struct loop_info64` 的 ABI 布局。
#[repr(C)]
#[derive(Clone, Copy)]
struct LinuxLoopInfo64 {
    lo_device: u64,
    lo_inode: u64,
    lo_rdevice: u64,
    lo_offset: u64,
    lo_sizelimit: u64,
    lo_number: u32,
    lo_encrypt_type: u32,
    lo_encrypt_key_size: u32,
    lo_flags: u32,
    lo_file_name: [u8; LO_NAME_SIZE],
    lo_crypt_name: [u8; LO_NAME_SIZE],
    lo_encrypt_key: [u8; LO_KEY_SIZE],
    lo_init: [u64; 2],
}

impl LinuxLoopInfo64 {
    const fn zeroed() -> Self {
        Self {
            lo_device: 0,
            lo_inode: 0,
            lo_rdevice: 0,
            lo_offset: 0,
            lo_sizelimit: 0,
            lo_number: 0,
            lo_encrypt_type: 0,
            lo_encrypt_key_size: 0,
            lo_flags: 0,
            lo_file_name: [0; LO_NAME_SIZE],
            lo_crypt_name: [0; LO_NAME_SIZE],
            lo_encrypt_key: [0; LO_KEY_SIZE],
            lo_init: [0; 2],
        }
    }
}

/// Linux `struct loop_config` 的 ABI 布局。
#[repr(C)]
#[derive(Clone, Copy)]
struct LinuxLoopConfig {
    fd: u32,
    block_size: u32,
    info: LinuxLoopInfo64,
    reserved: [u64; 8],
}

struct LoopControlEndpoint;

struct LoopControlInodeOps;

struct LoopControlFileOps;

struct VfsLoopBacking {
    file: Arc<File>,
}

impl LoopBacking for VfsLoopBacking {
    fn len(&self) -> Result<u64, LoopBackingError> {
        let stat = self.file.stat().map_err(map_vfs_loop_backing_error)?;
        u64::try_from(stat.size).map_err(|_| LoopBackingError::Invalid)
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, LoopBackingError> {
        self.file
            .read_at(buf, offset)
            .map_err(map_vfs_loop_backing_error)
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<usize, LoopBackingError> {
        self.file
            .write_at(buf, offset)
            .map_err(map_vfs_loop_backing_error)
    }

    fn sync(&self) -> Result<(), LoopBackingError> {
        self.file.sync().map_err(map_vfs_loop_backing_error)
    }
}

struct LoopEntry {
    bundle: LoopDeviceBundle,
    function: Arc<dyn DeviceFunction>,
}

impl LoopEntry {
    fn new(index: u32) -> Result<Arc<Self>, Errno> {
        let bundle = LoopDeviceBundle::new(index).map_err(map_loop_errno)?;
        let function: Arc<dyn DeviceFunction> = Arc::new(BlockFunction::with_devnode(
            bundle.name(),
            bundle.name(),
            bundle.block(),
        ));
        Ok(Arc::new(Self { bundle, function }))
    }

    fn driver(&self) -> Arc<LoopDriver> {
        self.bundle.driver()
    }
}

struct LoopRegistry {
    entries: Vec<Option<Arc<LoopEntry>>>,
}

impl LoopRegistry {
    const fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    fn get(&self, index: u32) -> Option<Arc<LoopEntry>> {
        self.entries
            .get(index as usize)
            .and_then(|entry| entry.as_ref().map(Arc::clone))
    }
}

static LOOP_REGISTRY: Spinlock<LoopRegistry> = Spinlock::new(LoopRegistry::new());

/// 注册 loop custom devnode 适配器。
pub fn register_devtmpfs_adapter() -> VfsResult<DevTmpfsCustomNodeAdapterRegistration> {
    register_custom_devnode_adapter(DevTmpfsCustomNodeAdapter::new(
        LOOP_DEVNODE_OWNER,
        LOOP_ADAPTER_NAME,
        build_loop_control_inode_ops,
    ))
}

/// 注册 `/dev/loop-control` 静态节点。
pub fn register_control_node() -> VfsResult<DevTmpfsStaticNodeRegistration> {
    register_static_dev_node(DevTmpfsStaticNode::new(
        LOOP_DEVNODE_OWNER,
        LOOP_CONTROL_NODE_NAME,
        build_loop_control_node,
    ))
}

fn build_loop_control_node() -> DevNodeSpec {
    let payload: Arc<dyn Any + Send + Sync> = Arc::new(LoopControlEndpoint);
    DevNodeSpec::custom(CustomDevNodeSpec::new(
        LOOP_CONTROL_NODE_NAME,
        CustomDevNodeKind::CharDevice,
        payload,
    ))
}

fn build_loop_control_inode_ops(
    spec: &CustomDevNodeSpec,
) -> VfsResult<Option<Arc<dyn InodeOps + Send + Sync>>> {
    let payload = spec.payload();
    if payload
        .as_ref()
        .downcast_ref::<LoopControlEndpoint>()
        .is_none()
    {
        return Ok(None);
    }
    Ok(Some(Arc::new(LoopControlInodeOps)))
}

impl InodeOps for LoopControlInodeOps {
    fn lookup(&self, _inode: &Inode, _name: &str) -> VfsResult<Arc<Inode>> {
        Err(VfsError::NotADirectory)
    }

    fn open(
        &self,
        _inode: &Inode,
        _opts: &OpenOptions,
        _cred: &vfs::cred::Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        Ok(Box::new(LoopControlFileOps))
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl FileOps for LoopControlFileOps {
    fn read_at(&self, _buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        Err(VfsError::NotSupported)
    }

    fn write_at(&self, _buf: &[u8], _offset: u64) -> VfsResult<usize> {
        Err(VfsError::NotSupported)
    }

    fn readdir(
        &self,
        _pos: u64,
        _sink: &mut dyn FnMut(DirEntry) -> ControlFlow<()>,
    ) -> VfsResult<u64> {
        Err(VfsError::NotADirectory)
    }

    fn sync(&self) -> VfsResult<()> {
        Ok(())
    }

    fn poll(&self, interest: PollEvents) -> PollEvents {
        interest.intersect(PollEvents::POLLIN.with(PollEvents::POLLOUT))
    }

    fn ioctl(&self, cmd: IoctlCmd, arg: usize) -> Result<usize, Errno> {
        match cmd.raw() {
            LOOP_CTL_GET_FREE => Ok(ensure_free_loop()?.bundle.index() as usize),
            LOOP_CTL_ADD => {
                let index = loop_index_arg(arg)?;
                add_loop(index)?;
                Ok(index as usize)
            }
            LOOP_CTL_REMOVE => {
                let index = loop_index_arg(arg)?;
                remove_loop(index)?;
                Ok(0)
            }
            _ => Err(Errno::ENOTTY),
        }
    }

    fn release(&self) {}

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// 尝试处理 `/dev/loopN` 的 loop 专用 ioctl。
///
/// 返回 `None` 表示该命令不是 loop ABI，调用方应继续执行通用块设备 ioctl。
pub fn try_loop_block_ioctl(
    dev: &Arc<BlockDevice>,
    cmd: IoctlCmd,
    arg: usize,
) -> Option<Result<usize, Errno>> {
    if !is_loop_ioctl(cmd.raw()) {
        return None;
    }
    let Some(driver) = dev.downcast_driver::<LoopDriver>() else {
        return Some(Err(Errno::ENOTTY));
    };
    Some(handle_loop_block_ioctl(driver, cmd.raw(), arg))
}

fn handle_loop_block_ioctl(driver: &LoopDriver, raw: usize, arg: usize) -> Result<usize, Errno> {
    match raw {
        LOOP_SET_FD => {
            let options = attach_options_from_fd(arg, LinuxLoopInfo64::zeroed())?;
            driver.attach(options).map_err(map_loop_errno)?;
            Ok(0)
        }
        LOOP_CLR_FD => {
            driver.detach().map_err(map_loop_errno)?;
            Ok(0)
        }
        LOOP_GET_STATUS64 => {
            let status = driver.status();
            if !status.attached {
                return Err(ENXIO);
            }
            let info = linux_loop_info_from_status(&status);
            write_user_struct(arg, &info)?;
            Ok(0)
        }
        LOOP_SET_STATUS64 => {
            let info: LinuxLoopInfo64 = read_user_struct(arg)?;
            reject_unsupported_crypto(&info)?;
            let flags = loop_flags_from_linux(info.lo_flags);
            let size_limit = nonzero_limit(info.lo_sizelimit);
            let file_name = name_from_field(&info.lo_file_name);
            driver
                .set_status(info.lo_offset, size_limit, file_name, flags)
                .map_err(map_loop_errno)?;
            Ok(0)
        }
        LOOP_SET_CAPACITY => {
            driver.resize_from_backing().map_err(map_loop_errno)?;
            Ok(0)
        }
        LOOP_SET_DIRECT_IO => {
            let status = driver.status();
            if !status.attached {
                return Err(ENXIO);
            }
            let mut flags = status.flags;
            flags.direct_io = arg != 0;
            driver
                .set_status(status.offset, status.size_limit, None, flags)
                .map_err(map_loop_errno)?;
            Ok(0)
        }
        LOOP_SET_BLOCK_SIZE => {
            if arg == crate::dev::loopdev::LOOP_LOGICAL_BLOCK_SIZE as usize {
                Ok(0)
            } else {
                Err(Errno::EINVAL)
            }
        }
        LOOP_CONFIGURE => {
            let config: LinuxLoopConfig = read_user_struct(arg)?;
            if config.block_size != 0
                && config.block_size != crate::dev::loopdev::LOOP_LOGICAL_BLOCK_SIZE
            {
                return Err(Errno::EINVAL);
            }
            reject_unsupported_crypto(&config.info)?;
            let mut options = attach_options_from_fd(config.fd as usize, config.info)?;
            if let Some(name) = name_from_field(&config.info.lo_file_name) {
                options.file_name = name;
            }
            driver.attach(options).map_err(map_loop_errno)?;
            Ok(0)
        }
        _ => Err(Errno::ENOTTY),
    }
}

fn ensure_free_loop() -> Result<Arc<LoopEntry>, Errno> {
    if let Some(existing) = {
        let registry = LOOP_REGISTRY.lock();
        registry.entries.iter().find_map(|entry| {
            let entry = entry.as_ref()?;
            (!entry.driver().is_attached()).then(|| Arc::clone(entry))
        })
    } {
        return Ok(existing);
    }

    let index = {
        let registry = LOOP_REGISTRY.lock();
        u32::try_from(registry.entries.len()).map_err(|_| Errno::ENOMEM)?
    };
    ensure_loop(index)
}

fn add_loop(index: u32) -> Result<(), Errno> {
    if LOOP_REGISTRY.lock().get(index).is_some() {
        return Err(Errno::EEXIST);
    }
    ensure_loop(index).map(|_| ())
}

fn ensure_loop(index: u32) -> Result<Arc<LoopEntry>, Errno> {
    if let Some(existing) = LOOP_REGISTRY.lock().get(index) {
        return Ok(existing);
    }

    let entry = LoopEntry::new(index)?;
    publish_entry(&entry)?;

    let mut registry = LOOP_REGISTRY.lock();
    if let Some(existing) = registry.get(index) {
        unpublish_entry(&entry);
        return Ok(existing);
    }
    let needed = index as usize + 1;
    if registry.entries.len() < needed {
        let extra = needed - registry.entries.len();
        registry
            .entries
            .try_reserve(extra)
            .map_err(|_| Errno::ENOMEM)?;
        registry.entries.resize_with(needed, || None);
    }
    registry.entries[index as usize] = Some(Arc::clone(&entry));
    Ok(entry)
}

fn remove_loop(index: u32) -> Result<(), Errno> {
    let entry = {
        let mut registry = LOOP_REGISTRY.lock();
        let Some(slot) = registry.entries.get_mut(index as usize) else {
            return Err(Errno::ENODEV);
        };
        let Some(entry) = slot.as_ref() else {
            return Err(Errno::ENODEV);
        };
        if entry.driver().is_attached() {
            return Err(Errno::EBUSY);
        }
        slot.take().ok_or(Errno::ENODEV)?
    };
    unpublish_entry(&entry);
    Ok(())
}

fn publish_entry(entry: &Arc<LoopEntry>) -> Result<(), Errno> {
    DEVICES
        .register_function(Arc::clone(&entry.function))
        .map_err(map_function_registry_errno)?;
    let Some(nodes) = entry.function.devnodes() else {
        DEVICES.unregister_function(&entry.function);
        return Err(Errno::EINVAL);
    };
    if let Err(err) = bind_dynamic_devnodes(&nodes) {
        DEVICES.unregister_function(&entry.function);
        entry.function.mark_gone();
        return Err(err.to_errno());
    }
    Ok(())
}

fn unpublish_entry(entry: &Arc<LoopEntry>) {
    entry.function.mark_gone();
    entry.function.drain_io();
    if let Some(nodes) = entry.function.devnodes() {
        let _ = unbind_dynamic_devnodes(&nodes);
    }
    DEVICES.unregister_function(&entry.function);
}

fn attach_options_from_fd(
    fd_raw: usize,
    info: LinuxLoopInfo64,
) -> Result<LoopAttachOptions, Errno> {
    let fd = u32::try_from(fd_raw).map_err(|_| Errno::EBADF)?;
    let file = crate::vfs::current_fdtable()
        .and_then(|fdt| fdt.get_file(Fd::from_raw(fd)))
        .ok_or(Errno::EBADF)?;
    if file.inode().kind() != FileType::Regular {
        return Err(Errno::EINVAL);
    }
    let flags = file.flags();
    if !flags.readable() {
        return Err(Errno::EBADF);
    }
    let read_only = !flags.writable() || (info.lo_flags & LO_FLAGS_READ_ONLY) != 0;
    let file_name =
        name_from_field(&info.lo_file_name).unwrap_or_else(|| file_name_for_fd(fd, &file));
    let backing: Arc<dyn LoopBacking> = Arc::new(VfsLoopBacking { file });
    Ok(LoopAttachOptions {
        backing,
        file_name,
        read_only,
        offset: info.lo_offset,
        size_limit: nonzero_limit(info.lo_sizelimit),
        flags: loop_flags_from_linux(info.lo_flags),
    })
}

fn file_name_for_fd(fd: u32, file: &Arc<File>) -> Box<str> {
    if let Some(ctx) = crate::vfs::current_vfs_context()
        && let Some(path) = crate::vfs::namespace_path(&ctx, file.dentry(), file.mount())
    {
        return path.into_boxed_str();
    }
    let mut name = String::new();
    if name.try_reserve(16).is_ok() {
        name.push_str("fd:");
        let _ = write!(&mut name, "{}", fd);
    }
    name.into_boxed_str()
}

fn linux_loop_info_from_status(status: &LoopStatus) -> LinuxLoopInfo64 {
    let mut info = LinuxLoopInfo64::zeroed();
    info.lo_offset = status.offset;
    info.lo_sizelimit = status.size_limit.unwrap_or(0);
    info.lo_number = status.index;
    info.lo_flags = linux_flags_from_status(status);
    copy_name_to_field(&mut info.lo_file_name, &status.file_name);
    info
}

fn linux_flags_from_status(status: &LoopStatus) -> u32 {
    let mut flags = 0;
    if status.read_only {
        flags |= LO_FLAGS_READ_ONLY;
    }
    if status.flags.autoclear {
        flags |= LO_FLAGS_AUTOCLEAR;
    }
    if status.flags.partscan {
        flags |= LO_FLAGS_PARTSCAN;
    }
    if status.flags.direct_io {
        flags |= LO_FLAGS_DIRECT_IO;
    }
    flags
}

fn loop_flags_from_linux(flags: u32) -> LoopFlags {
    LoopFlags {
        autoclear: (flags & LO_FLAGS_AUTOCLEAR) != 0,
        partscan: (flags & LO_FLAGS_PARTSCAN) != 0,
        direct_io: (flags & LO_FLAGS_DIRECT_IO) != 0,
    }
}

fn nonzero_limit(value: u64) -> Option<u64> {
    (value != 0).then_some(value)
}

fn reject_unsupported_crypto(info: &LinuxLoopInfo64) -> Result<(), Errno> {
    if info.lo_encrypt_type != 0 || info.lo_encrypt_key_size != 0 {
        return Err(Errno::EINVAL);
    }
    Ok(())
}

fn name_from_field(field: &[u8; LO_NAME_SIZE]) -> Option<Box<str>> {
    let end = field
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(field.len());
    if end == 0 {
        return None;
    }
    let mut name = String::new();
    name.try_reserve(end).ok()?;
    for &byte in &field[..end] {
        if byte.is_ascii_graphic() || byte == b' ' || byte == b'/' {
            name.push(byte as char);
        } else {
            name.push('?');
        }
    }
    Some(name.into_boxed_str())
}

fn copy_name_to_field(field: &mut [u8; LO_NAME_SIZE], name: &str) {
    let bytes = name.as_bytes();
    let n = bytes.len().min(field.len().saturating_sub(1));
    field[..n].copy_from_slice(&bytes[..n]);
}

fn read_user_struct<T: Copy>(user: usize) -> Result<T, Errno> {
    if user == 0 {
        return Err(Errno::EFAULT);
    }
    let mut value = MaybeUninit::<T>::zeroed();
    // Safety: `value` 指向内核栈上未初始化对象，按字节填满后再 assume_init；
    // T 只用于 repr(C) ABI POD 结构，调用点限制为 Copy 类型。
    let bytes =
        unsafe { core::slice::from_raw_parts_mut(value.as_mut_ptr().cast::<u8>(), size_of::<T>()) };
    copy_from_user(user, bytes).map_err(|err| err.as_errno())?;
    // Safety: 上面的 copy_from_user 已经覆盖了整个对象字节范围。
    Ok(unsafe { value.assume_init() })
}

fn write_user_struct<T: Copy>(user: usize, value: &T) -> Result<(), Errno> {
    if user == 0 {
        return Err(Errno::EFAULT);
    }
    // Safety: 只把 repr(C) POD 结构按字节复制到用户空间，不暴露 Rust 引用。
    let bytes =
        unsafe { core::slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) };
    copy_to_user(user, bytes).map_err(|err| err.as_errno())
}

fn is_loop_ioctl(raw: usize) -> bool {
    matches!(
        raw,
        LOOP_SET_FD
            | LOOP_CLR_FD
            | LOOP_SET_STATUS64
            | LOOP_GET_STATUS64
            | LOOP_SET_CAPACITY
            | LOOP_SET_DIRECT_IO
            | LOOP_SET_BLOCK_SIZE
            | LOOP_CONFIGURE
    )
}

fn loop_index_arg(arg: usize) -> Result<u32, Errno> {
    u32::try_from(arg).map_err(|_| Errno::EINVAL)
}

fn map_vfs_loop_backing_error(err: VfsError) -> LoopBackingError {
    match err {
        VfsError::NoDevice => LoopBackingError::NoDevice,
        VfsError::ReadOnlyFilesystem | VfsError::BadFileDescriptor => LoopBackingError::ReadOnly,
        VfsError::InvalidArgument | VfsError::IllegalSeek => LoopBackingError::Invalid,
        VfsError::NotSupported => LoopBackingError::Unsupported,
        _ => LoopBackingError::Io,
    }
}

fn map_loop_errno(err: LoopError) -> Errno {
    match err {
        LoopError::AlreadyAttached | LoopError::Busy => Errno::EBUSY,
        LoopError::NotAttached => ENXIO,
        LoopError::Invalid => Errno::EINVAL,
        LoopError::OutOfMemory => Errno::ENOMEM,
        LoopError::Io => Errno::EIO,
        LoopError::NoDevice => Errno::ENODEV,
        LoopError::ReadOnly => Errno::EROFS,
        LoopError::Unsupported => Errno::EOPNOTSUPP,
    }
}

fn map_function_registry_errno(err: FunctionRegistryError) -> Errno {
    match err {
        FunctionRegistryError::NameExists => Errno::EEXIST,
        FunctionRegistryError::OutOfMemory => Errno::ENOMEM,
    }
}
