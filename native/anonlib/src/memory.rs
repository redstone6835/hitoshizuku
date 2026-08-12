//! MemoryObject 与显式地址空间映射。

use core::marker::PhantomData;

use super::{BorrowedHandle, OwnedHandle, Process, Status, abi, mrt_call};

pub enum AddressSpaceObject {}
pub enum MemoryObjectMarker {}

/// 当前进程的地址空间 capability。
pub struct AddressSpace {
    handle: BorrowedHandle<'static, AddressSpaceObject>,
}

impl AddressSpace {
    /// 获取启动环境授予的当前地址空间。
    pub fn current() -> Option<Self> {
        let raw = unsafe { super::mrt_initial_handle(abi::MYGO_REQUIREMENT_current_address_space) };
        Some(Self {
            handle: BorrowedHandle::from_raw(raw)?,
        })
    }

    pub(crate) fn raw(&self) -> u64 {
        self.handle.raw()
    }

    /// 撤销一段由 Native memory.map 建立的完整映射。
    pub fn unmap(&self, mapping: MappedRegion) -> Result<(), Status> {
        if !abi::MYGO_HAS_memory_unmap {
            return Err(Status(abi::MYGO_STATUS_abi_unsupported_operation));
        }
        let result = unsafe {
            mrt_call(
                abi::MYGO_SLOT_memory_unmap,
                self.raw(),
                mapping.address,
                mapping.length,
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
}

/// 创建 MemoryObject 的固定请求。
#[derive(Clone, Copy)]
pub struct MemoryCreate {
    raw: abi::MygoMemoryCreateRequest,
}

impl MemoryCreate {
    /// 创建匿名 MemoryObject；size 会由内核向上取整到页边界。
    pub const fn anonymous(size: u64, alignment: u64) -> Self {
        Self {
            raw: abi::MygoMemoryCreateRequest {
                size,
                alignment,
                flags: 0,
                kind: abi::MYGO_MEMORY_KIND_ANONYMOUS,
                source_handle: 0,
                source_offset: 0,
                reserved: [0; 3],
            },
        }
    }

    /// 让匿名对象的写入在多个映射间共享可见。
    pub const fn shared(mut self) -> Self {
        self.raw.flags |= abi::MYGO_MEMORY_FLAG_SHARED;
        self
    }

    pub(crate) const fn dma(
        size: u64,
        alignment: u64,
        device: u64,
        flags: u32,
    ) -> Self {
        Self {
            raw: abi::MygoMemoryCreateRequest {
                size,
                alignment,
                flags,
                kind: abi::MYGO_MEMORY_KIND_DMA,
                source_handle: device,
                source_offset: 0,
                reserved: [0; 3],
            },
        }
    }
}

/// MemoryObject 映射权限；W+X 组合由内核拒绝。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MemoryPermissions(u32);

impl MemoryPermissions {
    pub const READ: Self = Self(abi::MYGO_MEMORY_PERMISSION_READ);
    pub const READ_WRITE: Self = Self(
        abi::MYGO_MEMORY_PERMISSION_READ | abi::MYGO_MEMORY_PERMISSION_WRITE,
    );
    pub const READ_EXECUTE: Self = Self(
        abi::MYGO_MEMORY_PERMISSION_READ | abi::MYGO_MEMORY_PERMISSION_EXECUTE,
    );

    pub const fn bits(self) -> u32 {
        self.0
    }
}

/// 由当前进程拥有的 MemoryObject capability。
pub struct MemoryObject {
    pub(crate) handle: OwnedHandle<MemoryObjectMarker>,
}

impl Process {
    /// 创建一个显式 backing 的 MemoryObject。
    pub fn create_memory(&self, request: MemoryCreate) -> Result<MemoryObject, Status> {
        if !abi::MYGO_HAS_memory_create {
            return Err(Status(abi::MYGO_STATUS_abi_unsupported_operation));
        }
        let result = unsafe {
            mrt_call(
                abi::MYGO_SLOT_memory_create,
                self.raw(),
                &request.raw as *const _ as usize as u64,
                0,
                0,
                0,
                0,
            )
        };
        if result.status != abi::MYGO_STATUS_ok {
            return Err(Status(result.status));
        }
        OwnedHandle::new(result.value0)
            .map(|handle| MemoryObject { handle })
            .ok_or(Status(abi::MYGO_STATUS_core_out_of_range))
    }
}

impl MemoryObject {
    pub(crate) fn raw(&self) -> u64 {
        self.handle.raw()
    }

    /// 查询对象类型、大小、映射数和 generation。
    pub fn query(&self) -> Result<abi::MygoMemoryInfo, Status> {
        if !abi::MYGO_HAS_memory_query {
            return Err(Status(abi::MYGO_STATUS_abi_unsupported_operation));
        }
        let mut info = abi::MygoMemoryInfo::default();
        let result = unsafe {
            mrt_call(
                abi::MYGO_SLOT_memory_query,
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

    /// 查询对象当前的驻留、共享复用和数据面访问统计。
    pub fn statistics(&self) -> Result<abi::MygoMemoryStatistics, Status> {
        if !abi::MYGO_HAS_memory_statistics {
            return Err(Status(abi::MYGO_STATUS_abi_unsupported_operation));
        }
        let mut statistics = abi::MygoMemoryStatistics::default();
        let result = unsafe {
            mrt_call(
                abi::MYGO_SLOT_memory_statistics,
                self.raw(),
                &mut statistics as *mut _ as usize as u64,
                0,
                0,
                0,
                0,
            )
        };
        if result.status == abi::MYGO_STATUS_ok {
            Ok(statistics)
        } else {
            Err(Status(result.status))
        }
    }

    /// 撤销对象的全部进程内映射，并使既有 generation token 失效。
    pub fn revoke(&self) -> Result<usize, Status> {
        if !abi::MYGO_HAS_memory_revoke {
            return Err(Status(abi::MYGO_STATUS_abi_unsupported_operation));
        }
        let result = unsafe {
            mrt_call(
                abi::MYGO_SLOT_memory_revoke,
                self.raw(),
                0,
                0,
                0,
                0,
                0,
            )
        };
        if result.status != abi::MYGO_STATUS_ok {
            return Err(Status(result.status));
        }
        usize::try_from(result.value0).map_err(|_| Status(abi::MYGO_STATUS_core_out_of_range))
    }

    /// 在指定地址空间中建立非固定映射。
    pub fn map(
        &self,
        address_space: &AddressSpace,
        offset: u64,
        length: u64,
        permissions: MemoryPermissions,
    ) -> Result<MappedRegion, Status> {
        self.map_aligned(address_space, offset, length, abi::MYGO_PAGE_SIZE, 0, permissions)
    }

    /// 建立带 alignment 和可选地址提示的映射；地址选择仍由内核完成。
    pub fn map_aligned(
        &self,
        address_space: &AddressSpace,
        offset: u64,
        length: u64,
        alignment: u64,
        address_hint: u64,
        permissions: MemoryPermissions,
    ) -> Result<MappedRegion, Status> {
        if !abi::MYGO_HAS_memory_map {
            return Err(Status(abi::MYGO_STATUS_abi_unsupported_operation));
        }
        let request = abi::MygoMemoryMapRequest {
            address_space: address_space.raw(),
            offset,
            length,
            alignment,
            address_hint,
            permissions: permissions.bits(),
            flags: 0,
            reserved: [0; 2],
        };
        let result = unsafe {
            mrt_call(
                abi::MYGO_SLOT_memory_map,
                self.raw(),
                &request as *const _ as usize as u64,
                0,
                0,
                0,
                0,
            )
        };
        if result.status != abi::MYGO_STATUS_ok {
            return Err(Status(result.status));
        }
        Ok(MappedRegion {
            address: result.value0,
            length: result.value1,
        })
    }

    /// 生成 DeviceFunction 等对象使用的 generation-checked 区间。
    pub fn region(&self, offset: u64, length: u64) -> Result<MemoryRegion<'_>, Status> {
        let info = self.query()?;
        if info.state != abi::MYGO_MEMORY_STATE_ACTIVE {
            return Err(Status(if info.state == abi::MYGO_MEMORY_STATE_REVOKED {
                abi::MYGO_STATUS_memory_revoked
            } else {
                abi::MYGO_STATUS_memory_poisoned
            }));
        }
        if length == 0 || offset.checked_add(length).is_none_or(|end| end > info.size) {
            return Err(Status(abi::MYGO_STATUS_memory_invalid_range));
        }
        Ok(MemoryRegion {
            raw: abi::MygoMemoryRegion {
                memory: self.raw(),
                offset,
                length,
                generation: info.generation,
            },
            marker: PhantomData,
        })
    }
}

/// 已安装映射的精确范围；撤销必须显式调用 AddressSpace::unmap。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MappedRegion {
    address: u64,
    length: u64,
}

impl MappedRegion {
    pub const fn address(self) -> u64 {
        self.address
    }

    pub const fn length(self) -> u64 {
        self.length
    }
}

/// 带 MemoryObject generation 的稳定区间描述。
pub struct MemoryRegion<'a> {
    pub(crate) raw: abi::MygoMemoryRegion,
    marker: PhantomData<&'a MemoryObject>,
}

impl MemoryRegion<'_> {
    pub const fn length(&self) -> u64 {
        self.raw.length
    }
}
