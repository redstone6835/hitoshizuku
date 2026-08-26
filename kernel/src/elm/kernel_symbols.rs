//! 动态 ELM 使用的内核直接符号目录。
//!
//! 本模块只扫描链接器收集的常驻描述符并执行解析。它不转发调用、不保存授权令牌，
//! 也不把子系统能力包装成 ELM provider。装载器完成解析后，模块直接调用写入槽位的
//! Rust 地址。

use alloc::vec::Vec;
use core::mem::{align_of, size_of};
use core::slice;

use elm_loader::{ElmHostOps, ElmHostSymbol, ElmHostSymbolError, ElmLoaderTarget};
use elm_model::{ELM_API_FEATURES_V1, ELM_API_VERSION_V1, ElmEbiArch, ElmEbiImportDecl, sha256};
use kernel_symbols::{
    KERNEL_SYMBOL_KIND_FUNCTION, KERNEL_SYMBOL_KIND_METHOD, KERNEL_SYMBOL_KIND_STATIC,
    KernelSymbolDescriptorV1,
};

/// 当前直接内核符号 Profile 使用的桥接 ABI 版本。
pub(crate) const KERNEL_API_BRIDGE_ABI_VERSION: u16 = 1;

unsafe extern "C" {
    static __elm_kernel_symbols_start: u8;
    static __elm_kernel_symbols_end: u8;
}

/// 内核符号目录校验或解析错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KernelSymbolError {
    MalformedCatalog,
    DuplicateIdentity,
    NotFound,
    CapabilityDenied,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ResolvedKernelSymbol {
    pub kind: u8,
    pub version: u32,
    pub capabilities: u64,
    pub flags: u32,
    pub retained_argument_mask: u64,
    pub rust_abi_hash: [u8; 32],
    pub address: usize,
}

/// 把内核链接期符号目录暴露给容器无关 Loader 的宿主适配器。
pub(crate) struct KernelSymbolHost {
    arch: ElmEbiArch,
}

impl KernelSymbolHost {
    pub(crate) const fn new(arch: ElmEbiArch) -> Self {
        Self { arch }
    }
}

impl ElmHostOps for KernelSymbolHost {
    fn target(&self) -> ElmLoaderTarget<'_> {
        ElmLoaderTarget {
            arch: self.arch,
            core_version: 1,
            elmapi_versions: &[ELM_API_VERSION_V1],
            elmapi_features: ELM_API_FEATURES_V1,
        }
    }

    fn resolve_kernel_symbol(
        &self,
        import: &ElmEbiImportDecl,
    ) -> Result<ElmHostSymbol, ElmHostSymbolError> {
        resolve(import, ::kernel_symbols::capability::ALL)
            .map(|symbol| ElmHostSymbol {
                kind: symbol.kind,
                version: symbol.version,
                capabilities: symbol.capabilities,
                flags: symbol.flags,
                retained_argument_mask: symbol.retained_argument_mask,
                rust_abi_hash: symbol.rust_abi_hash,
                address: symbol.address,
            })
            .map_err(|error| match error {
                KernelSymbolError::NotFound => ElmHostSymbolError::NotFound,
                KernelSymbolError::CapabilityDenied => ElmHostSymbolError::CapabilityDenied,
                KernelSymbolError::MalformedCatalog | KernelSymbolError::DuplicateIdentity => {
                    ElmHostSymbolError::MalformedCatalog
                }
            })
    }
}

fn catalog() -> Result<&'static [KernelSymbolDescriptorV1], KernelSymbolError> {
    let start = core::ptr::addr_of!(__elm_kernel_symbols_start) as usize;
    let end = core::ptr::addr_of!(__elm_kernel_symbols_end) as usize;
    let bytes = end
        .checked_sub(start)
        .ok_or(KernelSymbolError::MalformedCatalog)?;
    if start % align_of::<KernelSymbolDescriptorV1>() != 0
        || bytes % size_of::<KernelSymbolDescriptorV1>() != 0
    {
        return Err(KernelSymbolError::MalformedCatalog);
    }
    let count = bytes / size_of::<KernelSymbolDescriptorV1>();
    // Safety: 起止符号由四份内核链接脚本围住同一个描述符输入段；上面已经验证顺序、
    // 对齐和完整元素长度，且该只读链接区与内核镜像同寿命。
    Ok(unsafe { slice::from_raw_parts(start as *const KernelSymbolDescriptorV1, count) })
}

/// 校验链接期目录的结构、唯一性和地址不变量。
pub(crate) fn validate_catalog() -> Result<(), KernelSymbolError> {
    let symbols = catalog()?;
    for (index, symbol) in symbols.iter().enumerate() {
        if !symbol.validate()
            || symbols[..index].iter().any(|previous| {
                previous.link_name == symbol.link_name
                    || previous.api_path == symbol.api_path
                        && previous.contract == symbol.contract
                        && previous.version == symbol.version
            })
        {
            return if symbol.validate() {
                Err(KernelSymbolError::DuplicateIdentity)
            } else {
                Err(KernelSymbolError::MalformedCatalog)
            };
        }
    }
    Ok(())
}

/// 计算当前链接目录的规范 API Profile 摘要。
pub(crate) fn catalog_profile_hash() -> Result<[u8; 32], KernelSymbolError> {
    const DOMAIN: &[u8] = b"ELM-KERNEL-API-PROFILE-V2\0";
    const ABI_MODE: &[u8] = b"exact-rust";

    let mut symbols = catalog()?.iter().collect::<Vec<_>>();
    symbols.sort_by(|left, right| {
        left.api_path
            .cmp(right.api_path)
            .then_with(|| left.contract.cmp(right.contract))
            .then_with(|| left.version.cmp(&right.version))
    });
    let mut hash = elm_model::Sha256::new();
    hash.update(DOMAIN);
    hash.update(&KERNEL_API_BRIDGE_ABI_VERSION.to_le_bytes());
    hash.update(&kernel_symbols::KERNEL_INTERFACE_SOURCE_SHA256);
    hash.update(&(symbols.len() as u64).to_le_bytes());
    for symbol in symbols {
        hash.update(&[symbol.kind]);
        hash.update(&symbol.flags.to_le_bytes());
        hash.update(&symbol.version.to_le_bytes());
        hash.update(&symbol.capabilities.to_le_bytes());
        hash.update(&symbol.retained_argument_mask.to_le_bytes());
        hash_profile_field(&mut hash, symbol.api_path.as_bytes());
        hash_profile_field(&mut hash, symbol.item_path.as_bytes());
        hash_profile_field(&mut hash, symbol.contract.as_bytes());
        hash_profile_field(&mut hash, symbol.rust_abi.as_bytes());
        hash_profile_field(&mut hash, ABI_MODE);
    }
    Ok(hash.finish())
}

fn hash_profile_field(hash: &mut elm_model::Sha256, value: &[u8]) {
    hash.update(&(value.len() as u64).to_le_bytes());
    hash.update(value);
}

/// 按 EBI 导入声明解析一个真实内核函数、方法或静态对象地址。
pub(crate) fn resolve(
    import: &ElmEbiImportDecl,
    allowed_capabilities: u64,
) -> Result<ResolvedKernelSymbol, KernelSymbolError> {
    if !import.is_kernel_symbol() {
        return Err(KernelSymbolError::NotFound);
    }
    let symbols = catalog()?;
    let mut selected = None;
    let mut denied = false;
    let expected_static = import.is_kernel_static();
    for symbol in symbols.iter().filter(|symbol| {
        (expected_static && symbol.kind == KERNEL_SYMBOL_KIND_STATIC
            || !expected_static
                && matches!(
                    symbol.kind,
                    KERNEL_SYMBOL_KIND_FUNCTION | KERNEL_SYMBOL_KIND_METHOD
                ))
            && symbol.api_path == import.name
            && symbol.contract == import.contract.as_str()
            && import.accepts_version(symbol.version)
            && symbol_abi_hash(symbol, import.is_exact_rust_api()) == import.rust_abi_hash
    }) {
        if symbol.capabilities & !allowed_capabilities != 0 {
            denied = true;
            continue;
        }
        if selected
            .is_none_or(|current: &KernelSymbolDescriptorV1| symbol.version > current.version)
        {
            selected = Some(symbol);
        }
    }
    let Some(symbol) = selected else {
        return Err(if denied {
            KernelSymbolError::CapabilityDenied
        } else {
            KernelSymbolError::NotFound
        });
    };
    Ok(ResolvedKernelSymbol {
        kind: symbol.kind,
        version: symbol.version,
        capabilities: symbol.capabilities,
        flags: symbol.flags,
        retained_argument_mask: symbol.retained_argument_mask,
        rust_abi_hash: import.rust_abi_hash,
        address: symbol.address as usize,
    })
}

/// 校验运行时账本中的已解析地址仍指向当前常驻目录条目。
pub(crate) fn matches_resolved(
    name: &str,
    contract: &str,
    version: u32,
    capabilities: u64,
    rust_abi_hash: [u8; 32],
    address: usize,
) -> bool {
    catalog().is_ok_and(|symbols| {
        symbols.iter().any(|symbol| {
            symbol.api_path == name
                && symbol.contract == contract
                && symbol.version == version
                && symbol.capabilities == capabilities
                && symbol.address as usize == address
                && sha256(symbol.rust_abi.as_bytes()) == rust_abi_hash
        })
    })
}

fn symbol_abi_hash(symbol: &KernelSymbolDescriptorV1, _exact: bool) -> [u8; 32] {
    sha256(symbol.rust_abi.as_bytes())
}

#[cfg(feature = "kernel-tests")]
pub(crate) fn test_catalog_len() -> usize {
    catalog().map_or(0, <[_]>::len)
}
