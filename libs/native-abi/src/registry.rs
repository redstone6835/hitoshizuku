//! Hitoshizuku Native ABI epoch 1 的机器身份注册表。

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
    Image = 5,
    EventPort = 6,
    Component = 7,
    ComponentTransaction = 8,
    Interface = 9,
    Thread = 10,
    MemoryObject = 11,
    Directory = 12,
    File = 13,
    Channel = 14,
    SubmissionRing = 15,
    Socket = 16,
    DeviceFunction = 17,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InterfaceSpec {
    pub name: &'static str,
    pub interface: ObjectInterface,
}

pub const INTERFACES: [InterfaceSpec; 17] = [
    InterfaceSpec {
        name: "process",
        interface: ObjectInterface::Process,
    },
    InterfaceSpec {
        name: "address_space",
        interface: ObjectInterface::AddressSpace,
    },
    InterfaceSpec {
        name: "stream",
        interface: ObjectInterface::Stream,
    },
    InterfaceSpec {
        name: "clock",
        interface: ObjectInterface::Clock,
    },
    InterfaceSpec {
        name: "image",
        interface: ObjectInterface::Image,
    },
    InterfaceSpec {
        name: "event_port",
        interface: ObjectInterface::EventPort,
    },
    InterfaceSpec {
        name: "component",
        interface: ObjectInterface::Component,
    },
    InterfaceSpec {
        name: "component_transaction",
        interface: ObjectInterface::ComponentTransaction,
    },
    InterfaceSpec {
        name: "interface",
        interface: ObjectInterface::Interface,
    },
    InterfaceSpec {
        name: "thread",
        interface: ObjectInterface::Thread,
    },
    InterfaceSpec {
        name: "memory_object",
        interface: ObjectInterface::MemoryObject,
    },
    InterfaceSpec {
        name: "directory",
        interface: ObjectInterface::Directory,
    },
    InterfaceSpec {
        name: "file",
        interface: ObjectInterface::File,
    },
    InterfaceSpec {
        name: "channel",
        interface: ObjectInterface::Channel,
    },
    InterfaceSpec {
        name: "submission_ring",
        interface: ObjectInterface::SubmissionRing,
    },
    InterfaceSpec {
        name: "socket",
        interface: ObjectInterface::Socket,
    },
    InterfaceSpec {
        name: "device_function",
        interface: ObjectInterface::DeviceFunction,
    },
];

pub fn interface_spec(interface: ObjectInterface) -> &'static InterfaceSpec {
    INTERFACES
        .iter()
        .find(|spec| spec.interface == interface)
        .expect("每个 ObjectInterface 都必须登记公开名称")
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
    pub const CREATE: Self = Self(1 << 6);
    pub const EXECUTE: Self = Self(1 << 7);
    pub const SPAWN: Self = Self(1 << 8);
    pub const REPLACE: Self = Self(1 << 9);
    pub const INSPECT: Self = Self(1 << 10);
    pub const WAIT: Self = Self(1 << 11);
    pub const TERMINATE: Self = Self(1 << 12);
    pub const OBSERVE: Self = Self(1 << 13);
    pub const BIND: Self = Self(1 << 14);
    pub const LOAD: Self = Self(1 << 15);
    pub const UNLOAD: Self = Self(1 << 16);
    pub const MAP: Self = Self(1 << 17);
    pub const RESIZE: Self = Self(1 << 18);
    pub const OPEN: Self = Self(1 << 19);
    pub const MODIFY: Self = Self(1 << 20);
    pub const SEND: Self = Self(1 << 21);
    pub const RECEIVE: Self = Self(1 << 22);
    pub const REGISTER: Self = Self(1 << 23);
    pub const SUBMIT: Self = Self(1 << 24);
    pub const CANCEL: Self = Self(1 << 25);

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

pub const RIGHTS: [RightSpec; 26] = [
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
    RightSpec {
        name: "create",
        right: Rights::CREATE,
    },
    RightSpec {
        name: "execute",
        right: Rights::EXECUTE,
    },
    RightSpec {
        name: "spawn",
        right: Rights::SPAWN,
    },
    RightSpec {
        name: "replace",
        right: Rights::REPLACE,
    },
    RightSpec {
        name: "inspect",
        right: Rights::INSPECT,
    },
    RightSpec {
        name: "wait",
        right: Rights::WAIT,
    },
    RightSpec {
        name: "terminate",
        right: Rights::TERMINATE,
    },
    RightSpec {
        name: "observe",
        right: Rights::OBSERVE,
    },
    RightSpec {
        name: "bind",
        right: Rights::BIND,
    },
    RightSpec {
        name: "load",
        right: Rights::LOAD,
    },
    RightSpec {
        name: "unload",
        right: Rights::UNLOAD,
    },
    RightSpec {
        name: "map",
        right: Rights::MAP,
    },
    RightSpec {
        name: "resize",
        right: Rights::RESIZE,
    },
    RightSpec {
        name: "open",
        right: Rights::OPEN,
    },
    RightSpec {
        name: "modify",
        right: Rights::MODIFY,
    },
    RightSpec {
        name: "send",
        right: Rights::SEND,
    },
    RightSpec {
        name: "receive",
        right: Rights::RECEIVE,
    },
    RightSpec {
        name: "register",
        right: Rights::REGISTER,
    },
    RightSpec {
        name: "submit",
        right: Rights::SUBMIT,
    },
    RightSpec {
        name: "cancel",
        right: Rights::CANCEL,
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
    RootDirectory = 7,
    DeviceFunction = 8,
    ServiceChannel = 9,
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
            7 => Some(Self::RootDirectory),
            8 => Some(Self::DeviceFunction),
            9 => Some(Self::ServiceChannel),
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

pub const REQUIREMENTS: [RequirementSpec; 9] = [
    RequirementSpec {
        id: RequirementId::SelfProcess,
        name: "self_process",
        interface: ObjectInterface::Process,
        max_rights: Rights::from_bits(
            Rights::EXIT.bits()
                | Rights::DUPLICATE.bits()
                | Rights::CREATE.bits()
                | Rights::SPAWN.bits()
                | Rights::REPLACE.bits()
                | Rights::INSPECT.bits()
                | Rights::WAIT.bits()
                | Rights::TERMINATE.bits()
                | Rights::OBSERVE.bits()
                | Rights::LOAD.bits(),
        ),
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
        max_rights: Rights::from_bits(Rights::READ.bits() | Rights::OBSERVE.bits()),
    },
    RequirementSpec {
        id: RequirementId::Stdout,
        name: "stdout",
        interface: ObjectInterface::Stream,
        max_rights: Rights::from_bits(
            Rights::WRITE.bits() | Rights::DUPLICATE.bits() | Rights::OBSERVE.bits(),
        ),
    },
    RequirementSpec {
        id: RequirementId::Stderr,
        name: "stderr",
        interface: ObjectInterface::Stream,
        max_rights: Rights::from_bits(
            Rights::WRITE.bits() | Rights::DUPLICATE.bits() | Rights::OBSERVE.bits(),
        ),
    },
    RequirementSpec {
        id: RequirementId::MonotonicClock,
        name: "monotonic_clock",
        interface: ObjectInterface::Clock,
        max_rights: Rights::READ,
    },
    RequirementSpec {
        id: RequirementId::RootDirectory,
        name: "root_directory",
        interface: ObjectInterface::Directory,
        max_rights: Rights::from_bits(
            Rights::OPEN.bits()
                | Rights::MODIFY.bits()
                | Rights::INSPECT.bits()
                | Rights::DUPLICATE.bits(),
        ),
    },
    RequirementSpec {
        id: RequirementId::DeviceFunction,
        name: "device_function",
        interface: ObjectInterface::DeviceFunction,
        max_rights: Rights::from_bits(
            Rights::SUBMIT.bits()
                | Rights::MODIFY.bits()
                | Rights::MAP.bits()
                | Rights::INSPECT.bits()
                | Rights::DUPLICATE.bits(),
        ),
    },
    RequirementSpec {
        id: RequirementId::ServiceChannel,
        name: "service_channel",
        interface: ObjectInterface::Channel,
        max_rights: Rights::from_bits(
            Rights::SEND.bits()
                | Rights::RECEIVE.bits()
                | Rights::DUPLICATE.bits()
                | Rights::OBSERVE.bits(),
        ),
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
    ImageCreate = 10,
    ProcessSpawn = 11,
    ProcessReplace = 12,
    ProcessQuery = 13,
    ProcessWait = 14,
    ProcessTerminate = 15,
    EventCreate = 16,
    EventBind = 17,
    EventTimer = 18,
    EventCancel = 19,
    EventWait = 20,
    ComponentLoad = 21,
    ComponentActivate = 22,
    ComponentQuery = 23,
    ComponentInterface = 24,
    ComponentUnload = 25,
    ComponentFinish = 26,
    ComponentWake = 27,
    ThreadCreate = 28,
    ThreadJoin = 29,
    ThreadTerminate = 30,
    ThreadQuery = 31,
    MemoryCreate = 32,
    MemoryMap = 33,
    MemoryUnmap = 34,
    MemoryQuery = 35,
    DirectoryOpen = 36,
    DirectoryCreate = 37,
    DirectoryRemove = 38,
    DirectoryQuery = 39,
    FileRead = 40,
    FileWrite = 41,
    FileResize = 42,
    FileQuery = 43,
    FileMap = 44,
    ChannelCreate = 45,
    ChannelSend = 46,
    ChannelReceive = 47,
    RingCreate = 48,
    RingRegister = 49,
    RingUnregister = 50,
    RingKick = 51,
    RingCancel = 52,
    RingWait = 53,
    RingQuery = 54,
    SocketCreate = 55,
    SocketBind = 56,
    SocketConnect = 57,
    SocketListen = 58,
    SocketAccept = 59,
    SocketSend = 60,
    SocketReceive = 61,
    SocketShutdown = 62,
    SocketQuery = 63,
    DeviceInvoke = 64,
    DeviceQuery = 65,
    ImageQuery = 66,
    ThreadExit = 67,
    ThreadYield = 68,
    MemoryRevoke = 69,
    MemoryStatistics = 70,
}

/// operation 是否可以直接进入 SubmissionRing。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmissionMode {
    /// 参数包含用户指针或控制面状态，只允许同步调用。
    DirectOnly,
    /// 参数完全位于 descriptor 内，可以异步执行。
    Inline,
    /// 参数只引用已经注册的 MemoryRegion，可以异步执行。
    MemoryRegion,
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

impl OperationSpec {
    pub const fn submission(&self) -> SubmissionMode {
        match self.id {
            OperationId::ProcessExit
            | OperationId::HandleClose
            | OperationId::HandleDuplicate
            | OperationId::HandleRestrict
            | OperationId::MemoryAllocate
            | OperationId::MemoryFree
            | OperationId::ImageCreate
            | OperationId::ProcessSpawn
            | OperationId::ProcessReplace
            | OperationId::ProcessQuery
            | OperationId::ProcessWait
            | OperationId::ProcessTerminate
            | OperationId::EventCreate
            | OperationId::EventBind
            | OperationId::EventTimer
            | OperationId::EventCancel
            | OperationId::EventWait
            | OperationId::ComponentLoad
            | OperationId::ComponentActivate
            | OperationId::ComponentQuery
            | OperationId::ComponentInterface
            | OperationId::ComponentUnload
            | OperationId::ComponentFinish
            | OperationId::ComponentWake
            | OperationId::ThreadCreate
            | OperationId::ThreadJoin
            | OperationId::ThreadTerminate
            | OperationId::ThreadQuery
            | OperationId::ThreadExit
            | OperationId::ThreadYield
            | OperationId::MemoryCreate
            | OperationId::MemoryMap
            | OperationId::MemoryUnmap
            | OperationId::MemoryQuery
            | OperationId::MemoryRevoke
            | OperationId::MemoryStatistics
            | OperationId::DirectoryOpen
            | OperationId::DirectoryCreate
            | OperationId::DirectoryRemove
            | OperationId::DirectoryQuery
            | OperationId::FileResize
            | OperationId::FileQuery
            | OperationId::FileMap
            | OperationId::ChannelCreate => SubmissionMode::DirectOnly,
            OperationId::ClockRead => SubmissionMode::Inline,
            OperationId::RingCreate
            | OperationId::RingRegister
            | OperationId::RingUnregister
            | OperationId::RingKick
            | OperationId::RingCancel
            | OperationId::RingWait
            | OperationId::RingQuery => SubmissionMode::DirectOnly,
            OperationId::StreamRead
            | OperationId::StreamWrite
            | OperationId::FileRead
            | OperationId::FileWrite
            | OperationId::ChannelSend
            | OperationId::ChannelReceive
            | OperationId::SocketSend
            | OperationId::SocketReceive => SubmissionMode::MemoryRegion,
            OperationId::SocketCreate
            | OperationId::SocketBind
            | OperationId::SocketConnect
            | OperationId::SocketListen
            | OperationId::SocketAccept
            | OperationId::SocketShutdown
            | OperationId::SocketQuery => SubmissionMode::DirectOnly,
            OperationId::DeviceInvoke => SubmissionMode::MemoryRegion,
            OperationId::DeviceQuery | OperationId::ImageQuery => SubmissionMode::DirectOnly,
        }
    }
}

pub const OPERATIONS: [OperationSpec; 70] = [
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
    OperationSpec {
        id: OperationId::ImageCreate,
        name: "image.create",
        interface: Some(ObjectInterface::Process),
        required_rights: Rights::CREATE,
        signature: "epoch=1;operation=10;object=1;args=user_const_ptr,u64,zero,zero,zero;result=handle",
        signature_hash: [
            0xab, 0xa6, 0xb2, 0x4f, 0xbe, 0x9b, 0x0a, 0x6b, 0xa6, 0xda, 0x65, 0x90, 0x9d, 0xe6,
            0xd7, 0x98, 0x52, 0x63, 0xb9, 0x96, 0x5e, 0x3a, 0xab, 0xa6, 0x0a, 0xd3, 0x25, 0x4f,
            0x7a, 0xfa, 0xc2, 0x4b,
        ],
    },
    OperationSpec {
        id: OperationId::ProcessSpawn,
        name: "process.spawn",
        interface: Some(ObjectInterface::Process),
        required_rights: Rights::SPAWN,
        signature: "epoch=1;operation=11;object=1;args=user_const_ptr,u64,zero,zero,zero;result=handle",
        signature_hash: [
            0xf2, 0x55, 0xa4, 0xe2, 0xf4, 0x80, 0x69, 0xc2, 0x22, 0x07, 0x1c, 0xde, 0xca, 0xe7,
            0x2e, 0x90, 0x02, 0x77, 0x07, 0x88, 0x30, 0x38, 0x9a, 0x18, 0xf5, 0x84, 0x63, 0x91,
            0xa2, 0x3b, 0x71, 0xf4,
        ],
    },
    OperationSpec {
        id: OperationId::ProcessReplace,
        name: "process.replace",
        interface: Some(ObjectInterface::Process),
        required_rights: Rights::REPLACE,
        signature: "epoch=1;operation=12;object=1;args=user_const_ptr,u64,zero,zero,zero;result=noreturn",
        signature_hash: [
            0x96, 0x54, 0x79, 0x1d, 0x03, 0x02, 0xfc, 0xc9, 0xbf, 0xc9, 0xb4, 0xb0, 0xfc, 0x84,
            0xf2, 0xc1, 0x25, 0x34, 0x87, 0xb1, 0x25, 0x18, 0x31, 0xd6, 0x28, 0x3f, 0x94, 0x2c,
            0xe3, 0x28, 0xb8, 0xc3,
        ],
    },
    OperationSpec {
        id: OperationId::ProcessQuery,
        name: "process.query",
        interface: Some(ObjectInterface::Process),
        required_rights: Rights::INSPECT,
        signature: "epoch=1;operation=13;object=1;args=user_mut_ptr,zero,zero,zero,zero;result=status",
        signature_hash: [
            0x28, 0x2b, 0xd5, 0x41, 0xea, 0xae, 0x3e, 0xc5, 0xd3, 0xeb, 0xd7, 0x47, 0x30, 0x89,
            0x90, 0xd6, 0x3c, 0xca, 0xbd, 0x2f, 0xf1, 0xd5, 0x9d, 0x12, 0x62, 0x15, 0x24, 0x63,
            0x89, 0xc3, 0x11, 0xba,
        ],
    },
    OperationSpec {
        id: OperationId::ProcessWait,
        name: "process.wait",
        interface: Some(ObjectInterface::Process),
        required_rights: Rights::WAIT,
        signature: "epoch=1;operation=14;object=1;args=user_mut_ptr,u64,zero,zero,zero;result=status",
        signature_hash: [
            0x83, 0xeb, 0x1a, 0x83, 0xba, 0x3f, 0xd3, 0x31, 0x63, 0xbd, 0x6d, 0x5c, 0x80, 0x32,
            0x06, 0x50, 0x4b, 0x8e, 0xfa, 0x4f, 0xbb, 0x3d, 0xf0, 0x43, 0xae, 0xbd, 0x97, 0x79,
            0x71, 0x0c, 0xab, 0xff,
        ],
    },
    OperationSpec {
        id: OperationId::ProcessTerminate,
        name: "process.terminate",
        interface: Some(ObjectInterface::Process),
        required_rights: Rights::TERMINATE,
        signature: "epoch=1;operation=15;object=1;args=u32,zero,zero,zero,zero;result=status",
        signature_hash: [
            0xa1, 0xdf, 0x05, 0x20, 0x27, 0xef, 0xbd, 0xbb, 0x93, 0xfe, 0x88, 0x83, 0xa5, 0x70,
            0x1d, 0xdd, 0x07, 0x67, 0xd4, 0x18, 0x89, 0xee, 0xa7, 0xaa, 0x8e, 0x70, 0x03, 0xc4,
            0x7f, 0xe0, 0x91, 0x19,
        ],
    },
    OperationSpec {
        id: OperationId::EventCreate,
        name: "event.create",
        interface: Some(ObjectInterface::Process),
        required_rights: Rights::CREATE,
        signature: "epoch=1;operation=16;object=1;args=u32,zero,zero,zero,zero;result=handle",
        signature_hash: [
            0xcf, 0xc4, 0x61, 0xbf, 0x5a, 0xac, 0xf8, 0x15, 0xb2, 0x88, 0x99, 0x95, 0x63, 0x80,
            0xac, 0x9d, 0xb9, 0x53, 0xab, 0x59, 0x2a, 0x8d, 0x55, 0x6b, 0xf6, 0xc1, 0x35, 0x3e,
            0xe3, 0x5c, 0xb4, 0x58,
        ],
    },
    OperationSpec {
        id: OperationId::EventBind,
        name: "event.bind",
        interface: Some(ObjectInterface::EventPort),
        required_rights: Rights::BIND,
        signature: "epoch=1;operation=17;object=6;args=handle,u32,u64,zero,zero;result=u64",
        signature_hash: [
            0x24, 0x16, 0x1d, 0x6d, 0x32, 0x62, 0x94, 0xa6, 0xfe, 0x42, 0x53, 0xaf, 0x6d, 0xb0,
            0xc1, 0xeb, 0x95, 0x64, 0x98, 0x0e, 0x0a, 0x6d, 0x5e, 0xe2, 0x9f, 0x56, 0xbb, 0x39,
            0xf1, 0x5b, 0x30, 0x5e,
        ],
    },
    OperationSpec {
        id: OperationId::EventTimer,
        name: "event.timer",
        interface: Some(ObjectInterface::EventPort),
        required_rights: Rights::BIND,
        signature: "epoch=1;operation=18;object=6;args=u64,u64,u64,zero,zero;result=u64",
        signature_hash: [
            0xee, 0x95, 0xd7, 0xff, 0x7e, 0x8d, 0xe1, 0x3a, 0x80, 0x8c, 0xae, 0xc9, 0xe2, 0x11,
            0xd6, 0xb9, 0x78, 0xf6, 0x40, 0xee, 0x89, 0x31, 0x8e, 0xb0, 0x8d, 0x4e, 0x69, 0xfa,
            0x54, 0x2e, 0x82, 0x70,
        ],
    },
    OperationSpec {
        id: OperationId::EventCancel,
        name: "event.cancel",
        interface: Some(ObjectInterface::EventPort),
        required_rights: Rights::BIND,
        signature: "epoch=1;operation=19;object=6;args=u64,zero,zero,zero,zero;result=status",
        signature_hash: [
            0x75, 0x00, 0x9e, 0x0a, 0x5e, 0x3e, 0x71, 0xf6, 0xb3, 0x14, 0x78, 0x0c, 0xae, 0x4b,
            0x24, 0x88, 0x74, 0x44, 0xdd, 0xe3, 0xfe, 0x77, 0x9c, 0x00, 0xe8, 0xf4, 0x56, 0xb9,
            0x2b, 0x37, 0xb5, 0xb0,
        ],
    },
    OperationSpec {
        id: OperationId::EventWait,
        name: "event.wait",
        interface: Some(ObjectInterface::EventPort),
        required_rights: Rights::OBSERVE,
        signature: "epoch=1;operation=20;object=6;args=user_mut_ptr,u32,u64,zero,zero;result=u64",
        signature_hash: [
            0xda, 0x7b, 0xab, 0x23, 0xc5, 0x0d, 0xc5, 0x9d, 0x7c, 0x0a, 0xd4, 0x2e, 0x2a, 0x0d,
            0x5a, 0xbb, 0x14, 0xaf, 0xcc, 0x9e, 0xa4, 0x45, 0x49, 0xa2, 0x79, 0x16, 0x78, 0x68,
            0x14, 0xfe, 0x9b, 0xa1,
        ],
    },
    OperationSpec {
        id: OperationId::ComponentLoad,
        name: "component.load",
        interface: Some(ObjectInterface::Process),
        required_rights: Rights::LOAD,
        signature: "epoch=1;operation=21;object=1;args=user_const_ptr,user_mut_ptr,zero,zero,zero;result=handle",
        signature_hash: [
            0xe6, 0xe1, 0x17, 0x41, 0x37, 0x54, 0x37, 0xfd, 0x5a, 0x55, 0x8e, 0xd1, 0x4f, 0xe2,
            0xe7, 0xaa, 0x3d, 0xb0, 0x40, 0xd7, 0x60, 0xa1, 0x40, 0x30, 0xb0, 0xfc, 0xe8, 0xd1,
            0x7a, 0x59, 0x78, 0x5f,
        ],
    },
    OperationSpec {
        id: OperationId::ComponentActivate,
        name: "component.activate",
        interface: Some(ObjectInterface::ComponentTransaction),
        required_rights: Rights::LOAD,
        signature: "epoch=1;operation=22;object=8;args=u32,user_mut_ptr,zero,zero,zero;result=handle",
        signature_hash: [
            0x0d, 0xbf, 0x69, 0x9c, 0x0b, 0xb6, 0xa7, 0x1e, 0x77, 0x07, 0x59, 0xbb, 0x99, 0xde,
            0x0b, 0xc9, 0x10, 0x95, 0xaf, 0x44, 0x53, 0x9e, 0x36, 0x3b, 0x81, 0xc8, 0x4e, 0x1a,
            0xf7, 0x3a, 0xcd, 0x32,
        ],
    },
    OperationSpec {
        id: OperationId::ComponentQuery,
        name: "component.query",
        interface: Some(ObjectInterface::Component),
        required_rights: Rights::INSPECT,
        signature: "epoch=1;operation=23;object=7;args=user_mut_ptr,zero,zero,zero,zero;result=status",
        signature_hash: [
            0xf2, 0xb5, 0xcf, 0x52, 0x70, 0x05, 0x59, 0xf5, 0x98, 0x24, 0x61, 0xe3, 0x1a, 0xff,
            0x19, 0xe6, 0x4d, 0x55, 0x31, 0x93, 0x38, 0x09, 0xcd, 0xd6, 0xfb, 0xae, 0x17, 0x36,
            0x6b, 0x2b, 0x5a, 0x8e,
        ],
    },
    OperationSpec {
        id: OperationId::ComponentInterface,
        name: "component.interface",
        interface: Some(ObjectInterface::Component),
        required_rights: Rights::BIND,
        signature: "epoch=1;operation=24;object=7;args=user_const_ptr,zero,zero,zero,zero;result=handle,u64",
        signature_hash: [
            0xf4, 0x4d, 0x54, 0xc0, 0xd4, 0x86, 0xda, 0x22, 0x21, 0x6a, 0xe9, 0x47, 0xf2, 0x42,
            0x0f, 0x9a, 0x29, 0xf5, 0x02, 0x3d, 0x57, 0x9c, 0xbb, 0x46, 0xe2, 0x38, 0x58, 0xc1,
            0x72, 0xd3, 0x7f, 0x8e,
        ],
    },
    OperationSpec {
        id: OperationId::ComponentUnload,
        name: "component.unload",
        interface: Some(ObjectInterface::Component),
        required_rights: Rights::UNLOAD,
        signature: "epoch=1;operation=25;object=7;args=u64,user_mut_ptr,u64,zero,zero;result=handle",
        signature_hash: [
            0xb7, 0x92, 0x9f, 0x4b, 0x32, 0xaf, 0x38, 0x33, 0x21, 0xe5, 0x70, 0x14, 0xcb, 0xfe,
            0x3d, 0xc8, 0x39, 0x12, 0xc5, 0xee, 0x9c, 0x7e, 0xff, 0x92, 0x2e, 0x71, 0xf1, 0xae,
            0xa4, 0x71, 0x7b, 0x63,
        ],
    },
    OperationSpec {
        id: OperationId::ComponentFinish,
        name: "component.finish",
        interface: Some(ObjectInterface::ComponentTransaction),
        required_rights: Rights::UNLOAD,
        signature: "epoch=1;operation=26;object=8;args=u32,user_mut_ptr,zero,zero,zero;result=status",
        signature_hash: [
            0x14, 0x64, 0xf0, 0xe1, 0x86, 0xc4, 0x2f, 0x80, 0x46, 0x9c, 0xd7, 0xdd, 0x9e, 0xf7,
            0xaf, 0xfe, 0xbb, 0xd3, 0xf2, 0x11, 0xa4, 0x24, 0x01, 0x0f, 0x0f, 0x61, 0x56, 0x5c,
            0x7e, 0x05, 0x17, 0x59,
        ],
    },
    OperationSpec {
        id: OperationId::ComponentWake,
        name: "component.wake",
        interface: Some(ObjectInterface::Component),
        required_rights: Rights::NONE,
        signature: "epoch=1;operation=27;object=7;args=u64,zero,zero,zero,zero;result=status",
        signature_hash: [
            0xb0, 0x98, 0x3b, 0x8d, 0x5c, 0x4a, 0xda, 0x86, 0x5e, 0xf6, 0x7b, 0x7f, 0x58, 0xc8,
            0x17, 0xec, 0x58, 0x67, 0x53, 0x2d, 0xf7, 0x7f, 0xf2, 0xf6, 0xa2, 0xee, 0x85, 0x0a,
            0x27, 0xe7, 0xd1, 0xc3,
        ],
    },
    OperationSpec {
        id: OperationId::ThreadCreate,
        name: "thread.create",
        interface: Some(ObjectInterface::Process),
        required_rights: Rights::CREATE,
        signature: "epoch=1;operation=28;object=1;args=user_const_ptr,handle,zero,zero,zero;result=handle",
        signature_hash: [
            0xd7, 0xe7, 0x72, 0xdb, 0xb0, 0x59, 0xcb, 0x9f, 0xcc, 0x89, 0x29, 0xcd, 0x72, 0x3d,
            0x2a, 0x00, 0xe6, 0x0b, 0x5f, 0x49, 0xca, 0xc3, 0xfc, 0x45, 0x04, 0xe2, 0x2a, 0x07,
            0x32, 0x92, 0x6b, 0x3d,
        ],
    },
    OperationSpec {
        id: OperationId::ThreadJoin,
        name: "thread.join",
        interface: Some(ObjectInterface::Thread),
        required_rights: Rights::WAIT,
        signature: "epoch=1;operation=29;object=10;args=user_mut_ptr,u64,zero,zero,zero;result=status",
        signature_hash: [
            0x85, 0x34, 0x1c, 0xa2, 0x80, 0xfe, 0xa2, 0x03, 0xbd, 0x5b, 0x56, 0x99, 0x22, 0x2b,
            0x47, 0x30, 0x5c, 0x73, 0x99, 0x83, 0x01, 0x99, 0x39, 0x64, 0x73, 0x7f, 0x35, 0x5e,
            0x18, 0x0a, 0xd7, 0x20,
        ],
    },
    OperationSpec {
        id: OperationId::ThreadTerminate,
        name: "thread.terminate",
        interface: Some(ObjectInterface::Thread),
        required_rights: Rights::TERMINATE,
        signature: "epoch=1;operation=30;object=10;args=u32,zero,zero,zero,zero;result=status",
        signature_hash: [
            0x06, 0x23, 0x3b, 0x76, 0x3c, 0xae, 0x15, 0xc0, 0x51, 0x0f, 0x00, 0xbd, 0xe4, 0x35,
            0x91, 0xd4, 0xb9, 0xc2, 0x32, 0xf1, 0xa8, 0x33, 0x80, 0x34, 0x89, 0x0b, 0xe5, 0xb6,
            0x47, 0x92, 0x60, 0x98,
        ],
    },
    OperationSpec {
        id: OperationId::ThreadQuery,
        name: "thread.query",
        interface: Some(ObjectInterface::Thread),
        required_rights: Rights::INSPECT,
        signature: "epoch=1;operation=31;object=10;args=user_mut_ptr,zero,zero,zero,zero;result=status",
        signature_hash: [
            0x0c, 0x0b, 0xde, 0xe2, 0x4f, 0x4c, 0x16, 0x39, 0x34, 0x6d, 0x9f, 0x08, 0xb3, 0x7a,
            0x02, 0x80, 0x80, 0x0c, 0x2b, 0xbd, 0xbd, 0x1c, 0x15, 0xee, 0x0a, 0x95, 0xe6, 0x26,
            0x5e, 0xfe, 0x0b, 0xda,
        ],
    },
    OperationSpec {
        id: OperationId::MemoryCreate,
        name: "memory.create",
        interface: Some(ObjectInterface::Process),
        required_rights: Rights::CREATE,
        signature: "epoch=1;operation=32;object=1;args=user_const_ptr,zero,zero,zero,zero;result=handle",
        signature_hash: [
            0x12, 0x8d, 0xc1, 0x27, 0x27, 0x96, 0x3c, 0x32, 0xec, 0x3a, 0xbd, 0x4b, 0x2c, 0xfb,
            0xff, 0xa8, 0x3f, 0xf7, 0x71, 0x2e, 0x4b, 0x9a, 0x02, 0x46, 0x87, 0xe2, 0x6e, 0xd7,
            0x2c, 0xd5, 0xd4, 0x89,
        ],
    },
    OperationSpec {
        id: OperationId::MemoryMap,
        name: "memory.map",
        interface: Some(ObjectInterface::MemoryObject),
        required_rights: Rights::MAP,
        signature: "epoch=1;operation=33;object=11;args=user_const_ptr,zero,zero,zero,zero;result=u64,u64",
        signature_hash: [
            0x36, 0x04, 0x4f, 0xb6, 0xec, 0xa4, 0xc0, 0xb6, 0x7a, 0x79, 0x1d, 0x01, 0x3b, 0xed,
            0x8c, 0xce, 0x0f, 0xb4, 0x0e, 0x7f, 0x91, 0xc9, 0x4b, 0xc8, 0x35, 0xcf, 0xd9, 0x76,
            0x23, 0xed, 0x45, 0x19,
        ],
    },
    OperationSpec {
        id: OperationId::MemoryUnmap,
        name: "memory.unmap",
        interface: Some(ObjectInterface::AddressSpace),
        required_rights: Rights::FREE,
        signature: "epoch=1;operation=34;object=2;args=u64,u64,zero,zero,zero;result=status",
        signature_hash: [
            0x04, 0x13, 0xc1, 0x7a, 0xea, 0x19, 0xc5, 0xbe, 0x88, 0x30, 0x43, 0xe5, 0xd1, 0x25,
            0x00, 0x2e, 0x90, 0xca, 0x34, 0x80, 0x79, 0x57, 0x43, 0x4c, 0xfd, 0xcc, 0x15, 0xc0,
            0x3d, 0x95, 0xf0, 0xe5,
        ],
    },
    OperationSpec {
        id: OperationId::MemoryQuery,
        name: "memory.query",
        interface: Some(ObjectInterface::MemoryObject),
        required_rights: Rights::INSPECT,
        signature: "epoch=1;operation=35;object=11;args=user_mut_ptr,zero,zero,zero,zero;result=status",
        signature_hash: [
            0xa0, 0xce, 0xe9, 0x7a, 0xa5, 0x38, 0x36, 0x9f, 0xb4, 0xbe, 0xdb, 0x45, 0x3b, 0x82,
            0xd9, 0xdc, 0x94, 0xa3, 0xc4, 0xbd, 0xa1, 0x71, 0x74, 0x7d, 0xd1, 0x2e, 0x70, 0xb5,
            0x3b, 0x43, 0xde, 0x9f,
        ],
    },
    OperationSpec {
        id: OperationId::DirectoryOpen,
        name: "directory.open",
        interface: Some(ObjectInterface::Directory),
        required_rights: Rights::OPEN,
        signature: "epoch=1;operation=36;object=12;args=user_const_ptr,zero,zero,zero,zero;result=handle",
        signature_hash: [
            0xec, 0xf6, 0xab, 0xb4, 0x21, 0x7d, 0xb8, 0xe7, 0x6a, 0x5d, 0xee, 0x7c, 0x3f, 0x3e,
            0xcc, 0xf5, 0x70, 0xea, 0x3d, 0x43, 0xf3, 0x8b, 0x96, 0x0a, 0x06, 0x81, 0xef, 0xee,
            0xff, 0x80, 0x36, 0xa8,
        ],
    },
    OperationSpec {
        id: OperationId::DirectoryCreate,
        name: "directory.create",
        interface: Some(ObjectInterface::Directory),
        required_rights: Rights::MODIFY,
        signature: "epoch=1;operation=37;object=12;args=user_const_ptr,zero,zero,zero,zero;result=handle",
        signature_hash: [
            0xef, 0x70, 0x0a, 0x44, 0xef, 0x86, 0x45, 0xf2, 0xa6, 0xf6, 0x69, 0x2e, 0xac, 0xc6,
            0xe1, 0x6d, 0xc5, 0x37, 0x5e, 0xfa, 0xfd, 0x8b, 0x5f, 0xe2, 0x46, 0x10, 0x40, 0xb3,
            0x9d, 0xb6, 0xfb, 0x56,
        ],
    },
    OperationSpec {
        id: OperationId::DirectoryRemove,
        name: "directory.remove",
        interface: Some(ObjectInterface::Directory),
        required_rights: Rights::MODIFY,
        signature: "epoch=1;operation=38;object=12;args=user_const_ptr,u32,zero,zero,zero;result=status",
        signature_hash: [
            0x19, 0xfc, 0x8d, 0x6a, 0x2b, 0x51, 0x05, 0xb0, 0xad, 0xfd, 0x68, 0x0a, 0xa7, 0x71,
            0xa0, 0x7f, 0x99, 0xba, 0x48, 0xf8, 0xfd, 0x8c, 0xcb, 0xe2, 0xbf, 0xd2, 0x04, 0xf7,
            0xae, 0x0c, 0xb8, 0xa8,
        ],
    },
    OperationSpec {
        id: OperationId::DirectoryQuery,
        name: "directory.query",
        interface: Some(ObjectInterface::Directory),
        required_rights: Rights::INSPECT,
        signature: "epoch=1;operation=39;object=12;args=user_mut_ptr,zero,zero,zero,zero;result=status",
        signature_hash: [
            0xbe, 0x14, 0x14, 0x09, 0xa4, 0x21, 0x4d, 0xae, 0x3b, 0x7c, 0x6a, 0xcb, 0xa7, 0xce,
            0x8a, 0x7f, 0xbe, 0xf6, 0x68, 0xfa, 0xa3, 0x0f, 0x94, 0x06, 0x97, 0x0f, 0x73, 0x6f,
            0x86, 0xcd, 0xd3, 0x98,
        ],
    },
    OperationSpec {
        id: OperationId::FileRead,
        name: "file.read",
        interface: Some(ObjectInterface::File),
        required_rights: Rights::READ,
        signature: "epoch=1;operation=40;object=13;args=user_mut_ptr,u64,u64,u32,zero;result=u64",
        signature_hash: [
            0xee, 0x8c, 0xa3, 0x5d, 0xfd, 0x02, 0x3c, 0xe7, 0xd6, 0xf6, 0xad, 0xf6, 0xbd, 0x91,
            0x8d, 0xee, 0xb4, 0x10, 0xd0, 0x63, 0xc6, 0x9e, 0x31, 0xde, 0x06, 0xcf, 0xf0, 0x69,
            0xf4, 0x75, 0xee, 0x52,
        ],
    },
    OperationSpec {
        id: OperationId::FileWrite,
        name: "file.write",
        interface: Some(ObjectInterface::File),
        required_rights: Rights::WRITE,
        signature: "epoch=1;operation=41;object=13;args=user_const_ptr,u64,u64,u32,zero;result=u64",
        signature_hash: [
            0xba, 0x26, 0x22, 0xb7, 0xc5, 0x69, 0xce, 0x41, 0x03, 0x4b, 0xf4, 0x52, 0x79, 0xf1,
            0xf8, 0x0d, 0xe8, 0x23, 0xbf, 0xc9, 0xb5, 0x4d, 0x37, 0x67, 0xa1, 0x7c, 0xd0, 0xa2,
            0x86, 0x21, 0xd9, 0x56,
        ],
    },
    OperationSpec {
        id: OperationId::FileResize,
        name: "file.resize",
        interface: Some(ObjectInterface::File),
        required_rights: Rights::RESIZE,
        signature: "epoch=1;operation=42;object=13;args=u64,zero,zero,zero,zero;result=status",
        signature_hash: [
            0x4d, 0x2a, 0xb0, 0xe2, 0xd6, 0x3f, 0x20, 0x0b, 0xb5, 0xbd, 0xa1, 0xf0, 0x2c, 0x8d,
            0x53, 0x17, 0x06, 0xa0, 0xf1, 0x0f, 0xee, 0x6f, 0x0b, 0xe7, 0x46, 0x75, 0x3f, 0x59,
            0x8f, 0x02, 0x59, 0x43,
        ],
    },
    OperationSpec {
        id: OperationId::FileQuery,
        name: "file.query",
        interface: Some(ObjectInterface::File),
        required_rights: Rights::INSPECT,
        signature: "epoch=1;operation=43;object=13;args=user_mut_ptr,zero,zero,zero,zero;result=status",
        signature_hash: [
            0xe0, 0x88, 0x94, 0x9e, 0x0c, 0x3f, 0x69, 0x12, 0x4a, 0x08, 0xd8, 0x6f, 0x8b, 0xb7,
            0x13, 0x6e, 0x58, 0x6a, 0xcd, 0x40, 0x87, 0x84, 0x18, 0x43, 0x5d, 0x1a, 0x1e, 0xd0,
            0xe5, 0xc2, 0xca, 0xb0,
        ],
    },
    OperationSpec {
        id: OperationId::FileMap,
        name: "file.map",
        interface: Some(ObjectInterface::File),
        required_rights: Rights::MAP,
        signature: "epoch=1;operation=44;object=13;args=u64,u64,u32,zero,zero;result=handle",
        signature_hash: [
            0xde, 0x00, 0x44, 0x54, 0x7b, 0x9a, 0xb9, 0x1a, 0x16, 0xb5, 0x4f, 0xd3, 0x6a, 0xb8,
            0x92, 0xe8, 0xb5, 0xe2, 0xae, 0x8d, 0x14, 0xb1, 0xf4, 0xa4, 0x35, 0xde, 0xb9, 0x10,
            0x49, 0x07, 0xe5, 0x86,
        ],
    },
    OperationSpec {
        id: OperationId::ChannelCreate,
        name: "channel.create",
        interface: Some(ObjectInterface::Process),
        required_rights: Rights::CREATE,
        signature: "epoch=1;operation=45;object=1;args=u32,zero,zero,zero,zero;result=handle,handle",
        signature_hash: [
            0x51, 0x5d, 0xdd, 0xb2, 0x2c, 0x1b, 0x44, 0x26, 0x2a, 0x31, 0xfc, 0x72, 0x6d, 0x90,
            0x43, 0xd1, 0x64, 0x62, 0x3d, 0x50, 0xe0, 0x5b, 0x56, 0x20, 0x6e, 0x69, 0x61, 0x2b,
            0xb4, 0x79, 0xc1, 0x3c,
        ],
    },
    OperationSpec {
        id: OperationId::ChannelSend,
        name: "channel.send",
        interface: Some(ObjectInterface::Channel),
        required_rights: Rights::SEND,
        signature: "epoch=1;operation=46;object=14;args=user_const_ptr,zero,zero,zero,zero;result=status",
        signature_hash: [
            0xa1, 0x95, 0xb8, 0x90, 0x22, 0x4a, 0xc7, 0xc5, 0x3d, 0x52, 0xaa, 0x66, 0x56, 0x2b,
            0x92, 0xe0, 0x12, 0x3e, 0x38, 0x14, 0x15, 0xbb, 0x40, 0xe2, 0x98, 0x81, 0xcd, 0xdd,
            0xb5, 0x55, 0xf8, 0x0a,
        ],
    },
    OperationSpec {
        id: OperationId::ChannelReceive,
        name: "channel.receive",
        interface: Some(ObjectInterface::Channel),
        required_rights: Rights::RECEIVE,
        signature: "epoch=1;operation=47;object=14;args=user_mut_ptr,u64,zero,zero,zero;result=u64,u64",
        signature_hash: [
            0x63, 0x31, 0xd4, 0x98, 0xa6, 0x89, 0xe6, 0xa2, 0xc9, 0xde, 0x98, 0xa5, 0x7c, 0x3b,
            0x94, 0x36, 0x6c, 0xb4, 0xf7, 0xb8, 0xbd, 0x43, 0x0b, 0xe8, 0xc5, 0xe0, 0x0d, 0xcd,
            0xa1, 0xf2, 0x7a, 0x3a,
        ],
    },
    OperationSpec {
        id: OperationId::RingCreate,
        name: "ring.create",
        interface: Some(ObjectInterface::Process),
        required_rights: Rights::CREATE,
        signature: "epoch=1;operation=48;object=1;args=u32,zero,zero,zero,zero;result=handle,u64",
        signature_hash: [
            0x57, 0x5e, 0x97, 0x9d, 0x4c, 0x88, 0x90, 0xda, 0x7f, 0x25, 0x8c, 0xc3, 0xbd, 0xf6,
            0xcb, 0xd8, 0x20, 0x2f, 0xf7, 0xcb, 0x07, 0x01, 0x59, 0x80, 0x08, 0x89, 0x73, 0x51,
            0xeb, 0x05, 0x6e, 0x6c,
        ],
    },
    OperationSpec {
        id: OperationId::RingRegister,
        name: "ring.register",
        interface: Some(ObjectInterface::SubmissionRing),
        required_rights: Rights::REGISTER,
        signature: "epoch=1;operation=49;object=15;args=u64,u64,u64,zero,zero;result=u64",
        signature_hash: [
            0x1f, 0x14, 0xac, 0x53, 0x22, 0x88, 0x30, 0x3f, 0xd4, 0xff, 0x6e, 0xa4, 0xbf, 0x65,
            0xf5, 0x6d, 0xba, 0x92, 0x19, 0x1b, 0x6c, 0xaf, 0x2c, 0x5a, 0x91, 0x52, 0x88, 0x92,
            0xeb, 0x9c, 0xc9, 0xae,
        ],
    },
    OperationSpec {
        id: OperationId::RingUnregister,
        name: "ring.unregister",
        interface: Some(ObjectInterface::SubmissionRing),
        required_rights: Rights::REGISTER,
        signature: "epoch=1;operation=50;object=15;args=u64,zero,zero,zero,zero;result=status",
        signature_hash: [
            0x09, 0x7f, 0xcb, 0x36, 0x00, 0xd2, 0x1f, 0x4f, 0x0f, 0xba, 0xa6, 0xcb, 0x10, 0x42,
            0x91, 0x7f, 0x5b, 0x31, 0x0c, 0x40, 0x3e, 0x8d, 0xa2, 0x21, 0x8e, 0x96, 0xc5, 0xcf,
            0x41, 0x08, 0x86, 0x54,
        ],
    },
    OperationSpec {
        id: OperationId::RingKick,
        name: "ring.kick",
        interface: Some(ObjectInterface::SubmissionRing),
        required_rights: Rights::SUBMIT,
        signature: "epoch=1;operation=51;object=15;args=u32,zero,zero,zero,zero;result=u32",
        signature_hash: [
            0xf3, 0xdc, 0x5d, 0xde, 0xc5, 0x50, 0x71, 0xdf, 0x21, 0x8f, 0x19, 0x0a, 0xab, 0xc6,
            0xdb, 0xdc, 0x7e, 0x6a, 0xe6, 0x9a, 0x20, 0xfc, 0xd3, 0x7a, 0xe4, 0x16, 0xd2, 0x19,
            0x98, 0x69, 0x7c, 0xdc,
        ],
    },
    OperationSpec {
        id: OperationId::RingCancel,
        name: "ring.cancel",
        interface: Some(ObjectInterface::SubmissionRing),
        required_rights: Rights::CANCEL,
        signature: "epoch=1;operation=52;object=15;args=u64,zero,zero,zero,zero;result=status",
        signature_hash: [
            0xce, 0x9e, 0x9f, 0x39, 0x68, 0x5e, 0x6c, 0xc3, 0x40, 0xa2, 0x37, 0xc4, 0x08, 0x61,
            0xe0, 0xe2, 0x56, 0xcf, 0xcc, 0x49, 0x42, 0x70, 0x30, 0x8e, 0xee, 0xb2, 0xe3, 0xe8,
            0xb9, 0x58, 0x6e, 0x20,
        ],
    },
    OperationSpec {
        id: OperationId::RingWait,
        name: "ring.wait",
        interface: Some(ObjectInterface::SubmissionRing),
        required_rights: Rights::OBSERVE,
        signature: "epoch=1;operation=53;object=15;args=u32,u64,zero,zero,zero;result=u32",
        signature_hash: [
            0x00, 0xb6, 0xf4, 0x61, 0xcd, 0x63, 0x1f, 0x6f, 0x05, 0x68, 0x76, 0x48, 0x98, 0xee,
            0x59, 0x5a, 0xda, 0x84, 0xe8, 0x40, 0x14, 0x4e, 0x85, 0xe9, 0xfe, 0x03, 0xe4, 0x64,
            0xbc, 0x4d, 0x05, 0x9f,
        ],
    },
    OperationSpec {
        id: OperationId::RingQuery,
        name: "ring.query",
        interface: Some(ObjectInterface::SubmissionRing),
        required_rights: Rights::INSPECT,
        signature: "epoch=1;operation=54;object=15;args=user_mut_ptr,zero,zero,zero,zero;result=status",
        signature_hash: [
            0x59, 0x2f, 0x5c, 0x58, 0xd6, 0xc8, 0x18, 0x69, 0xbb, 0x8c, 0xf2, 0x1a, 0x58, 0xb6,
            0x04, 0x39, 0x90, 0xd2, 0x2d, 0x33, 0x68, 0x2d, 0x03, 0x21, 0x0d, 0x46, 0xc2, 0x60,
            0x13, 0x0b, 0x39, 0xe3,
        ],
    },
    OperationSpec {
        id: OperationId::SocketCreate,
        name: "socket.create",
        interface: Some(ObjectInterface::Process),
        required_rights: Rights::CREATE,
        signature: "epoch=1;operation=55;object=1;args=user_const_ptr,zero,zero,zero,zero;result=handle",
        signature_hash: [
            0xb2, 0x3d, 0x67, 0x16, 0x78, 0xd3, 0xb6, 0x3a, 0xa7, 0xc1, 0x29, 0xae, 0x6b, 0xaa,
            0xe5, 0xc5, 0x68, 0x24, 0x1a, 0xbd, 0x9c, 0xcd, 0x82, 0x71, 0xc9, 0x47, 0x39, 0x3f,
            0xea, 0x02, 0x1b, 0x9c,
        ],
    },
    OperationSpec {
        id: OperationId::SocketBind,
        name: "socket.bind",
        interface: Some(ObjectInterface::Socket),
        required_rights: Rights::BIND,
        signature: "epoch=1;operation=56;object=16;args=user_const_ptr,zero,zero,zero,zero;result=status",
        signature_hash: [
            0x45, 0xc0, 0x6e, 0x81, 0x7b, 0x91, 0x29, 0xe7, 0x9c, 0x81, 0xdc, 0x62, 0x2d, 0xbb,
            0x9b, 0xe2, 0x5d, 0x64, 0x3a, 0x68, 0xc7, 0x3f, 0xa2, 0x15, 0x76, 0xf4, 0xee, 0x9e,
            0xde, 0x83, 0x65, 0xa4,
        ],
    },
    OperationSpec {
        id: OperationId::SocketConnect,
        name: "socket.connect",
        interface: Some(ObjectInterface::Socket),
        required_rights: Rights::BIND,
        signature: "epoch=1;operation=57;object=16;args=user_const_ptr,zero,zero,zero,zero;result=status",
        signature_hash: [
            0xc3, 0x11, 0xb7, 0xf3, 0x66, 0x47, 0x90, 0x23, 0x1b, 0x97, 0x1c, 0x76, 0x69, 0x43,
            0x29, 0x92, 0xd6, 0x13, 0x3c, 0x50, 0xa5, 0xaf, 0x45, 0x0b, 0x32, 0xfb, 0xba, 0x5f,
            0x31, 0x2c, 0xa3, 0x2f,
        ],
    },
    OperationSpec {
        id: OperationId::SocketListen,
        name: "socket.listen",
        interface: Some(ObjectInterface::Socket),
        required_rights: Rights::BIND,
        signature: "epoch=1;operation=58;object=16;args=u32,zero,zero,zero,zero;result=status",
        signature_hash: [
            0x1e, 0xf5, 0x83, 0xd3, 0x78, 0x63, 0x6b, 0x8a, 0x92, 0x6e, 0xb4, 0xcf, 0xe2, 0xd7,
            0x69, 0xf5, 0x11, 0x2c, 0x53, 0xd4, 0x45, 0xdb, 0x1a, 0x66, 0xea, 0x88, 0x09, 0x12,
            0x09, 0x69, 0x20, 0xf7,
        ],
    },
    OperationSpec {
        id: OperationId::SocketAccept,
        name: "socket.accept",
        interface: Some(ObjectInterface::Socket),
        required_rights: Rights::CREATE,
        signature: "epoch=1;operation=59;object=16;args=u64,zero,zero,zero,zero;result=handle",
        signature_hash: [
            0x55, 0x22, 0x5a, 0x5c, 0x47, 0xc0, 0xb5, 0x14, 0x4d, 0x67, 0x0d, 0xc9, 0xf8, 0xe9,
            0xc2, 0x00, 0x1d, 0x25, 0x68, 0x5f, 0xed, 0x27, 0xa1, 0x24, 0xce, 0x56, 0x43, 0xe1,
            0x3d, 0x9a, 0x41, 0x34,
        ],
    },
    OperationSpec {
        id: OperationId::SocketSend,
        name: "socket.send",
        interface: Some(ObjectInterface::Socket),
        required_rights: Rights::SEND,
        signature: "epoch=1;operation=60;object=16;args=u64,u64,u64,user_const_ptr,u64;result=u64",
        signature_hash: [
            0x1a, 0x98, 0xfc, 0x37, 0x3e, 0x9e, 0xa9, 0x1d, 0xb7, 0xde, 0x09, 0xcb, 0x2e, 0xab,
            0x9d, 0x92, 0x91, 0x5b, 0x24, 0x59, 0x4d, 0x5e, 0x93, 0x54, 0x82, 0x8a, 0x70, 0x49,
            0xea, 0x48, 0x34, 0xef,
        ],
    },
    OperationSpec {
        id: OperationId::SocketReceive,
        name: "socket.receive",
        interface: Some(ObjectInterface::Socket),
        required_rights: Rights::RECEIVE,
        signature: "epoch=1;operation=61;object=16;args=u64,u64,u64,user_mut_ptr,u64;result=u64",
        signature_hash: [
            0x0c, 0x51, 0xf4, 0x2e, 0x03, 0x27, 0x40, 0xcd, 0x19, 0x1c, 0x8e, 0xad, 0x6d, 0x94,
            0x15, 0x16, 0xcf, 0x98, 0xb2, 0x85, 0x2c, 0xc2, 0x4c, 0xd1, 0x55, 0xea, 0x57, 0x9d,
            0xea, 0xe4, 0x89, 0x79,
        ],
    },
    OperationSpec {
        id: OperationId::SocketShutdown,
        name: "socket.shutdown",
        interface: Some(ObjectInterface::Socket),
        required_rights: Rights::MODIFY,
        signature: "epoch=1;operation=62;object=16;args=u32,zero,zero,zero,zero;result=status",
        signature_hash: [
            0x1c, 0x70, 0xc5, 0xcf, 0x26, 0xa2, 0x60, 0x32, 0x16, 0x1e, 0x8a, 0x82, 0x0a, 0x1a,
            0xaf, 0x33, 0xb8, 0x42, 0x91, 0x55, 0xeb, 0x6b, 0x97, 0x18, 0xd1, 0x25, 0x03, 0x17,
            0x37, 0x4a, 0x83, 0x7e,
        ],
    },
    OperationSpec {
        id: OperationId::SocketQuery,
        name: "socket.query",
        interface: Some(ObjectInterface::Socket),
        required_rights: Rights::INSPECT,
        signature: "epoch=1;operation=63;object=16;args=user_mut_ptr,zero,zero,zero,zero;result=status",
        signature_hash: [
            0x96, 0x4f, 0x0c, 0xf6, 0x82, 0xb4, 0x65, 0x3f, 0x56, 0x6a, 0x6f, 0xc2, 0xac, 0x6b,
            0xec, 0x7a, 0x32, 0x42, 0x52, 0xf5, 0xad, 0xc7, 0xf2, 0x38, 0xda, 0xbb, 0x5a, 0x4a,
            0x8a, 0x78, 0x55, 0x20,
        ],
    },
    OperationSpec {
        id: OperationId::DeviceInvoke,
        name: "device.invoke",
        interface: Some(ObjectInterface::DeviceFunction),
        required_rights: Rights::SUBMIT,
        signature: "epoch=1;operation=64;object=17;args=user_const_ptr,zero,zero,zero,zero;result=u64",
        signature_hash: [
            0xd2, 0xe6, 0x2f, 0xc7, 0xe1, 0xfe, 0x9c, 0x60, 0xba, 0x76, 0x9a, 0x27, 0x1c, 0x3b,
            0xca, 0x8c, 0xa9, 0x6d, 0xa8, 0x44, 0x2c, 0xa9, 0x32, 0xa8, 0x03, 0x0f, 0x28, 0x73,
            0xa2, 0x3d, 0xda, 0x4a,
        ],
    },
    OperationSpec {
        id: OperationId::DeviceQuery,
        name: "device.query",
        interface: Some(ObjectInterface::DeviceFunction),
        required_rights: Rights::INSPECT,
        signature: "epoch=1;operation=65;object=17;args=user_mut_ptr,zero,zero,zero,zero;result=status",
        signature_hash: [
            0x29, 0x99, 0x66, 0x18, 0x02, 0x8e, 0x79, 0x0f, 0x86, 0x4e, 0xef, 0xad, 0x4b, 0x34,
            0xe6, 0x33, 0x01, 0xa0, 0x53, 0x4a, 0xe8, 0x66, 0xdb, 0x26, 0x8a, 0xe5, 0xe4, 0x36,
            0xa1, 0x90, 0x63, 0xa5,
        ],
    },
    OperationSpec {
        id: OperationId::ImageQuery,
        name: "image.query",
        interface: Some(ObjectInterface::Image),
        required_rights: Rights::INSPECT,
        signature: "epoch=1;operation=66;object=5;args=user_mut_ptr,zero,zero,zero,zero;result=status",
        signature_hash: [
            0x35, 0xaa, 0x1b, 0x66, 0x1f, 0x93, 0xe8, 0x46, 0x62, 0x7d, 0x4e, 0x86, 0x4c, 0xad,
            0x09, 0x4e, 0x0e, 0x18, 0xf4, 0x3a, 0xfb, 0xb8, 0x15, 0x28, 0x63, 0x7f, 0x58, 0xe4,
            0x40, 0x0f, 0x99, 0x2d,
        ],
    },
    OperationSpec {
        id: OperationId::ThreadExit,
        name: "thread.exit",
        interface: Some(ObjectInterface::Process),
        required_rights: Rights::NONE,
        signature: "epoch=1;operation=67;object=1;args=u32,zero,zero,zero,zero;result=noreturn",
        signature_hash: [
            0x6d, 0xa6, 0x36, 0xed, 0x1a, 0xe4, 0xac, 0x6c, 0x01, 0xef, 0xcd, 0xba, 0xdf, 0x86,
            0xa4, 0x9e, 0x8c, 0x6f, 0x85, 0x71, 0x63, 0x5c, 0x3c, 0xcc, 0x92, 0x02, 0x9b, 0x25,
            0xeb, 0xb5, 0xa4, 0x23,
        ],
    },
    OperationSpec {
        id: OperationId::ThreadYield,
        name: "thread.yield",
        interface: Some(ObjectInterface::Process),
        required_rights: Rights::NONE,
        signature: "epoch=1;operation=68;object=1;args=zero,zero,zero,zero,zero;result=status",
        signature_hash: [
            0x95, 0x1e, 0xe0, 0x75, 0x1f, 0xc3, 0x7b, 0x61, 0x52, 0x85, 0x10, 0xc3, 0x48, 0xf5,
            0x1b, 0xc8, 0x60, 0x99, 0xd5, 0x3a, 0xd3, 0xa7, 0x19, 0xc8, 0x01, 0x9d, 0x26, 0x66,
            0x4a, 0xda, 0x47, 0x25,
        ],
    },
    OperationSpec {
        id: OperationId::MemoryRevoke,
        name: "memory.revoke",
        interface: Some(ObjectInterface::MemoryObject),
        required_rights: Rights::MODIFY,
        signature: "epoch=1;operation=69;object=11;args=zero,zero,zero,zero,zero;result=u64",
        signature_hash: [
            0x74, 0xaf, 0xac, 0xff, 0x77, 0x6a, 0x09, 0x40, 0x46, 0x5e, 0xe6, 0x32, 0x0c, 0x84,
            0xba, 0x8c, 0x62, 0xe0, 0xc7, 0x1b, 0x30, 0xcf, 0xd7, 0x6c, 0x45, 0x3a, 0x5b, 0xa6,
            0xd9, 0x6e, 0xcc, 0xcf,
        ],
    },
    OperationSpec {
        id: OperationId::MemoryStatistics,
        name: "memory.statistics",
        interface: Some(ObjectInterface::MemoryObject),
        required_rights: Rights::INSPECT,
        signature: "epoch=1;operation=70;object=11;args=user_mut_ptr,zero,zero,zero,zero;result=status",
        signature_hash: [
            0xf1, 0x47, 0xf5, 0x23, 0x94, 0x47, 0x37, 0x0e, 0x64, 0x13, 0xa7, 0xb9, 0x2f, 0xd8,
            0xa4, 0xdc, 0x9f, 0x04, 0x88, 0xf7, 0xc7, 0xe6, 0x63, 0x2a, 0x8c, 0xfb, 0x7f, 0x71,
            0xb1, 0x5a, 0x64, 0x71,
        ],
    },
];

pub fn operation(id: OperationId) -> Option<&'static OperationSpec> {
    OPERATIONS.iter().find(|spec| spec.id == id)
}

pub fn operation_by_id(id: u32) -> Option<&'static OperationSpec> {
    OPERATIONS.iter().find(|spec| spec.id as u32 == id)
}
