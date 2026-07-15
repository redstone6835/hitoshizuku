#![no_std]
#![warn(missing_docs)]

//! ELM 的容器无关装载预检器。
//!
//! 本 crate 只消费已经由某个投影器生成并验证的 EBI 镜像。它不认识 EKI、SOYO、ELF、
//! VFS 或具体内存管理器，也不执行模块代码。宿主通过 [`ElmHostOps`] 暴露目标信息和内核
//! 符号解析能力；预检器据此完成架构、ELM API、直接内核导入及可选导入的确定性协商。
//!
//! 实际段映射、重定位、W^X、故障边界和生命周期提交仍由宿主执行，但必须消费这里产生的
//! [`ElmLoadPlan`]，避免不同容器或不同装载入口各自实现一套兼容规则。

extern crate alloc;

use alloc::vec::Vec;

use elm::{ElmEbiArch, ElmEbiImage, ElmEbiImportDecl, ElmEbiLoadStatus, ElmEbiUnit};

/// 当前独立装载器与宿主之间的协议版本。
pub const ELM_LOADER_HOST_ABI_V1: u16 = 1;

/// 宿主能够提供给一个 EBI 单元的目标与 ELM API 能力。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmLoaderTarget<'a> {
    /// 当前内核运行的目标架构。
    pub arch: ElmEbiArch,
    /// 当前 ELM Core 的协议版本。
    pub core_version: u16,
    /// 宿主支持的 ELM API 版本，必须严格递增且不包含零。
    pub elmapi_versions: &'a [u16],
    /// 宿主当前 ELM API 根表公开的功能位。
    pub elmapi_features: u64,
}

impl ElmLoaderTarget<'_> {
    fn validate(self) -> Result<(), ElmEbiLoadStatus> {
        if self.core_version == 0
            || self.elmapi_versions.is_empty()
            || self.elmapi_versions.iter().any(|version| *version == 0)
            || self
                .elmapi_versions
                .windows(2)
                .any(|versions| versions[0] >= versions[1])
        {
            return Err(ElmEbiLoadStatus::UnsupportedAbi);
        }
        Ok(())
    }
}

/// 宿主符号目录解析出的一个常驻内核符号。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElmHostSymbol {
    /// 符号种类，取值来自 `kernel-symbols` 的稳定目录协议。
    pub kind: u8,
    /// 实际选择的符号版本。
    pub version: u32,
    /// 调用该符号需要的能力集合。
    pub capabilities: u64,
    /// 符号目录描述标志。
    pub flags: u32,
    /// 会被常驻内核长期保存的参数位置位图。
    pub retained_argument_mask: u64,
    /// 与导入声明匹配的规范 ABI 摘要。
    pub rust_abi_hash: [u8; 32],
    /// 常驻内核中的真实地址。
    pub address: usize,
}

/// 宿主解析内核符号时可区分的稳定失败原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElmHostSymbolError {
    /// 当前符号目录中不存在匹配项。
    NotFound,
    /// 符号存在，但当前单元没有所需能力。
    CapabilityDenied,
    /// 宿主符号目录自身损坏或存在歧义。
    MalformedCatalog,
}

/// ELM Loader 向具体内核请求的最小宿主操作集合。
pub trait ElmHostOps {
    /// 返回当前目标和 ELM API 能力。
    fn target(&self) -> ElmLoaderTarget<'_>;

    /// 按完整 EBI 导入声明解析一个常驻内核符号。
    fn resolve_kernel_symbol(
        &self,
        import: &ElmEbiImportDecl,
    ) -> Result<ElmHostSymbol, ElmHostSymbolError>;
}

/// 一个导入经过统一预检后的装载动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElmPreparedImport {
    /// 由宿主符号目录解析出的直接内核符号。
    Kernel(ElmHostSymbol),
    /// 可选内核符号不存在，重定位槽必须写零。
    OptionalKernelMissing,
    /// 该项是 ELM API 或 ELM 间导入，交由 elm-mgr 的关系图继续解析。
    ManagedOrModule,
}

/// 一个 EBI 镜像通过容器无关预检后得到的不可变装载计划。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElmLoadPlan {
    /// ELM API 协商结果；镜像未声明 API 根时为空。
    pub selected_elmapi: Option<u16>,
    /// 与 EBI import 表严格同序的解析结果。
    pub imports: Vec<ElmPreparedImport>,
    /// 所有已解析内核符号能力的并集。
    pub kernel_symbol_capabilities: u64,
    /// 镜像是否包含要求同一 Rust 工具链的精确 Rust ABI 导入。
    pub requires_exact_rust_abi: bool,
}

/// 对一个已经投影出的 EBI 镜像执行统一装载预检。
pub fn prepare_ebi_load(
    image: &ElmEbiImage,
    host: &impl ElmHostOps,
) -> Result<ElmLoadPlan, ElmEbiLoadStatus> {
    let target = host.target();
    target.validate()?;
    image.validate(target.arch)?;
    prepare_ebi_unit(&image.unit, host)
}

/// 对一个已经完成容器级校验的 EBI 单元执行目标与导入预检。
///
/// 投影器或调用方必须先校验镜像段、重定位和完整性；本入口用于 elm-mgr 在建立 cell 前
/// 对关系与直接符号进行统一协商。
pub fn prepare_ebi_unit(
    unit: &ElmEbiUnit,
    host: &impl ElmHostOps,
) -> Result<ElmLoadPlan, ElmEbiLoadStatus> {
    let target = host.target();
    target.validate()?;
    unit.validate(target.arch)?;
    if unit.target.min_core_version > target.core_version {
        return Err(ElmEbiLoadStatus::UnsupportedAbi);
    }

    let selected_elmapi = match unit.api_compatibility.as_ref() {
        Some(compatibility) => {
            if compatibility.required_features & !target.elmapi_features != 0 {
                return Err(ElmEbiLoadStatus::UnsupportedAbi);
            }
            Some(
                compatibility
                    .select_highest_common(target.elmapi_versions)
                    .ok_or(ElmEbiLoadStatus::UnsupportedAbi)?,
            )
        }
        None => None,
    };

    let mut imports = Vec::new();
    imports
        .try_reserve_exact(unit.imports.len())
        .map_err(|_| ElmEbiLoadStatus::RuntimeRejected)?;
    let mut capabilities = 0u64;
    let mut requires_exact_rust_abi = false;
    for (index, import) in unit.imports.iter().enumerate() {
        let elmapi_root = unit
            .api_compatibility
            .as_ref()
            .is_some_and(|compatibility| compatibility.root_import_index == index as u32);
        if import.is_kernel_symbol() {
            if elmapi_root {
                return Err(ElmEbiLoadStatus::InvalidTarget);
            }
            match host.resolve_kernel_symbol(import) {
                Ok(symbol) => {
                    capabilities |= symbol.capabilities;
                    requires_exact_rust_abi |= import.is_exact_rust_api();
                    imports.push(ElmPreparedImport::Kernel(symbol));
                }
                Err(ElmHostSymbolError::NotFound) if import.is_optional() => {
                    imports.push(ElmPreparedImport::OptionalKernelMissing);
                }
                Err(ElmHostSymbolError::NotFound | ElmHostSymbolError::CapabilityDenied) => {
                    return Err(ElmEbiLoadStatus::RuntimeRejected);
                }
                Err(ElmHostSymbolError::MalformedCatalog) => {
                    return Err(ElmEbiLoadStatus::InvalidUnit);
                }
            }
        } else {
            imports.push(ElmPreparedImport::ManagedOrModule);
        }
    }

    Ok(ElmLoadPlan {
        selected_elmapi,
        imports,
        kernel_symbol_capabilities: capabilities,
        requires_exact_rust_abi,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use elm::{
        ELM_EBI_IMPORT_FLAG_EXACT_RUST_API, ELM_EBI_IMPORT_FLAG_KERNEL_SYMBOL,
        ELM_EBI_IMPORT_FLAG_OPTIONAL, ElmEbiTarget, ElmEbiUnit, ElmKind, ElmManifest, ElmName,
        ElmVersion, sha256,
    };

    struct Host;

    impl ElmHostOps for Host {
        fn target(&self) -> ElmLoaderTarget<'_> {
            ElmLoaderTarget {
                arch: ElmEbiArch::Riscv64,
                core_version: 1,
                elmapi_versions: &[1],
                elmapi_features: u64::MAX,
            }
        }

        fn resolve_kernel_symbol(
            &self,
            import: &ElmEbiImportDecl,
        ) -> Result<ElmHostSymbol, ElmHostSymbolError> {
            if import.name == "allocator.missing" {
                return Err(ElmHostSymbolError::NotFound);
            }
            Ok(ElmHostSymbol {
                kind: 1,
                version: 1,
                capabilities: 2,
                flags: 0,
                retained_argument_mask: 0,
                rust_abi_hash: import.rust_abi_hash,
                address: 0x1000,
            })
        }
    }

    fn image(imports: impl IntoIterator<Item = ElmEbiImportDecl>) -> ElmEbiImage {
        let manifest = ElmManifest::new(
            ElmName::new("loader-test").unwrap(),
            ElmVersion::new("0.1.0").unwrap(),
            ElmKind::Service,
        );
        let mut unit = ElmEbiUnit::new(manifest, ElmEbiTarget::new(ElmEbiArch::Riscv64));
        for import in imports {
            unit = unit.with_import(import);
        }
        ElmEbiImage::new(unit)
    }

    #[test]
    fn preflight_resolves_kernel_symbols_and_preserves_order() {
        let abi = sha256(b"fn(usize)->usize");
        let present = ElmEbiImportDecl::new(
            "allocator.present",
            "kernel.allocator.present@1",
            1,
            ELM_EBI_IMPORT_FLAG_KERNEL_SYMBOL | ELM_EBI_IMPORT_FLAG_EXACT_RUST_API,
            abi,
        )
        .unwrap();
        let missing = ElmEbiImportDecl::new(
            "allocator.missing",
            "kernel.allocator.missing@1",
            1,
            ELM_EBI_IMPORT_FLAG_KERNEL_SYMBOL | ELM_EBI_IMPORT_FLAG_OPTIONAL,
            abi,
        )
        .unwrap();
        let image = image([present, missing]);
        let plan = prepare_ebi_unit(&image.unit, &Host).unwrap();
        assert!(matches!(plan.imports[0], ElmPreparedImport::Kernel(_)));
        assert_eq!(plan.imports[1], ElmPreparedImport::OptionalKernelMissing);
        assert_eq!(plan.kernel_symbol_capabilities, 2);
        assert!(plan.requires_exact_rust_abi);
    }
}
