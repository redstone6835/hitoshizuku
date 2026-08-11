//! 与内核对象类型无关的 Native capability handle 状态机。

use alloc::vec::Vec;

use crate::{ObjectInterface, Rights, status};

pub const MAX_NATIVE_HANDLE_SLOTS: u32 = 4096;

/// 用户可见 handle。编码中 generation 位于高 32 位，index 位于低 32 位。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeHandle {
    pub generation: u32,
    pub index: u32,
}

impl NativeHandle {
    pub const fn from_parts(generation: u32, index: u32) -> Self {
        Self { generation, index }
    }

    pub const fn from_raw(raw: u64) -> Self {
        Self {
            generation: (raw >> 32) as u32,
            index: raw as u32,
        }
    }

    pub const fn raw(self) -> u64 {
        ((self.generation as u64) << 32) | self.index as u64
    }
}

#[derive(Debug)]
pub enum HandleSlot<T> {
    Vacant {
        generation: u32,
    },
    Occupied {
        generation: u32,
        object: T,
        interface: ObjectInterface,
        rights: Rights,
    },
    Retired,
}

/// 一次成功 lookup 固定下来的对象引用与授权快照。
#[derive(Debug, PartialEq, Eq)]
pub struct NativeHandleRef<'a, T> {
    pub object: &'a T,
    pub interface: ObjectInterface,
    pub rights: Rights,
}

/// 进程级 capability handle 表。
///
/// 该类型只维护索引、代际、接口和权限，不依赖调度器、VFS 或具体内核对象。
pub struct NativeHandleTable<T> {
    slots: Vec<HandleSlot<T>>,
    free_indices: Vec<u32>,
    max_slots: u32,
}

impl<T> NativeHandleTable<T> {
    pub fn new() -> Result<Self, u32> {
        let mut slots = Vec::new();
        slots
            .try_reserve_exact(MAX_NATIVE_HANDLE_SLOTS as usize)
            .map_err(|_| status::CORE_RESOURCE_EXHAUSTED)?;
        let mut free_indices = Vec::new();
        free_indices
            .try_reserve_exact(MAX_NATIVE_HANDLE_SLOTS as usize)
            .map_err(|_| status::CORE_RESOURCE_EXHAUSTED)?;
        Ok(Self {
            slots,
            free_indices,
            max_slots: MAX_NATIVE_HANDLE_SLOTS,
        })
    }

    pub fn insert(
        &mut self,
        object: T,
        interface: ObjectInterface,
        rights: Rights,
    ) -> Result<NativeHandle, u32> {
        if let Some(index) = self.free_indices.pop() {
            let slot = &mut self.slots[(index - 1) as usize];
            let HandleSlot::Vacant { generation } = slot else {
                debug_assert!(false, "free handle index 未指向空闲槽");
                return Err(status::CORE_RESOURCE_EXHAUSTED);
            };
            let generation = *generation;
            *slot = HandleSlot::Occupied {
                generation,
                object,
                interface,
                rights,
            };
            return Ok(NativeHandle::from_parts(generation, index));
        }

        if self.slots.len() >= self.max_slots as usize {
            return Err(status::CORE_RESOURCE_EXHAUSTED);
        }
        let index = self.slots.len() as u32 + 1;
        self.slots.push(HandleSlot::Occupied {
            generation: 1,
            object,
            interface,
            rights,
        });
        Ok(NativeHandle::from_parts(1, index))
    }

    pub fn lookup(
        &self,
        handle: NativeHandle,
        expected_interface: Option<ObjectInterface>,
        required_rights: Rights,
    ) -> Result<NativeHandleRef<'_, T>, u32> {
        let slot = self.slot(handle)?;
        let HandleSlot::Occupied {
            object,
            interface,
            rights,
            ..
        } = slot
        else {
            return Err(status::HANDLE_INVALID);
        };
        if expected_interface.is_some_and(|expected| expected != *interface) {
            return Err(status::HANDLE_WRONG_INTERFACE);
        }
        if !required_rights.is_subset_of(*rights) {
            return Err(status::SECURITY_RIGHTS_DENIED);
        }
        Ok(NativeHandleRef {
            object,
            interface: *interface,
            rights: *rights,
        })
    }

    pub fn close(&mut self, handle: NativeHandle) -> Result<T, u32> {
        let slot_index = self.slot_index(handle)?;
        match &self.slots[slot_index] {
            HandleSlot::Retired => return Err(status::HANDLE_STALE),
            HandleSlot::Vacant { generation } => {
                return Err(if *generation == handle.generation {
                    status::HANDLE_INVALID
                } else {
                    status::HANDLE_STALE
                });
            }
            HandleSlot::Occupied { generation, .. } if *generation != handle.generation => {
                return Err(status::HANDLE_STALE);
            }
            HandleSlot::Occupied { .. } => {}
        }

        let old = core::mem::replace(&mut self.slots[slot_index], HandleSlot::Retired);
        let HandleSlot::Occupied {
            generation, object, ..
        } = old
        else {
            unreachable!();
        };
        if generation != u32::MAX {
            self.slots[slot_index] = HandleSlot::Vacant {
                generation: generation + 1,
            };
            self.free_indices.push(handle.index);
        }
        Ok(object)
    }

    fn slot(&self, handle: NativeHandle) -> Result<&HandleSlot<T>, u32> {
        let slot = &self.slots[self.slot_index(handle)?];
        match slot {
            HandleSlot::Retired => Err(status::HANDLE_STALE),
            HandleSlot::Vacant { generation } => Err(if *generation == handle.generation {
                status::HANDLE_INVALID
            } else {
                status::HANDLE_STALE
            }),
            HandleSlot::Occupied { generation, .. } if *generation != handle.generation => {
                Err(status::HANDLE_STALE)
            }
            HandleSlot::Occupied { .. } => Ok(slot),
        }
    }

    fn slot_index(&self, handle: NativeHandle) -> Result<usize, u32> {
        if handle.index == 0 || handle.generation == 0 {
            return Err(status::HANDLE_INVALID);
        }
        let index = (handle.index - 1) as usize;
        if index >= self.slots.len() {
            return Err(status::HANDLE_INVALID);
        }
        Ok(index)
    }
}

impl<T: Clone> NativeHandleTable<T> {
    pub fn duplicate(&mut self, source: NativeHandle) -> Result<NativeHandle, u32> {
        let entry = self.lookup(source, None, Rights::DUPLICATE)?;
        let object = entry.object.clone();
        let interface = entry.interface;
        let rights = entry.rights;
        self.insert(object, interface, rights)
    }

    pub fn restrict(
        &mut self,
        source: NativeHandle,
        requested_rights: Rights,
    ) -> Result<NativeHandle, u32> {
        let entry = self.lookup(source, None, Rights::DUPLICATE)?;
        if !requested_rights.is_subset_of(entry.rights) {
            return Err(status::SECURITY_RIGHTS_DENIED);
        }
        let object = entry.object.clone();
        let interface = entry.interface;
        self.insert(object, interface, requested_rights)
    }
}

#[cfg(test)]
mod tests {
    use super::{HandleSlot, NativeHandle, NativeHandleTable};
    use crate::{ObjectInterface, Rights, status};

    #[test]
    fn first_allocation_returns_a_live_nonzero_handle() {
        let mut table = NativeHandleTable::new().expect("handle table 应创建成功");

        let handle = table
            .insert("stdout", ObjectInterface::Stream, Rights::WRITE)
            .expect("首个 handle 应分配成功");
        let entry = table
            .lookup(handle, Some(ObjectInterface::Stream), Rights::WRITE)
            .expect("刚分配的 handle 应可查找");

        assert_eq!(handle, NativeHandle::from_parts(1, 1));
        assert_eq!(handle.raw(), 0x0000_0001_0000_0001);
        assert_eq!(*entry.object, "stdout");
        assert_eq!(entry.interface, ObjectInterface::Stream);
        assert_eq!(entry.rights, Rights::WRITE);
    }

    #[test]
    fn duplicate_and_restrict_create_independent_handles() {
        let rights = Rights::WRITE | Rights::DUPLICATE;
        let mut table = NativeHandleTable::new().expect("handle table 应创建成功");
        let source = table
            .insert("stdout", ObjectInterface::Stream, rights)
            .expect("源 handle 应分配成功");

        let duplicate = table.duplicate(source).expect("duplicate 应成功");
        let restricted = table
            .restrict(source, Rights::WRITE)
            .expect("权限子集应成功降权");

        assert_ne!(duplicate, source);
        assert_ne!(restricted, source);
        assert_ne!(restricted, duplicate);
        assert_eq!(
            table
                .lookup(duplicate, Some(ObjectInterface::Stream), rights)
                .expect("duplicate 应保留源权限")
                .rights,
            rights
        );
        assert_eq!(
            table
                .lookup(restricted, Some(ObjectInterface::Stream), Rights::WRITE)
                .expect("restricted handle 应具有请求的权限")
                .rights,
            Rights::WRITE
        );
        assert!(
            table
                .lookup(source, Some(ObjectInterface::Stream), rights)
                .is_ok(),
            "duplicate/restrict 不得修改源 handle"
        );
    }

    #[test]
    fn restrict_rejects_privilege_escalation_without_changing_source() {
        let mut table = NativeHandleTable::new().expect("handle table 应创建成功");
        let source = table
            .insert(
                "stdout",
                ObjectInterface::Stream,
                Rights::WRITE | Rights::DUPLICATE,
            )
            .expect("源 handle 应分配成功");

        assert_eq!(
            table.restrict(source, Rights::WRITE | Rights::READ),
            Err(status::SECURITY_RIGHTS_DENIED)
        );
        assert!(
            table
                .lookup(
                    source,
                    Some(ObjectInterface::Stream),
                    Rights::WRITE | Rights::DUPLICATE
                )
                .is_ok(),
            "失败的降权不得修改源 handle"
        );
    }

    #[test]
    fn duplicate_and_restrict_require_duplicate_right() {
        let mut table = NativeHandleTable::new().expect("handle table 应创建成功");
        let source = table
            .insert("stdin", ObjectInterface::Stream, Rights::READ)
            .expect("源 handle 应分配成功");

        assert_eq!(table.duplicate(source), Err(status::SECURITY_RIGHTS_DENIED));
        assert_eq!(
            table.restrict(source, Rights::READ),
            Err(status::SECURITY_RIGHTS_DENIED)
        );
        assert!(
            table
                .lookup(source, Some(ObjectInterface::Stream), Rights::READ)
                .is_ok(),
            "失败操作不得修改源 handle"
        );
    }

    #[test]
    fn close_makes_the_old_handle_stale_and_reuses_with_next_generation() {
        let mut table = NativeHandleTable::new().expect("handle table 应创建成功");
        let old = table
            .insert("first", ObjectInterface::Stream, Rights::READ)
            .expect("handle 应分配成功");

        assert_eq!(table.close(old), Ok("first"));
        assert_eq!(
            table.lookup(old, Some(ObjectInterface::Clock), Rights::WRITE),
            Err(status::HANDLE_STALE)
        );

        let replacement = table
            .insert("second", ObjectInterface::Stream, Rights::READ)
            .expect("空闲槽应可复用");
        assert_eq!(replacement.index, old.index);
        assert_eq!(replacement.generation, old.generation + 1);
        assert_eq!(
            table.lookup(old, Some(ObjectInterface::Stream), Rights::READ),
            Err(status::HANDLE_STALE)
        );
    }

    #[test]
    fn lookup_checks_interface_before_rights() {
        let mut table = NativeHandleTable::new().expect("handle table 应创建成功");
        let handle = table
            .insert("clock", ObjectInterface::Clock, Rights::READ)
            .expect("handle 应分配成功");

        assert_eq!(
            table.lookup(handle, Some(ObjectInterface::Stream), Rights::WRITE),
            Err(status::HANDLE_WRONG_INTERFACE)
        );
        assert_eq!(
            table.lookup(handle, Some(ObjectInterface::Clock), Rights::WRITE),
            Err(status::SECURITY_RIGHTS_DENIED)
        );
    }

    #[test]
    fn malformed_and_never_allocated_handles_are_invalid() {
        let table = NativeHandleTable::<&'static str>::new().expect("handle table 应创建成功");

        for handle in [
            NativeHandle::from_parts(0, 0),
            NativeHandle::from_parts(1, 0),
            NativeHandle::from_parts(0, 1),
            NativeHandle::from_parts(1, 1),
        ] {
            assert_eq!(
                table.lookup(handle, None, Rights::NONE),
                Err(status::HANDLE_INVALID)
            );
        }
    }

    #[test]
    fn table_never_issues_more_than_4096_slots() {
        let mut table = NativeHandleTable::new().expect("handle table 应创建成功");

        for value in 0..4096 {
            table
                .insert(value, ObjectInterface::Stream, Rights::READ)
                .expect("前 4096 个槽位应可分配");
        }

        assert_eq!(
            table.insert(4096, ObjectInterface::Stream, Rights::READ),
            Err(status::CORE_RESOURCE_EXHAUSTED)
        );
    }

    #[test]
    fn closing_max_generation_retires_the_slot_permanently() {
        let mut table = NativeHandleTable::<&'static str>::new().expect("handle table 应创建成功");
        table.slots.push(HandleSlot::Occupied {
            generation: u32::MAX,
            object: "last",
            interface: ObjectInterface::Stream,
            rights: Rights::READ,
        });
        let last = NativeHandle::from_parts(u32::MAX, 1);

        assert_eq!(table.close(last), Ok("last"));
        assert!(matches!(table.slots[0], HandleSlot::Retired));
        assert_eq!(
            table.lookup(last, Some(ObjectInterface::Stream), Rights::READ),
            Err(status::HANDLE_STALE)
        );

        let next = table
            .insert("next", ObjectInterface::Stream, Rights::READ)
            .expect("退役槽不得阻止新槽分配");
        assert_eq!(next, NativeHandle::from_parts(1, 2));
    }
}
