//! 动态 ELM 使用的内核直接符号目录。
//!
//! 本模块只扫描链接器收集的常驻描述符并执行解析。它不转发调用、不保存授权令牌，
//! 也不把子系统能力包装成 ELM provider。装载器完成解析后，模块直接调用写入槽位的
//! Rust 地址。

use core::mem::{align_of, size_of};
use core::slice;

use elm_model::{ElmEbiImportDecl, kernel_symbol_interface_abi_hash, sha256};
use kernel_symbols::{
    KERNEL_SYMBOL_KIND_FUNCTION, KERNEL_SYMBOL_KIND_METHOD, KERNEL_SYMBOL_KIND_STATIC,
    KernelSymbolDescriptorV1,
};

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
                && (sha256(symbol.rust_abi.as_bytes()) == rust_abi_hash
                    || kernel_symbol_interface_abi_hash(
                        symbol.rust_abi.as_bytes(),
                        symbol.interface_hash,
                    ) == rust_abi_hash)
        })
    })
}

fn symbol_abi_hash(symbol: &KernelSymbolDescriptorV1, exact: bool) -> [u8; 32] {
    if exact {
        kernel_symbol_interface_abi_hash(symbol.rust_abi.as_bytes(), symbol.interface_hash)
    } else {
        sha256(symbol.rust_abi.as_bytes())
    }
}

#[cfg(feature = "kernel-tests")]
pub(crate) fn test_catalog_len() -> usize {
    catalog().map_or(0, <[_]>::len)
}
