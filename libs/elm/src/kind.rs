//! ELM 单元种类的稳定线类型。

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum ElmKind {
    Manager = 1,
    Service = 2,
    Driver = 3,
    Extension = 4,
    Filesystem = 5,
    Network = 6,
    Debug = 7,
    Other = 8,
}

impl ElmKind {
    pub const fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            1 => Some(Self::Manager),
            2 => Some(Self::Service),
            3 => Some(Self::Driver),
            4 => Some(Self::Extension),
            5 => Some(Self::Filesystem),
            6 => Some(Self::Network),
            7 => Some(Self::Debug),
            8 => Some(Self::Other),
            _ => None,
        }
    }

    pub const fn as_raw(self) -> u32 {
        self as u32
    }
}
