//! 相对 Directory capability 的文件系统对象。

use core::ops::{BitOr, BitOrAssign};

use super::memory::{MemoryObject, MemoryObjectMarker, MemoryPermissions};
use super::{BorrowedHandle, OwnedHandle, Status, abi, mrt_call};

pub enum DirectoryObject {}
pub enum FileObject {}

/// Directory handle 可请求的权限集合。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DirectoryRights(u64);

impl DirectoryRights {
    pub const OPEN: Self = Self(abi::MYGO_RIGHT_open);
    pub const INSPECT: Self = Self(abi::MYGO_RIGHT_inspect);
    pub const DUPLICATE: Self = Self(abi::MYGO_RIGHT_duplicate);

    const fn bits(self) -> u64 {
        self.0
    }
}

impl BitOr for DirectoryRights {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for DirectoryRights {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

/// File handle 可请求的权限集合。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FileRights(u64);

impl FileRights {
    pub const READ: Self = Self(abi::MYGO_RIGHT_read);
    pub const WRITE: Self = Self(abi::MYGO_RIGHT_write);
    pub const RESIZE: Self = Self(abi::MYGO_RIGHT_resize);
    pub const MAP: Self = Self(abi::MYGO_RIGHT_map);
    pub const INSPECT: Self = Self(abi::MYGO_RIGHT_inspect);
    pub const DUPLICATE: Self = Self(abi::MYGO_RIGHT_duplicate);

    const fn bits(self) -> u64 {
        self.0
    }
}

impl BitOr for FileRights {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for FileRights {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

enum DirectoryHandle {
    Borrowed(BorrowedHandle<'static, DirectoryObject>),
    Owned(OwnedHandle<DirectoryObject>),
}

impl DirectoryHandle {
    fn raw(&self) -> u64 {
        match self {
            Self::Borrowed(handle) => (*handle).raw(),
            Self::Owned(handle) => handle.raw(),
        }
    }
}

/// Directory capability。路径解析始终相对此对象进行。
pub struct Directory {
    handle: DirectoryHandle,
}

impl Directory {
    /// 获取启动环境授予的根目录视图。
    pub fn root() -> Option<Self> {
        let raw = unsafe { super::mrt_initial_handle(abi::MYGO_REQUIREMENT_root_directory) };
        Some(Self {
            handle: DirectoryHandle::Borrowed(BorrowedHandle::from_raw(raw)?),
        })
    }

    pub(crate) fn raw(&self) -> u64 {
        self.handle.raw()
    }

    /// 打开相对路径指向的 File。
    pub fn open_file(&self, path: &[u8], requested_rights: FileRights) -> Result<File, Status> {
        let handle = self.open(
            path,
            abi::MYGO_DIRECTORY_ENTRY_FILE,
            requested_rights.bits(),
            false,
        )?;
        Ok(File {
            handle: OwnedHandle::new(handle)
                .ok_or(Status(abi::MYGO_STATUS_core_out_of_range))?,
        })
    }

    /// 打开相对路径指向的子目录。
    pub fn open_directory(
        &self,
        path: &[u8],
        requested_rights: DirectoryRights,
    ) -> Result<Directory, Status> {
        let handle = self.open(
            path,
            abi::MYGO_DIRECTORY_ENTRY_DIRECTORY,
            requested_rights.bits(),
            false,
        )?;
        Ok(Directory {
            handle: DirectoryHandle::Owned(
                OwnedHandle::new(handle)
                    .ok_or(Status(abi::MYGO_STATUS_core_out_of_range))?,
            ),
        })
    }

    /// 创建并打开相对路径指向的 File。
    pub fn create_file(&self, path: &[u8], requested_rights: FileRights) -> Result<File, Status> {
        let handle = self.open(
            path,
            abi::MYGO_DIRECTORY_ENTRY_FILE,
            requested_rights.bits(),
            true,
        )?;
        Ok(File {
            handle: OwnedHandle::new(handle)
                .ok_or(Status(abi::MYGO_STATUS_core_out_of_range))?,
        })
    }

    /// 创建并打开相对路径指向的子目录。
    pub fn create_directory(
        &self,
        path: &[u8],
        requested_rights: DirectoryRights,
    ) -> Result<Directory, Status> {
        let handle = self.open(
            path,
            abi::MYGO_DIRECTORY_ENTRY_DIRECTORY,
            requested_rights.bits(),
            true,
        )?;
        Ok(Directory {
            handle: DirectoryHandle::Owned(
                OwnedHandle::new(handle)
                    .ok_or(Status(abi::MYGO_STATUS_core_out_of_range))?,
            ),
        })
    }

    fn open(
        &self,
        path: &[u8],
        kind: u32,
        requested_rights: u64,
        create: bool,
    ) -> Result<u64, Status> {
        let (available, slot) = if create {
            (abi::MYGO_HAS_directory_create, abi::MYGO_SLOT_directory_create)
        } else {
            (abi::MYGO_HAS_directory_open, abi::MYGO_SLOT_directory_open)
        };
        if !available {
            return Err(Status(abi::MYGO_STATUS_abi_unsupported_operation));
        }
        let length = u32::try_from(path.len())
            .map_err(|_| Status(abi::MYGO_STATUS_core_out_of_range))?;
        let request = abi::MygoDirectoryRequest {
            path: abi::MygoPathRef {
                ptr: path.as_ptr() as usize as u64,
                length,
                flags: 0,
            },
            kind,
            flags: 0,
            requested_rights,
            reserved: [0; 4],
        };
        let result = unsafe {
            mrt_call(
                slot,
                self.raw(),
                &request as *const _ as usize as u64,
                0,
                0,
                0,
                0,
            )
        };
        if result.status == abi::MYGO_STATUS_ok {
            Ok(result.value0)
        } else {
            Err(Status(result.status))
        }
    }

    /// 删除相对路径指向的文件或空目录。
    pub fn remove(&self, path: &[u8], directory: bool) -> Result<(), Status> {
        if !abi::MYGO_HAS_directory_remove {
            return Err(Status(abi::MYGO_STATUS_abi_unsupported_operation));
        }
        let length = u32::try_from(path.len())
            .map_err(|_| Status(abi::MYGO_STATUS_core_out_of_range))?;
        let path = abi::MygoPathRef {
            ptr: path.as_ptr() as usize as u64,
            length,
            flags: 0,
        };
        let flags = if directory {
            u64::from(abi::MYGO_DIRECTORY_REMOVE_DIRECTORY)
        } else {
            0
        };
        let result = unsafe {
            mrt_call(
                abi::MYGO_SLOT_directory_remove,
                self.raw(),
                &path as *const _ as usize as u64,
                flags,
                0,
                0,
                0,
            )
        };
        if result.status == abi::MYGO_STATUS_ok {
            Ok(())
        } else {
            Err(Status(result.status))
        }
    }

    pub fn query(&self) -> Result<abi::MygoDirectoryInfo, Status> {
        if !abi::MYGO_HAS_directory_query {
            return Err(Status(abi::MYGO_STATUS_abi_unsupported_operation));
        }
        let mut info = abi::MygoDirectoryInfo::default();
        let result = unsafe {
            mrt_call(
                abi::MYGO_SLOT_directory_query,
                self.raw(),
                &mut info as *mut _ as usize as u64,
                0,
                0,
                0,
                0,
            )
        };
        if result.status == abi::MYGO_STATUS_ok {
            Ok(info)
        } else {
            Err(Status(result.status))
        }
    }
}

/// File capability；没有隐式游标，所有 I/O 都显式传递 offset。
pub struct File {
    pub(crate) handle: OwnedHandle<FileObject>,
}

impl File {
    pub(crate) fn raw(&self) -> u64 {
        self.handle.raw()
    }

    pub fn read_at(&self, buffer: &mut [u8], offset: u64) -> Result<usize, Status> {
        self.transfer(buffer.as_mut_ptr() as usize as u64, buffer.len(), offset, false)
    }

    pub fn write_at(&self, buffer: &[u8], offset: u64) -> Result<usize, Status> {
        self.transfer(buffer.as_ptr() as usize as u64, buffer.len(), offset, true)
    }

    fn transfer(&self, pointer: u64, length: usize, offset: u64, write: bool) -> Result<usize, Status> {
        let (available, slot) = if write {
            (abi::MYGO_HAS_file_write, abi::MYGO_SLOT_file_write)
        } else {
            (abi::MYGO_HAS_file_read, abi::MYGO_SLOT_file_read)
        };
        if !available {
            return Err(Status(abi::MYGO_STATUS_abi_unsupported_operation));
        }
        let length = u64::try_from(length)
            .map_err(|_| Status(abi::MYGO_STATUS_core_out_of_range))?;
        let result = unsafe { mrt_call(slot, self.raw(), pointer, length, offset, 0, 0) };
        if result.status != abi::MYGO_STATUS_ok {
            return Err(Status(result.status));
        }
        usize::try_from(result.value0).map_err(|_| Status(abi::MYGO_STATUS_core_out_of_range))
    }

    pub fn resize(&self, size: u64) -> Result<(), Status> {
        if !abi::MYGO_HAS_file_resize {
            return Err(Status(abi::MYGO_STATUS_abi_unsupported_operation));
        }
        let result = unsafe { mrt_call(abi::MYGO_SLOT_file_resize, self.raw(), size, 0, 0, 0, 0) };
        if result.status == abi::MYGO_STATUS_ok {
            Ok(())
        } else {
            Err(Status(result.status))
        }
    }

    pub fn query(&self) -> Result<abi::MygoFileInfo, Status> {
        if !abi::MYGO_HAS_file_query {
            return Err(Status(abi::MYGO_STATUS_abi_unsupported_operation));
        }
        let mut info = abi::MygoFileInfo::default();
        let result = unsafe {
            mrt_call(
                abi::MYGO_SLOT_file_query,
                self.raw(),
                &mut info as *mut _ as usize as u64,
                0,
                0,
                0,
                0,
            )
        };
        if result.status == abi::MYGO_STATUS_ok {
            Ok(info)
        } else {
            Err(Status(result.status))
        }
    }

    /// 创建 file-backed MemoryObject，之后仍需显式 memory.map。
    pub fn memory(
        &self,
        offset: u64,
        length: u64,
        permissions: MemoryPermissions,
    ) -> Result<MemoryObject, Status> {
        if !abi::MYGO_HAS_file_map {
            return Err(Status(abi::MYGO_STATUS_abi_unsupported_operation));
        }
        let result = unsafe {
            mrt_call(
                abi::MYGO_SLOT_file_map,
                self.raw(),
                offset,
                length,
                u64::from(permissions.bits()),
                0,
                0,
            )
        };
        if result.status != abi::MYGO_STATUS_ok {
            return Err(Status(result.status));
        }
        OwnedHandle::<MemoryObjectMarker>::new(result.value0)
            .map(|handle| MemoryObject { handle })
            .ok_or(Status(abi::MYGO_STATUS_core_out_of_range))
    }
}
