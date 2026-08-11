//! Native Directory/File 对象。
//!
//! Native 只把 Directory capability 作为路径解析起点；每次操作都显式提交相对
//! 路径或文件偏移，不读取任务的 cwd，也不把临时 VFS fd 泄漏到 Native handle table。

use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use general::mm::VmSpace;
use general::syscall::NativeCallOutcome;
use native_abi::wire::{DirectoryInfo, DirectoryRequest, FileInfo, PathRef};
use native_abi::{ObjectInterface, PAGE_SIZE, Rights, status, wire};
use sched::Task;
use sched::mutex::Mutex;
use vfs::dentry::Dentry;
use vfs::error::VfsError;
use vfs::file::{AccessMode, File, OpenOptions};
use vfs::inode::Inode;
use vfs::mount::Mount;
use vfs::operation;
use vfs::path::Dirfd;
use vfs::stat::{FileMode, FileType};
use vfs::{Arc as VfsArc, VfsContext};

use super::dispatch::native_return;
use super::operations::insert_native_handle;
use super::{
    KernelNativeObject, NativeProcessState, copy_user_bytes_in, copy_user_value,
    copy_user_value_out,
};

const MAX_NATIVE_PATH: usize = wire::MAX_PATH_BYTES as usize;

pub(crate) struct DirectoryObject {
    context: VfsArc<VfsContext>,
    dentry: VfsArc<Dentry>,
    mount: VfsArc<Mount>,
    generation: AtomicU64,
}

pub(crate) struct FileObject {
    pub(crate) file: VfsArc<File>,
    generation: AtomicU64,
    granted_rights: Rights,
}

struct FileMappingEntry {
    inode: Weak<Inode>,
    objects: Vec<Weak<super::memory::MemoryObject>>,
}

static FILE_MAPPINGS: Mutex<Vec<FileMappingEntry>> = Mutex::new(Vec::new());

pub(super) fn with_file_mapping_lock<T>(f: impl FnOnce() -> T) -> T {
    let _guard = FILE_MAPPINGS.lock();
    f()
}

impl DirectoryObject {
    pub(crate) fn from_context(context: &VfsContext) -> Arc<Self> {
        Arc::new(Self {
            context: VfsArc::new(context.fork().expect("Native 根目录 context 创建失败")),
            dentry: context.root.root(),
            mount: context.root.mount(),
            generation: AtomicU64::new(1),
        })
    }

    fn operation_context(&self) -> Result<VfsContext, VfsError> {
        let context = self.context.fork()?;
        context.set_root(Arc::clone(&self.dentry), Arc::clone(&self.mount))?;
        context.set_cwd(Arc::clone(&self.dentry), Arc::clone(&self.mount))?;
        Ok(context)
    }

    fn bump(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
    }
}

impl FileObject {
    fn info(&self) -> Result<FileInfo, VfsError> {
        let stat = self.file.stat()?;
        Ok(FileInfo {
            kind: file_kind(stat.mode),
            flags: 0,
            size: stat.size.max(0) as u64,
            generation: self.generation.load(Ordering::Acquire),
            modified_ns: 0,
            granted_rights: self.granted_rights.bits(),
            reserved: [0; 3],
        })
    }
}

pub(super) fn directory_open(
    task: &Arc<Task>,
    state: &NativeProcessState,
    directory: &DirectoryObject,
    user: u64,
) -> NativeCallOutcome {
    let request = match copy_user_value::<DirectoryRequest>(task, user) {
        Ok(request) => request,
        Err(error) => return native_return(error, 0, 0),
    };
    open_or_create(task, state, directory, &request, false)
}

pub(super) fn directory_create(
    task: &Arc<Task>,
    state: &NativeProcessState,
    directory: &DirectoryObject,
    user: u64,
) -> NativeCallOutcome {
    let request = match copy_user_value::<DirectoryRequest>(task, user) {
        Ok(request) => request,
        Err(error) => return native_return(error, 0, 0),
    };
    open_or_create(task, state, directory, &request, true)
}

pub(super) fn directory_remove(
    task: &Arc<Task>,
    directory: &DirectoryObject,
    user: u64,
    flags: u64,
) -> NativeCallOutcome {
    if flags & !(wire::DIRECTORY_REMOVE_DIRECTORY as u64) != 0 {
        return native_return(status::CORE_INVALID_ARGUMENT, 0, 0);
    }
    let path = match copy_path(task, user) {
        Ok(path) => path,
        Err(error) => return native_return(error, 0, 0),
    };
    let context = match directory.operation_context() {
        Ok(context) => context,
        Err(error) => return map_fs_error(error),
    };
    let result = if flags == wire::DIRECTORY_REMOVE_DIRECTORY as u64 {
        operation::rmdir(&context, &Dirfd::Cwd, &path)
    } else {
        operation::unlink(&context, &Dirfd::Cwd, &path)
    };
    match result {
        Ok(()) => {
            directory.bump();
            native_return(status::OK, 0, 0)
        }
        Err(error) => map_fs_error(error),
    }
}

pub(super) fn directory_query(
    task: &Arc<Task>,
    directory: &DirectoryObject,
    user: u64,
) -> NativeCallOutcome {
    let Some(inode) = directory.dentry.inode() else {
        return native_return(status::FILESYSTEM_NOT_FOUND, 0, 0);
    };
    if inode.kind() != FileType::Directory {
        return native_return(status::FILESYSTEM_NOT_DIRECTORY, 0, 0);
    }
    let info = DirectoryInfo {
        flags: 0,
        reserved0: 0,
        generation: directory.generation.load(Ordering::Acquire),
        entry_count: 0,
        change_counter: directory.generation.load(Ordering::Acquire),
        reserved: [0; 4],
    };
    match copy_user_value_out(task, user, &info) {
        Ok(()) => native_return(status::OK, 0, 0),
        Err(error) => native_return(error, 0, 0),
    }
}

pub(super) fn file_read(
    task: &Arc<Task>,
    file: &FileObject,
    user: u64,
    length: u64,
    offset: u64,
    flags: u64,
) -> NativeCallOutcome {
    if let Err(error) = validate_file_io_flags(flags) {
        return native_return(error, 0, 0);
    }
    transfer_file(task, &file.file, user, length, offset, false)
}

pub(super) fn file_write(
    task: &Arc<Task>,
    file: &FileObject,
    user: u64,
    length: u64,
    offset: u64,
    flags: u64,
) -> NativeCallOutcome {
    if let Err(error) = validate_file_io_flags(flags) {
        return native_return(error, 0, 0);
    }
    transfer_file(task, &file.file, user, length, offset, true)
}

pub(super) fn validate_file_io_flags(flags: u64) -> Result<(), u32> {
    (flags == 0)
        .then_some(())
        .ok_or(status::CORE_INVALID_ARGUMENT)
}

pub(super) fn file_read_memory(
    file: &FileObject,
    memory: &Arc<super::memory::MemoryObject>,
    memory_offset: u64,
    length: u64,
    file_offset: u64,
) -> NativeCallOutcome {
    let Ok(length) = usize::try_from(length) else {
        return native_return(status::CORE_OUT_OF_RANGE, 0, 0);
    };
    if length == 0 {
        return native_return(status::OK, 0, 0);
    }
    let mut buffer = alloc::vec::Vec::new();
    if buffer.try_reserve_exact(length).is_err() {
        return native_return(status::CORE_RESOURCE_EXHAUSTED, 0, 0);
    }
    buffer.resize(length, 0);
    file_read_memory_buffered(file, memory, memory_offset, file_offset, &mut buffer)
}

pub(super) fn file_read_memory_buffered(
    file: &FileObject,
    memory: &Arc<super::memory::MemoryObject>,
    memory_offset: u64,
    file_offset: u64,
    buffer: &mut [u8],
) -> NativeCallOutcome {
    let count = match file.file.read_at(buffer, file_offset) {
        Ok(count) => count,
        Err(error) => return map_fs_error(error),
    };
    if count > buffer.len() {
        return native_return(status::FILESYSTEM_ERROR, 0, 0);
    }
    if let Err(error) = memory.write_from(memory_offset, &buffer[..count]) {
        return native_return(error, 0, 0);
    }
    native_return(status::OK, count as u64, 0)
}

pub(super) fn file_write_memory(
    file: &FileObject,
    memory: &Arc<super::memory::MemoryObject>,
    memory_offset: u64,
    length: u64,
    file_offset: u64,
) -> NativeCallOutcome {
    let Ok(length) = usize::try_from(length) else {
        return native_return(status::CORE_OUT_OF_RANGE, 0, 0);
    };
    if length == 0 {
        return native_return(status::OK, 0, 0);
    }
    let mut buffer = alloc::vec::Vec::new();
    if buffer.try_reserve_exact(length).is_err() {
        return native_return(status::CORE_RESOURCE_EXHAUSTED, 0, 0);
    }
    buffer.resize(length, 0);
    file_write_memory_buffered(file, memory, memory_offset, file_offset, &mut buffer)
}

pub(super) fn file_write_memory_buffered(
    file: &FileObject,
    memory: &Arc<super::memory::MemoryObject>,
    memory_offset: u64,
    file_offset: u64,
    buffer: &mut [u8],
) -> NativeCallOutcome {
    if let Err(error) = memory.read_into(memory_offset, buffer) {
        return native_return(error, 0, 0);
    }
    match file.file.write_at(&buffer, file_offset) {
        Ok(count) if count <= buffer.len() => native_return(status::OK, count as u64, 0),
        Ok(_) => native_return(status::FILESYSTEM_ERROR, 0, 0),
        Err(error) => map_fs_error(error),
    }
}

pub(super) fn file_resize(file: &FileObject, length: u64) -> NativeCallOutcome {
    let mut registry = FILE_MAPPINGS.lock();
    match file.file.truncate(length) {
        Ok(()) => {
            file.generation.fetch_add(1, Ordering::AcqRel);
            invalidate_file_mappings(&mut registry, file.file.inode(), length);
            native_return(status::OK, 0, 0)
        }
        Err(error) => map_fs_error(error),
    }
}

pub(super) fn file_query(task: &Arc<Task>, file: &FileObject, user: u64) -> NativeCallOutcome {
    let info = match file.info() {
        Ok(info) => info,
        Err(error) => return map_fs_error(error),
    };
    match copy_user_value_out(task, user, &info) {
        Ok(()) => native_return(status::OK, 0, 0),
        Err(error) => native_return(error, 0, 0),
    }
}

pub(super) fn file_map(
    _task: &Arc<Task>,
    state: &NativeProcessState,
    file: &FileObject,
    handle_rights: Rights,
    offset: u64,
    length: u64,
    permissions: u64,
) -> NativeCallOutcome {
    let permissions = permissions as u32;
    if offset % PAGE_SIZE != 0
        || length == 0
        || permissions == 0
        || permissions
            & !(wire::MEMORY_PERMISSION_READ
                | wire::MEMORY_PERMISSION_WRITE
                | wire::MEMORY_PERMISSION_EXECUTE)
            != 0
        || permissions & (wire::MEMORY_PERMISSION_WRITE | wire::MEMORY_PERMISSION_EXECUTE)
            == (wire::MEMORY_PERMISSION_WRITE | wire::MEMORY_PERMISSION_EXECUTE)
    {
        return native_return(status::CORE_INVALID_ARGUMENT, 0, 0);
    }
    let mut required_rights = Rights::MAP;
    if permissions & wire::MEMORY_PERMISSION_READ != 0 {
        required_rights |= Rights::READ;
    }
    if permissions & wire::MEMORY_PERMISSION_WRITE != 0 {
        required_rights |= Rights::WRITE;
    }
    if permissions & wire::MEMORY_PERMISSION_EXECUTE != 0 {
        required_rights |= Rights::EXECUTE;
    }
    if !required_rights.is_subset_of(handle_rights) {
        return native_return(status::SECURITY_RIGHTS_DENIED, 0, 0);
    }
    let Some(size) = length
        .checked_add(PAGE_SIZE - 1)
        .map(|value| value / PAGE_SIZE * PAGE_SIZE)
    else {
        return native_return(status::MEMORY_INVALID_RANGE, 0, 0);
    };
    let mut registry = FILE_MAPPINGS.lock();
    let Ok(file_size) = file.file.stat().map(|stat| stat.size.max(0) as u64) else {
        return native_return(status::FILESYSTEM_ERROR, 0, 0);
    };
    let Some(end) = offset.checked_add(size) else {
        return native_return(status::MEMORY_INVALID_RANGE, 0, 0);
    };
    if end > file_size {
        return native_return(status::MEMORY_INVALID_RANGE, 0, 0);
    }
    let backing: Arc<dyn mm::FileLike> = file.file.clone();
    let object = Arc::new(super::memory::MemoryObject::file(
        size,
        PAGE_SIZE,
        permissions,
        backing,
        offset,
        file_size,
    ));
    if let Err(error) = register_file_mapping(&mut registry, file.file.inode(), &object) {
        return native_return(error, 0, 0);
    }
    drop(registry);
    let mut granted = Rights::MAP | Rights::INSPECT | Rights::DUPLICATE;
    if permissions & wire::MEMORY_PERMISSION_READ != 0 {
        granted |= Rights::READ;
    }
    if permissions & wire::MEMORY_PERMISSION_WRITE != 0 {
        granted |= Rights::WRITE;
    }
    if permissions & wire::MEMORY_PERMISSION_EXECUTE != 0 {
        granted |= Rights::EXECUTE;
    }
    if Rights::WRITE.is_subset_of(handle_rights) || Rights::RESIZE.is_subset_of(handle_rights) {
        granted |= Rights::MODIFY;
    }
    insert_native_handle(
        state,
        KernelNativeObject::MemoryObject(object),
        ObjectInterface::MemoryObject,
        granted,
    )
}

fn open_or_create(
    task: &Arc<Task>,
    state: &NativeProcessState,
    directory: &DirectoryObject,
    request: &DirectoryRequest,
    create: bool,
) -> NativeCallOutcome {
    if request.reserved != [0; 4]
        || request.path.flags != 0
        || !matches!(
            request.kind,
            wire::DIRECTORY_ENTRY_FILE | wire::DIRECTORY_ENTRY_DIRECTORY
        )
    {
        return native_return(status::CORE_INVALID_ARGUMENT, 0, 0);
    }
    let requested = Rights::from_bits(request.requested_rights);
    let defaults = if request.kind == wire::DIRECTORY_ENTRY_DIRECTORY {
        Rights::OPEN | Rights::INSPECT | Rights::DUPLICATE
    } else {
        Rights::READ | Rights::INSPECT | Rights::DUPLICATE
    };
    let granted = if requested.bits() == 0 {
        defaults
    } else {
        requested
    };
    let allowed = if request.kind == wire::DIRECTORY_ENTRY_DIRECTORY {
        Rights::OPEN | Rights::INSPECT | Rights::DUPLICATE
    } else {
        Rights::READ
            | Rights::WRITE
            | Rights::RESIZE
            | Rights::MAP
            | Rights::INSPECT
            | Rights::DUPLICATE
    };
    if !granted.is_subset_of(allowed) {
        return native_return(status::SECURITY_RIGHTS_DENIED, 0, 0);
    }
    let path = match copy_path_ref(task, &request.path) {
        Ok(path) => path,
        Err(error) => return native_return(error, 0, 0),
    };
    let context = match directory.operation_context() {
        Ok(context) => context,
        Err(error) => return map_fs_error(error),
    };
    if create && request.kind == wire::DIRECTORY_ENTRY_DIRECTORY {
        if let Err(error) = operation::mkdirat(&context, &Dirfd::Cwd, &path, FileMode::new(0o777)) {
            return map_fs_error(error);
        }
    }
    let access = file_access_mode(granted);
    let options = OpenOptions {
        access,
        create: create && request.kind == wire::DIRECTORY_ENTRY_FILE,
        exclusive: create,
        directory: request.kind == wire::DIRECTORY_ENTRY_DIRECTORY,
        path_only: request.kind == wire::DIRECTORY_ENTRY_DIRECTORY,
        ..OpenOptions::default()
    };
    let table = vfs::fdtable::FdTable::new(&context.limits);
    let fd = match operation::openat(
        &context,
        &table,
        &Dirfd::Cwd,
        &path,
        options,
        FileMode::new(0o666),
    ) {
        Ok(fd) => fd,
        Err(error) => return map_fs_error(error),
    };
    let Some(file) = table.get_file(fd) else {
        return native_return(status::FILESYSTEM_ERROR, 0, 0);
    };
    let _ = table.close_fd(fd);
    if request.kind == wire::DIRECTORY_ENTRY_DIRECTORY {
        let object = Arc::new(DirectoryObject {
            context: directory.context.clone(),
            dentry: file.dentry().clone(),
            mount: file.mount().clone(),
            generation: AtomicU64::new(1),
        });
        insert_native_handle(
            state,
            KernelNativeObject::Directory(object),
            ObjectInterface::Directory,
            granted,
        )
    } else {
        insert_native_handle(
            state,
            KernelNativeObject::File(Arc::new(FileObject {
                file,
                generation: AtomicU64::new(1),
                granted_rights: granted,
            })),
            ObjectInterface::File,
            granted,
        )
    }
}

fn transfer_file(
    task: &Arc<Task>,
    file: &File,
    user: u64,
    length: u64,
    offset: u64,
    write: bool,
) -> NativeCallOutcome {
    let Ok(user) = usize::try_from(user) else {
        return native_return(status::STREAM_FAULT, 0, 0);
    };
    let Ok(length) = usize::try_from(length) else {
        return native_return(status::CORE_OUT_OF_RANGE, 0, 0);
    };
    if length == 0 {
        return native_return(status::OK, 0, 0);
    }
    let Some(end) = user.checked_add(length) else {
        return native_return(status::STREAM_FAULT, 0, 0);
    };
    let Some(vm) = task
        .ext_lookup(sched::TASKEXT_VM_SPACE)
        .and_then(|payload| payload.downcast::<VmSpace>().ok())
    else {
        return native_return(status::STREAM_FAULT, 0, 0);
    };
    let result = if write {
        unsafe { vm.with_user_read_slice(user, end - user, |buffer| file.write_at(buffer, offset)) }
    } else {
        unsafe { vm.with_user_write_slice(user, end - user, |buffer| file.read_at(buffer, offset)) }
    };
    match result {
        Ok(Ok(count)) => native_return(status::OK, count as u64, 0),
        Ok(Err(error)) => map_fs_error(error),
        Err(_) => native_return(status::STREAM_FAULT, 0, 0),
    }
}

fn copy_path(task: &Arc<Task>, user: u64) -> Result<String, u32> {
    let reference = copy_user_value::<PathRef>(task, user)?;
    copy_path_ref(task, &reference)
}

fn copy_path_ref(task: &Arc<Task>, reference: &PathRef) -> Result<String, u32> {
    if reference.length == 0 || reference.length as usize > MAX_NATIVE_PATH {
        return Err(status::CORE_OUT_OF_RANGE);
    }
    let length = reference.length as usize;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length)
        .map_err(|_| status::CORE_RESOURCE_EXHAUSTED)?;
    bytes.resize(length, 0);
    copy_user_bytes_in(task, reference.ptr, &mut bytes)?;
    if bytes.iter().any(|byte| *byte == 0) || bytes.first() == Some(&b'/') {
        return Err(status::FILESYSTEM_INVALID_PATH);
    }
    String::from_utf8(bytes).map_err(|_| status::FILESYSTEM_INVALID_PATH)
}

pub(super) fn file_access_mode(rights: Rights) -> AccessMode {
    let readable = Rights::READ.is_subset_of(rights);
    let writable = Rights::WRITE.is_subset_of(rights) || Rights::RESIZE.is_subset_of(rights);
    match (readable, writable) {
        (true, true) => AccessMode::ReadWrite,
        (false, true) => AccessMode::WriteOnly,
        _ => AccessMode::ReadOnly,
    }
}

fn register_file_mapping(
    registry: &mut Vec<FileMappingEntry>,
    inode: &Arc<Inode>,
    object: &Arc<super::memory::MemoryObject>,
) -> Result<(), u32> {
    registry.retain(|entry| entry.inode.strong_count() != 0);
    if let Some(entry) = registry.iter_mut().find(|entry| {
        entry
            .inode
            .upgrade()
            .is_some_and(|candidate| Arc::ptr_eq(&candidate, inode))
    }) {
        entry.objects.retain(|object| object.strong_count() != 0);
        entry
            .objects
            .try_reserve(1)
            .map_err(|_| status::CORE_RESOURCE_EXHAUSTED)?;
        entry.objects.push(Arc::downgrade(object));
        return Ok(());
    }

    let mut objects = Vec::new();
    objects
        .try_reserve_exact(1)
        .map_err(|_| status::CORE_RESOURCE_EXHAUSTED)?;
    registry
        .try_reserve(1)
        .map_err(|_| status::CORE_RESOURCE_EXHAUSTED)?;
    objects.push(Arc::downgrade(object));
    registry.push(FileMappingEntry {
        inode: Arc::downgrade(inode),
        objects,
    });
    Ok(())
}

fn invalidate_file_mappings(registry: &mut Vec<FileMappingEntry>, inode: &Arc<Inode>, length: u64) {
    registry.retain_mut(|entry| {
        let Some(candidate) = entry.inode.upgrade() else {
            return false;
        };
        if Arc::ptr_eq(&candidate, inode) {
            entry.objects.retain(|object| {
                let Some(object) = object.upgrade() else {
                    return false;
                };
                object.invalidate_file_mappings_after_resize(length);
                true
            });
        }
        !entry.objects.is_empty()
    });
}

fn file_kind(mode: u32) -> u32 {
    match mode & 0o170000 {
        0o040000 => wire::DIRECTORY_ENTRY_DIRECTORY,
        _ => wire::DIRECTORY_ENTRY_FILE,
    }
}

fn map_fs_error(error: VfsError) -> NativeCallOutcome {
    let code = match error {
        VfsError::NotFound => status::FILESYSTEM_NOT_FOUND,
        VfsError::NotADirectory => status::FILESYSTEM_NOT_DIRECTORY,
        VfsError::IsADirectory => status::FILESYSTEM_IS_DIRECTORY,
        VfsError::AlreadyExists => status::FILESYSTEM_ALREADY_EXISTS,
        VfsError::DirectoryNotEmpty => status::FILESYSTEM_NOT_EMPTY,
        VfsError::ReadOnlyFilesystem => status::FILESYSTEM_READ_ONLY,
        VfsError::PermissionDenied | VfsError::OperationNotPermitted => {
            status::SECURITY_RIGHTS_DENIED
        }
        VfsError::WouldBlock => status::STREAM_WOULD_BLOCK,
        VfsError::BrokenPipe | VfsError::ConnectionReset => status::STREAM_CLOSED,
        _ => status::FILESYSTEM_ERROR,
    };
    native_return(code, 0, 0)
}
