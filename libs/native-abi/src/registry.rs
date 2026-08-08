//! MyGO Native ABI epoch 1 的机器身份注册表。

use core::ops::{BitOr, BitOrAssign};

pub const ABI_FAMILY_MYGO_NATIVE: u16 = 1;
pub const ABI_EPOCH: u16 = 1;
pub const PAGE_SIZE: u64 = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum TargetArch {
    Riscv64 = 1,
    LoongArch64 = 2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum ObjectInterface {
    Process = 1,
    AddressSpace = 2,
    Stream = 3,
    Clock = 4,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rights(u64);

impl Rights {
    pub const NONE: Self = Self(0);
    pub const READ: Self = Self(1 << 0);
    pub const WRITE: Self = Self(1 << 1);
    pub const DUPLICATE: Self = Self(1 << 2);
    pub const ALLOCATE: Self = Self(1 << 3);
    pub const FREE: Self = Self(1 << 4);
    pub const EXIT: Self = Self(1 << 5);

    pub const fn from_bits(bits: u64) -> Self {
        Self(bits)
    }

    pub const fn bits(self) -> u64 {
        self.0
    }

    pub const fn is_subset_of(self, other: Self) -> bool {
        self.0 & !other.0 == 0
    }
}

impl BitOr for Rights {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for Rights {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RightSpec {
    pub name: &'static str,
    pub right: Rights,
}

pub const RIGHTS: [RightSpec; 6] = [
    RightSpec {
        name: "read",
        right: Rights::READ,
    },
    RightSpec {
        name: "write",
        right: Rights::WRITE,
    },
    RightSpec {
        name: "duplicate",
        right: Rights::DUPLICATE,
    },
    RightSpec {
        name: "allocate",
        right: Rights::ALLOCATE,
    },
    RightSpec {
        name: "free",
        right: Rights::FREE,
    },
    RightSpec {
        name: "exit",
        right: Rights::EXIT,
    },
];

pub fn right_by_name(name: &str) -> Option<RightSpec> {
    RIGHTS.iter().copied().find(|spec| spec.name == name)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum RequirementId {
    SelfProcess = 1,
    CurrentAddressSpace = 2,
    Stdin = 3,
    Stdout = 4,
    Stderr = 5,
    MonotonicClock = 6,
}

impl RequirementId {
    pub const fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            1 => Some(Self::SelfProcess),
            2 => Some(Self::CurrentAddressSpace),
            3 => Some(Self::Stdin),
            4 => Some(Self::Stdout),
            5 => Some(Self::Stderr),
            6 => Some(Self::MonotonicClock),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequirementSpec {
    pub id: RequirementId,
    pub name: &'static str,
    pub interface: ObjectInterface,
    pub max_rights: Rights,
}

pub const REQUIREMENTS: [RequirementSpec; 6] = [
    RequirementSpec {
        id: RequirementId::SelfProcess,
        name: "self_process",
        interface: ObjectInterface::Process,
        max_rights: Rights::EXIT,
    },
    RequirementSpec {
        id: RequirementId::CurrentAddressSpace,
        name: "current_address_space",
        interface: ObjectInterface::AddressSpace,
        max_rights: Rights::from_bits(Rights::ALLOCATE.bits() | Rights::FREE.bits()),
    },
    RequirementSpec {
        id: RequirementId::Stdin,
        name: "stdin",
        interface: ObjectInterface::Stream,
        max_rights: Rights::READ,
    },
    RequirementSpec {
        id: RequirementId::Stdout,
        name: "stdout",
        interface: ObjectInterface::Stream,
        max_rights: Rights::from_bits(Rights::WRITE.bits() | Rights::DUPLICATE.bits()),
    },
    RequirementSpec {
        id: RequirementId::Stderr,
        name: "stderr",
        interface: ObjectInterface::Stream,
        max_rights: Rights::from_bits(Rights::WRITE.bits() | Rights::DUPLICATE.bits()),
    },
    RequirementSpec {
        id: RequirementId::MonotonicClock,
        name: "monotonic_clock",
        interface: ObjectInterface::Clock,
        max_rights: Rights::READ,
    },
];

pub fn requirement(id: RequirementId) -> Option<&'static RequirementSpec> {
    REQUIREMENTS.iter().find(|spec| spec.id == id)
}

pub fn requirement_by_id(id: u32) -> Option<&'static RequirementSpec> {
    REQUIREMENTS.iter().find(|spec| spec.id as u32 == id)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum OperationId {
    ProcessExit = 1,
    HandleClose = 2,
    HandleDuplicate = 3,
    HandleRestrict = 4,
    StreamRead = 5,
    StreamWrite = 6,
    ClockRead = 7,
    MemoryAllocate = 8,
    MemoryFree = 9,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OperationSpec {
    pub id: OperationId,
    pub name: &'static str,
    pub interface: Option<ObjectInterface>,
    pub required_rights: Rights,
    pub signature: &'static str,
    pub signature_hash: [u8; 32],
}

pub const OPERATIONS: [OperationSpec; 9] = [
    OperationSpec {
        id: OperationId::ProcessExit,
        name: "process.exit",
        interface: Some(ObjectInterface::Process),
        required_rights: Rights::EXIT,
        signature: "epoch=1;operation=1;object=1;args=u32,zero,zero,zero,zero;result=noreturn",
        signature_hash: [
            0xa6, 0xc1, 0xfb, 0x70, 0xa1, 0x0c, 0x4b, 0x82, 0xa6, 0x32, 0xa3, 0x02, 0x75, 0xef,
            0x98, 0xd9, 0x62, 0x4e, 0x46, 0xee, 0x8c, 0x3b, 0x2e, 0xdf, 0x02, 0xac, 0xb4, 0xe9,
            0xef, 0x83, 0x44, 0x2b,
        ],
    },
    OperationSpec {
        id: OperationId::HandleClose,
        name: "handle.close",
        interface: None,
        required_rights: Rights::NONE,
        signature: "epoch=1;operation=2;object=any;args=zero,zero,zero,zero,zero;result=status",
        signature_hash: [
            0x36, 0xac, 0x17, 0x9c, 0x8e, 0xe6, 0xd0, 0x3b, 0xc9, 0xab, 0x69, 0x39, 0x9c, 0x07,
            0x8c, 0x8c, 0x82, 0x5d, 0xb1, 0xd1, 0x42, 0xd3, 0x81, 0x15, 0x3d, 0x62, 0xff, 0x6f,
            0x72, 0xcf, 0x58, 0x87,
        ],
    },
    OperationSpec {
        id: OperationId::HandleDuplicate,
        name: "handle.duplicate",
        interface: None,
        required_rights: Rights::DUPLICATE,
        signature: "epoch=1;operation=3;object=any;args=zero,zero,zero,zero,zero;result=handle",
        signature_hash: [
            0x88, 0x42, 0x43, 0xc0, 0x3f, 0x2f, 0x23, 0x04, 0xc3, 0xe3, 0xa8, 0xac, 0x70, 0xfa,
            0x0f, 0xe1, 0xad, 0x7e, 0xd5, 0xff, 0x56, 0xee, 0x70, 0xe5, 0x9e, 0x0b, 0x49, 0xa2,
            0xe1, 0x1b, 0x8b, 0xdd,
        ],
    },
    OperationSpec {
        id: OperationId::HandleRestrict,
        name: "handle.restrict",
        interface: None,
        required_rights: Rights::DUPLICATE,
        signature: "epoch=1;operation=4;object=any;args=rights64,zero,zero,zero,zero;result=handle",
        signature_hash: [
            0x5d, 0x15, 0xdf, 0x98, 0xb9, 0xfd, 0x3b, 0xdc, 0x3a, 0x3a, 0x9b, 0x1a, 0x81, 0xdc,
            0xb9, 0x96, 0xaf, 0x9a, 0x7a, 0x4c, 0x2c, 0x08, 0xe7, 0x7b, 0x7d, 0xfa, 0x0e, 0x69,
            0x7c, 0xa8, 0x5d, 0x42,
        ],
    },
    OperationSpec {
        id: OperationId::StreamRead,
        name: "stream.read",
        interface: Some(ObjectInterface::Stream),
        required_rights: Rights::READ,
        signature: "epoch=1;operation=5;object=3;args=user_mut_ptr,u64,zero,zero,zero;result=u64",
        signature_hash: [
            0x98, 0x99, 0xf2, 0xa8, 0x8d, 0x7b, 0x89, 0xed, 0x70, 0xff, 0x0c, 0x4c, 0x21, 0x63,
            0x2c, 0x13, 0xee, 0x8a, 0xb9, 0xfa, 0xb7, 0x30, 0x36, 0x8d, 0x79, 0x8c, 0x4a, 0xa1,
            0x62, 0x10, 0x87, 0x82,
        ],
    },
    OperationSpec {
        id: OperationId::StreamWrite,
        name: "stream.write",
        interface: Some(ObjectInterface::Stream),
        required_rights: Rights::WRITE,
        signature: "epoch=1;operation=6;object=3;args=user_const_ptr,u64,zero,zero,zero;result=u64",
        signature_hash: [
            0x55, 0xee, 0x0a, 0x78, 0xe0, 0xe3, 0xbd, 0xf4, 0x89, 0x0a, 0xdf, 0x14, 0x1d, 0xed,
            0x39, 0x58, 0x8a, 0xdb, 0xe7, 0x4d, 0x63, 0x18, 0xbd, 0x6f, 0x73, 0xc4, 0xca, 0x6a,
            0x35, 0xe6, 0x8d, 0xac,
        ],
    },
    OperationSpec {
        id: OperationId::ClockRead,
        name: "clock.read",
        interface: Some(ObjectInterface::Clock),
        required_rights: Rights::READ,
        signature: "epoch=1;operation=7;object=4;args=zero,zero,zero,zero,zero;result=u64",
        signature_hash: [
            0x89, 0x88, 0x5f, 0x4f, 0x0c, 0x44, 0xda, 0x7b, 0xec, 0xab, 0x99, 0xb2, 0x7d, 0x24,
            0xae, 0x92, 0x72, 0x6f, 0xbf, 0x94, 0xe3, 0x8f, 0x09, 0x00, 0xe0, 0xd4, 0xde, 0x88,
            0x70, 0x3d, 0x28, 0x03,
        ],
    },
    OperationSpec {
        id: OperationId::MemoryAllocate,
        name: "memory.allocate",
        interface: Some(ObjectInterface::AddressSpace),
        required_rights: Rights::ALLOCATE,
        signature: "epoch=1;operation=8;object=2;args=u64,u64,zero,zero,zero;result=u64,u64",
        signature_hash: [
            0x43, 0x77, 0x0b, 0x2c, 0xe5, 0x3f, 0xcb, 0x6d, 0x76, 0xdc, 0xd4, 0x44, 0x50, 0xd2,
            0xcf, 0x86, 0xb0, 0x7f, 0x45, 0x05, 0x11, 0x5a, 0xb4, 0x4c, 0x57, 0xb9, 0x47, 0x88,
            0x0b, 0x21, 0x20, 0xfb,
        ],
    },
    OperationSpec {
        id: OperationId::MemoryFree,
        name: "memory.free",
        interface: Some(ObjectInterface::AddressSpace),
        required_rights: Rights::FREE,
        signature: "epoch=1;operation=9;object=2;args=u64,u64,zero,zero,zero;result=status",
        signature_hash: [
            0x6b, 0x1f, 0x1c, 0xef, 0x89, 0x62, 0x0e, 0x51, 0x69, 0x8e, 0x2b, 0x9a, 0x93, 0x7b,
            0xa7, 0x9e, 0x4e, 0x22, 0xbd, 0xcd, 0x85, 0xf3, 0x93, 0x5c, 0x92, 0x18, 0xd4, 0xe5,
            0x36, 0xee, 0x51, 0x4e,
        ],
    },
];

pub fn operation(id: OperationId) -> Option<&'static OperationSpec> {
    OPERATIONS.iter().find(|spec| spec.id == id)
}

pub fn operation_by_id(id: u32) -> Option<&'static OperationSpec> {
    OPERATIONS.iter().find(|spec| spec.id as u32 == id)
}
