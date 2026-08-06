//! SOYO v1 文件格式的机器可识别值与资源上限。

use core::ops::BitOr;

pub const SOYO_MAGIC: [u8; 4] = *b"soyo";
pub const FORMAT_VERSION: u16 = 1;
pub const PAGE_SIZE: u64 = 4096;
pub const MAX_FILE_SIZE: u64 = 256 * 1024 * 1024;
pub const MAX_IMAGE_SIZE: u64 = 1024 * 1024 * 1024;
pub const MAX_DIRECTORY_ENTRIES: u32 = 64;
pub const MAX_STRING_BYTES: usize = 1024 * 1024;
pub const MAX_SEGMENTS: u32 = 32;
pub const MAX_IMPORTS: u32 = 256;
pub const MAX_CAPABILITIES: u32 = 64;
pub const MAX_RELOCATIONS: u32 = 65_536;
pub const MAX_TLS_SIZE: u64 = 16 * 1024 * 1024;

macro_rules! wire_flags {
    ($name:ident, $bits:ty, { $($constant:ident = $value:expr),+ $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub struct $name($bits);

        impl $name {
            $(pub const $constant: Self = Self($value);)+

            pub const fn from_bits(bits: $bits) -> Self {
                Self(bits)
            }

            pub const fn bits(self) -> $bits {
                self.0
            }

            pub const fn contains(self, other: Self) -> bool {
                self.0 & other.0 == other.0
            }
        }

        impl BitOr for $name {
            type Output = Self;

            fn bitor(self, rhs: Self) -> Self::Output {
                Self(self.0 | rhs.0)
            }
        }
    };
}

wire_flags!(FeatureFlags, u64, {
    STATIC_TLS = 1 << 0,
    INIT_FINI_ARRAY = 1 << 1,
    KNOWN = 0b11,
});

wire_flags!(DirectoryFlags, u16, {
    REQUIRED = 1 << 0,
    KNOWN = 1 << 0,
});

wire_flags!(ImportFlags, u32, {
    REQUIRED = 1 << 0,
    OPTIONAL = 1 << 1,
    KNOWN = 0b11,
});

wire_flags!(CapabilityFlags, u16, {
    REQUIRED = 1 << 0,
    OPTIONAL = 1 << 1,
    KNOWN = 0b11,
});

wire_flags!(SegmentPermissions, u16, {
    READ = 1 << 0,
    WRITE = 1 << 1,
    EXECUTE = 1 << 2,
    KNOWN = 0b111,
});

wire_flags!(RuntimeFlags, u64, {
    RUN_INIT_ARRAY = 1 << 0,
    RUN_FINI_ARRAY = 1 << 1,
    KNOWN = 0b11,
});

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum ArtifactKind {
    Executable = 1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum HashAlgorithm {
    Sha256 = 1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum TableType {
    String = 1,
    ImageSegment = 2,
    AbiImport = 3,
    CapabilityRequirement = 4,
    Relocation = 5,
    RuntimeInfo = 6,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum SegmentKind {
    Code = 1,
    Rodata = 2,
    Data = 3,
    Bss = 4,
    TlsTemplate = 5,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum RelocationKind {
    ImageBase64 = 1,
    SegmentBase64 = 2,
}
