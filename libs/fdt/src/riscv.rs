//! RISC-V CPU Device Tree binding 解码。
//!
//! 新版 binding 使用 `riscv,isa-base` 和 `riscv,isa-extensions` 分开描述
//! ISA，老固件则只提供已废弃的 `riscv,isa`。本模块优先解码新形式，
//! 仅在两个新属性均缺失时回退到 legacy 字符串，并统一暴露扩展
//! 查询、MMU 类型和 cache-block 大小。

use crate::{Node, PropertyError, StringList};

const PROP_ISA_BASE: &str = "riscv,isa-base";
const PROP_ISA_EXTENSIONS: &str = "riscv,isa-extensions";
const PROP_LEGACY_ISA: &str = "riscv,isa";
const PROP_MMU_TYPE: &str = "mmu-type";
const PROP_CBOM_BLOCK_SIZE: &str = "riscv,cbom-block-size";
const PROP_CBOZ_BLOCK_SIZE: &str = "riscv,cboz-block-size";
const PROP_CBOP_BLOCK_SIZE: &str = "riscv,cbop-block-size";

/// ISA 描述的固件来源。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RiscvIsaSource {
    /// 规范的 `riscv,isa-base` + `riscv,isa-extensions`。
    Split,
    /// 已废弃但仍需兼容的 `riscv,isa`。
    Legacy,
}

/// RISC-V CPU binding 解码错误。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RiscvCpuError {
    /// 缺少启动所需的必选属性。
    MissingRequired { property: &'static str },
    /// 新 ISA binding 的两个属性只出现了一个。
    IncompleteIsaPair,
    /// 属性不符合 binding 声明的基本类型。
    InvalidProperty {
        property: &'static str,
        error: PropertyError,
    },
    /// ISA base 不是规范的 `rv32i/e` 或 `rv64i/e`。
    InvalidIsaBase,
    /// 拆分形式中的扩展名不是非空小写 ASCII 名称。
    InvalidIsaExtension,
}

#[derive(Clone, Debug)]
enum RiscvIsaExtensions<'a> {
    Split(StringList<'a>),
    Legacy(&'a str),
}

/// 已按 RISC-V CPU binding 校验的借用视图。
#[derive(Clone, Debug)]
pub struct RiscvCpuBinding<'a> {
    isa_base: &'a str,
    isa_extensions: RiscvIsaExtensions<'a>,
    isa_source: RiscvIsaSource,
    mmu_type: &'a str,
    cbom_block_size: Option<u32>,
    cboz_block_size: Option<u32>,
    cbop_block_size: Option<u32>,
}

impl<'a> RiscvCpuBinding<'a> {
    /// 解码一个 `device_type = "cpu"` 节点的 RISC-V 标准属性。
    pub fn parse(node: Node<'a>) -> Result<Self, RiscvCpuError> {
        let base_property = node.property(PROP_ISA_BASE);
        let extensions_property = node.property(PROP_ISA_EXTENSIONS);
        let (isa_base, isa_extensions, isa_source) = match (base_property, extensions_property) {
            (Some(base), Some(extensions)) => {
                let isa_base = base
                    .as_str()
                    .map_err(|error| RiscvCpuError::InvalidProperty {
                        property: PROP_ISA_BASE,
                        error,
                    })?;
                validate_isa_base(isa_base)?;
                let isa_extensions = extensions.as_string_list().map_err(|error| {
                    RiscvCpuError::InvalidProperty {
                        property: PROP_ISA_EXTENSIONS,
                        error,
                    }
                })?;
                validate_split_extensions(isa_extensions.clone())?;
                (
                    isa_base,
                    RiscvIsaExtensions::Split(isa_extensions),
                    RiscvIsaSource::Split,
                )
            }
            (None, None) => {
                let property =
                    node.property(PROP_LEGACY_ISA)
                        .ok_or(RiscvCpuError::MissingRequired {
                            property: PROP_LEGACY_ISA,
                        })?;
                let legacy = property
                    .as_str()
                    .map_err(|error| RiscvCpuError::InvalidProperty {
                        property: PROP_LEGACY_ISA,
                        error,
                    })?;
                let isa_base = legacy_isa_base(legacy).ok_or(RiscvCpuError::InvalidIsaBase)?;
                (
                    isa_base,
                    RiscvIsaExtensions::Legacy(legacy),
                    RiscvIsaSource::Legacy,
                )
            }
            _ => return Err(RiscvCpuError::IncompleteIsaPair),
        };

        let mmu_type = required_string(node, PROP_MMU_TYPE)?;
        Ok(Self {
            isa_base,
            isa_extensions,
            isa_source,
            mmu_type,
            cbom_block_size: optional_u32(node, PROP_CBOM_BLOCK_SIZE)?,
            cboz_block_size: optional_u32(node, PROP_CBOZ_BLOCK_SIZE)?,
            cbop_block_size: optional_u32(node, PROP_CBOP_BLOCK_SIZE)?,
        })
    }

    /// 返回 `rv32i/e` 或 `rv64i/e` ISA base。
    #[inline]
    pub const fn isa_base(&self) -> &'a str {
        self.isa_base
    }

    /// 返回本次实际采用的 ISA 属性形式。
    #[inline]
    pub const fn isa_source(&self) -> RiscvIsaSource {
        self.isa_source
    }

    /// 查询一个不带版本后缀的标准 ISA 扩展名。
    pub fn has_isa_extension(&self, extension: &str) -> bool {
        match &self.isa_extensions {
            RiscvIsaExtensions::Split(extensions) => {
                extensions.clone().any(|candidate| candidate == extension)
            }
            RiscvIsaExtensions::Legacy(isa) => legacy_has_extension(isa, extension),
        }
    }

    /// 返回 `mmu-type` 原始规范字符串。
    #[inline]
    pub const fn mmu_type(&self) -> &'a str {
        self.mmu_type
    }

    /// `Zicbom` 管理块大小。
    #[inline]
    pub const fn cbom_block_size(&self) -> Option<u32> {
        self.cbom_block_size
    }

    /// `Zicboz` 清零块大小。
    #[inline]
    pub const fn cboz_block_size(&self) -> Option<u32> {
        self.cboz_block_size
    }

    /// `Zicbop` 预取块大小。
    #[inline]
    pub const fn cbop_block_size(&self) -> Option<u32> {
        self.cbop_block_size
    }
}

fn required_string<'a>(node: Node<'a>, property: &'static str) -> Result<&'a str, RiscvCpuError> {
    node.property(property)
        .ok_or(RiscvCpuError::MissingRequired { property })?
        .as_str()
        .map_err(|error| RiscvCpuError::InvalidProperty { property, error })
}

fn optional_u32(node: Node<'_>, property: &'static str) -> Result<Option<u32>, RiscvCpuError> {
    node.property(property)
        .map(|value| {
            value
                .as_u32()
                .map_err(|error| RiscvCpuError::InvalidProperty { property, error })
        })
        .transpose()
}

fn validate_isa_base(base: &str) -> Result<(), RiscvCpuError> {
    if matches!(base, "rv32i" | "rv32e" | "rv64i" | "rv64e") {
        Ok(())
    } else {
        Err(RiscvCpuError::InvalidIsaBase)
    }
}

fn validate_split_extensions(extensions: StringList<'_>) -> Result<(), RiscvCpuError> {
    for extension in extensions {
        let mut bytes = extension.bytes();
        if !bytes.next().is_some_and(|byte| byte.is_ascii_lowercase())
            || !bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        {
            return Err(RiscvCpuError::InvalidIsaExtension);
        }
    }
    Ok(())
}

fn legacy_isa_base(isa: &str) -> Option<&str> {
    let prefix = isa.get(..5)?;
    matches!(prefix, "rv32i" | "rv32e" | "rv64i" | "rv64e").then_some(prefix)
}

fn legacy_has_extension(isa: &str, extension: &str) -> bool {
    if extension.len() == 1 {
        let expected = extension.as_bytes()[0];
        let base = isa
            .split_once('_')
            .map_or(isa, |(single_letter, _)| single_letter);
        return base
            .as_bytes()
            .get(4..)
            .is_some_and(|extensions| extensions.contains(&expected));
    }

    isa.split('_').skip(1).any(|candidate| {
        let Some(suffix) = candidate.strip_prefix(extension) else {
            return false;
        };
        suffix.is_empty() || suffix.as_bytes().first().is_some_and(u8::is_ascii_digit)
    })
}
