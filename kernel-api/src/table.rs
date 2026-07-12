use elm::ELM_API_VERSION_V1;

pub(crate) mod sealed {
    pub trait Sealed {}
}

/// 由 `kernel-api` crate 发布的固定布局 Kernel API 函数表。
///
/// 该 trait 被 crate 内部封闭，外部 ELM 不能为任意 Rust 类型实现它。这样
/// [`ApiImport`](crate::ApiImport) 才能在安全接口中把内核返回的静态地址解释为具体函数表，
/// 而不会允许调用方借助泛型参数制造类型混淆。
///
/// # 安全性
///
/// 实现者必须保证类型使用 `repr(C)` 固定布局、首字段为 [`ApiTableHeaderV1`]，所有函数指针
/// 都使用该版本规定的调用约定，并且常量与 [`KernelApiLayoutV1`] 目录完全一致。
pub unsafe trait KernelApiTable: sealed::Sealed + Sync + 'static {
    /// 函数表所属的规范命名空间 identifier。
    const IDENTIFIER: &'static str;
    /// 函数表的精确 ABI 版本。
    const VERSION: u16;
    /// 该版本定义的全部能力位。
    const CAPABILITIES: u64;
    /// 该版本规范布局的 SHA-256。
    const LAYOUT_HASH: [u8; 32];
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// 单次 Kernel API 调用必须携带的授权令牌。
///
/// 令牌只标识内核注册表中的租约，不承载权限真值。每个函数表入口都必须把它作为第一个
/// 参数，并根据当前 ELM 执行上下文重新校验 cell、generation、命名空间、版本和该入口所需
/// 能力。卸载、隔离或热替换会删除原租约，因此缓存的旧令牌会立即失效。
///
/// ELM 业务代码不应自行构造令牌，而应从
/// [`ApiTableRef::token`](crate::ApiTableRef::token) 取得与函数表配套的值。
pub struct ApiGrantTokenV1 {
    grant_id: u64,
    generation: u64,
}

impl ApiGrantTokenV1 {
    pub(crate) const fn new(grant_id: u64, generation: u64) -> Self {
        Self {
            grant_id,
            generation,
        }
    }

    /// 返回运行时授权注册表中的非零租约标识。
    pub const fn grant_id(self) -> u64 {
        self.grant_id
    }

    /// 返回取得该租约时调用方 ELM 的 generation。
    pub const fn generation(self) -> u64 {
        self.generation
    }

    /// 检查令牌是否包含可提交给 Kernel API 入口的基本字段。
    ///
    /// 此检查不等于授权成功；内核仍必须查询授权注册表并绑定当前执行上下文。
    pub const fn is_well_formed(self) -> bool {
        self.grant_id != 0 && self.generation != 0
    }
}

/// 所有 Kernel API 函数表共享的稳定前缀。
///
/// 表头之后的每个函数指针都必须把 [`ApiGrantTokenV1`] 作为第一个参数。表头描述表实现
/// 能力，令牌描述调用方租约，两者均不能替代内核入口处的动态授权检查。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApiTableHeaderV1 {
    /// 生产者写入的完整函数表字节数。
    pub struct_size: u32,
    /// 函数表 ABI 版本。
    pub abi_version: u16,
    /// v1 必须为零。
    pub reserved0: u16,
    /// 表实现支持的全部能力位。
    pub capabilities: u64,
}

impl ApiTableHeaderV1 {
    /// 为一个静态函数表构造规范头部。
    pub const fn new<T>(capabilities: u64) -> Self {
        Self {
            struct_size: core::mem::size_of::<T>() as u32,
            abi_version: ELM_API_VERSION_V1,
            reserved0: 0,
            capabilities,
        }
    }

    /// 验证表头是否满足 v1 前缀不变量。
    pub const fn valid_for<T>(&self, required_capabilities: u64) -> bool {
        self.struct_size as usize >= core::mem::size_of::<T>()
            && self.abi_version == ELM_API_VERSION_V1
            && self.reserved0 == 0
            && required_capabilities & !self.capabilities == 0
    }
}

/// 一个命名空间版本对应的规范布局摘要。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KernelApiLayoutV1 {
    /// 命名空间 identifier。
    pub identifier: &'static str,
    /// 精确 ABI 版本。
    pub version: u16,
    /// 函数表固定字节数。
    pub table_size: u32,
    /// 该版本函数表定义的全部能力位。
    pub capabilities: u64,
    /// 函数表规范布局 SHA-256。
    pub layout_hash: [u8; 32],
}

impl KernelApiLayoutV1 {
    /// 从一个由本 crate 发布的函数表类型构造规范布局摘要。
    pub const fn of<T: KernelApiTable>() -> Self {
        Self {
            identifier: T::IDENTIFIER,
            version: T::VERSION,
            table_size: core::mem::size_of::<T>() as u32,
            capabilities: T::CAPABILITIES,
            layout_hash: T::LAYOUT_HASH,
        }
    }
}

/// 查询当前已经公开的函数表布局。
pub(crate) fn layout(identifier: &str, version: u16) -> Option<KernelApiLayoutV1> {
    match (identifier, version) {
        (crate::memory::KERNEL_MEMORY_API_IDENTIFIER, crate::memory::KERNEL_MEMORY_API_VERSION) => {
            Some(KernelApiLayoutV1::of::<crate::memory::KernelMemoryApiV1>())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[repr(C)]
    struct TestTable {
        header: ApiTableHeaderV1,
        call: extern "C" fn() -> i32,
    }

    impl sealed::Sealed for TestTable {}

    // Safety: 测试表使用 repr(C)、规范表头和固定函数指针布局。
    unsafe impl KernelApiTable for TestTable {
        const IDENTIFIER: &'static str = "kernel.test";
        const VERSION: u16 = 1;
        const CAPABILITIES: u64 = 0b101;
        const LAYOUT_HASH: [u8; 32] = [0x5a; 32];
    }

    #[test]
    fn table_header_checks_size_version_and_capabilities() {
        let header = ApiTableHeaderV1::new::<TestTable>(0b101);
        assert!(header.valid_for::<TestTable>(0b001));
        assert!(!header.valid_for::<TestTable>(0b010));
        assert_eq!(
            KernelApiLayoutV1::of::<TestTable>().identifier,
            "kernel.test"
        );
    }

    #[test]
    fn grant_token_rejects_zero_fields() {
        assert_eq!(core::mem::size_of::<ApiGrantTokenV1>(), 16);
        assert_eq!(core::mem::align_of::<ApiGrantTokenV1>(), 8);
        assert!(ApiGrantTokenV1::new(1, 2).is_well_formed());
        assert!(!ApiGrantTokenV1::new(0, 2).is_well_formed());
        assert!(!ApiGrantTokenV1::new(1, 0).is_well_formed());
    }
}
