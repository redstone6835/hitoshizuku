use core::marker::PhantomData;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use elm::{ELM_API_STATUS_PERMISSION, ElmApiNamespaceV1};

use crate::{ApiGrantTokenV1, ApiTableHeaderV1, KernelApiTable};

/// 获取 Kernel API 函数表时可能返回的错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiImportError {
    /// attribute、导入槽参数与函数表类型声明不一致。
    InvalidDeclaration,
    /// 当前 ELM 没有声明或没有被授予该命名空间。
    PermissionDenied,
    /// 内核没有发布请求的命名空间或版本。
    Unavailable,
    /// 返回的表地址、尺寸、版本或能力位不满足契约。
    InvalidTable,
}

/// attribute 声明使用的静态 Kernel API 导入槽。
///
/// 该对象只缓存静态函数表地址和协商结果。子系统入口仍必须根据当前 ELM 上下文检查
/// generation、能力租约和资源所有权，不能把缓存地址视为永久授权。
///
/// ```ignore
/// #[elm::kernel_api(namespace = "kernel.time", version = 1, capabilities = 1)]
/// static TIME: kernel_api::ApiImport<kernel_api::time::TimeApiV1> =
///     kernel_api::ApiImport::new("kernel.time", 1, 1);
///
/// let table = TIME.acquire()?;
/// let now = (table.table().monotonic_ns)(table.token());
/// ```
pub struct ApiImport<T: KernelApiTable> {
    identifier: &'static str,
    version: u16,
    required_capabilities: u64,
    table: AtomicUsize,
    generation: AtomicU64,
    grant_id: AtomicU64,
    capabilities: AtomicU64,
    marker: PhantomData<fn() -> T>,
}

impl<T: KernelApiTable> ApiImport<T> {
    /// 构造尚未协商的 Kernel API 导入槽。
    pub const fn new(identifier: &'static str, version: u16, required_capabilities: u64) -> Self {
        Self {
            identifier,
            version,
            required_capabilities,
            table: AtomicUsize::new(0),
            generation: AtomicU64::new(0),
            grant_id: AtomicU64::new(0),
            capabilities: AtomicU64::new(0),
            marker: PhantomData,
        }
    }

    /// 返回该槽声明的命名空间 identifier。
    pub const fn identifier(&self) -> &'static str {
        self.identifier
    }

    /// 返回该槽要求的精确版本。
    pub const fn version(&self) -> u16 {
        self.version
    }

    /// 返回该槽声明的必需能力位集合。
    pub const fn required_capabilities(&self) -> u64 {
        self.required_capabilities
    }

    /// 首次协商并缓存函数表，或复用已经验证的结果。
    pub fn acquire(&self) -> Result<ApiTableRef<'_, T>, ApiImportError> {
        self.validate_declaration()?;
        let cached = self.table.load(Ordering::Acquire);
        if cached != 0 {
            return self.checked_ref(cached);
        }

        let namespace =
            elm::runtime::query_namespace(self.identifier, &[self.version]).map_err(|error| {
                match error {
                    elm::RuntimeApiError::Status(ELM_API_STATUS_PERMISSION) => {
                        ApiImportError::PermissionDenied
                    }
                    _ => ApiImportError::Unavailable,
                }
            })?;
        self.validate_namespace(&namespace)?;
        self.generation
            .store(namespace.generation, Ordering::Release);
        self.grant_id.store(namespace.grant_id, Ordering::Release);
        self.capabilities
            .store(namespace.capabilities, Ordering::Release);
        self.table.store(namespace.table_address, Ordering::Release);
        self.checked_ref(namespace.table_address)
    }

    fn validate_declaration(&self) -> Result<(), ApiImportError> {
        if self.identifier != T::IDENTIFIER
            || self.version != T::VERSION
            || self.required_capabilities & !T::CAPABILITIES != 0
            || T::LAYOUT_HASH == [0; 32]
        {
            return Err(ApiImportError::InvalidDeclaration);
        }
        Ok(())
    }

    fn validate_namespace(&self, namespace: &ElmApiNamespaceV1) -> Result<(), ApiImportError> {
        let table_address = namespace.table_address;
        let table_size = namespace.table_size as usize;
        if namespace.struct_size < core::mem::size_of::<ElmApiNamespaceV1>() as u32
            || namespace.flags != 0
            || namespace.reserved0 != 0
            || namespace.selected_version != self.version
            || table_address == 0
            || table_address % core::mem::align_of::<T>() != 0
            || table_address % core::mem::align_of::<ApiTableHeaderV1>() != 0
            || core::mem::size_of::<T>() < core::mem::size_of::<ApiTableHeaderV1>()
            || table_size < core::mem::size_of::<T>()
            || table_size < core::mem::size_of::<ApiTableHeaderV1>()
            || namespace.generation == 0
            || namespace.grant_id == 0
            || self.required_capabilities & !namespace.capabilities != 0
        {
            return Err(ApiImportError::InvalidTable);
        }
        // Safety: 上面的范围检查保证可以读取所有 Kernel API 表共享的固定头部。
        let header = unsafe { &*(table_address as *const ApiTableHeaderV1) };
        if !header.valid_for::<T>(self.required_capabilities)
            || header.capabilities != T::CAPABILITIES
            || namespace.capabilities & !header.capabilities != 0
            || header.struct_size as usize > table_size
        {
            return Err(ApiImportError::InvalidTable);
        }
        Ok(())
    }

    fn checked_ref(&self, address: usize) -> Result<ApiTableRef<'_, T>, ApiImportError> {
        if address == 0 || address % core::mem::align_of::<T>() != 0 {
            return Err(ApiImportError::InvalidTable);
        }
        let token = ApiGrantTokenV1::new(
            self.grant_id.load(Ordering::Acquire),
            self.generation.load(Ordering::Acquire),
        );
        if !token.is_well_formed() {
            return Err(ApiImportError::InvalidTable);
        }
        Ok(ApiTableRef {
            address,
            token,
            capabilities: self.capabilities.load(Ordering::Acquire),
            marker: PhantomData,
        })
    }
}

// Safety: 所有可变缓存字段使用原子操作；identifier 指向静态只读字符串。
unsafe impl<T: KernelApiTable> Sync for ApiImport<T> {}

/// 已验证、借用自静态 [`ApiImport`] 的只读函数表引用。
pub struct ApiTableRef<'a, T: KernelApiTable> {
    address: usize,
    token: ApiGrantTokenV1,
    capabilities: u64,
    marker: PhantomData<&'a T>,
}

impl<T: KernelApiTable> ApiTableRef<'_, T> {
    /// 返回协商函数表时当前 ELM 的 generation。
    pub const fn generation(&self) -> u64 {
        self.token.generation()
    }

    /// 返回与该函数表和当前 ELM generation 绑定的调用令牌。
    ///
    /// Kernel API 函数表中的每个入口都把此值作为第一个参数。令牌可以复制，但不能跨
    /// ELM、跨 generation 或用于另一个命名空间；内核会在每次调用时重新验证。
    pub const fn token(&self) -> ApiGrantTokenV1 {
        self.token
    }

    /// 返回当前租约授予的能力位。
    pub const fn capabilities(&self) -> u64 {
        self.capabilities
    }

    /// 返回只读函数表。
    pub fn table(&self) -> &T {
        // Safety: ApiImport::acquire 已验证地址、对齐、尺寸和公共表头；生命周期受静态表约束。
        unsafe { &*(self.address as *const T) }
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

    impl crate::table::sealed::Sealed for TestTable {}

    // Safety: 测试表使用 repr(C)、规范表头和固定函数指针布局。
    unsafe impl KernelApiTable for TestTable {
        const IDENTIFIER: &'static str = "kernel.test";
        const VERSION: u16 = 1;
        const CAPABILITIES: u64 = 0x5;
        const LAYOUT_HASH: [u8; 32] = [0x5a; 32];
    }

    extern "C" fn test_call() -> i32 {
        0
    }

    static TEST_TABLE: TestTable = TestTable {
        header: ApiTableHeaderV1::new::<TestTable>(0x5),
        call: test_call,
    };

    #[test]
    fn import_retains_declared_contract() {
        let import = ApiImport::<TestTable>::new("kernel.test", 1, 0x5);
        assert_eq!(import.identifier(), "kernel.test");
        assert_eq!(import.version(), 1);
        assert_eq!(import.required_capabilities(), 0x5);
    }

    #[test]
    fn namespace_validation_rejects_malformed_table_and_grant() {
        let import = ApiImport::<TestTable>::new("kernel.test", 1, 0x1);
        let namespace = ElmApiNamespaceV1 {
            struct_size: core::mem::size_of::<ElmApiNamespaceV1>() as u32,
            flags: 0,
            selected_version: 1,
            reserved0: 0,
            table_size: core::mem::size_of::<TestTable>() as u32,
            table_address: &TEST_TABLE as *const TestTable as usize,
            generation: 1,
            grant_id: 7,
            capabilities: 0x5,
        };
        assert!(import.validate_namespace(&namespace).is_ok());

        import.generation.store(1, Ordering::Release);
        import.grant_id.store(7, Ordering::Release);
        import.capabilities.store(0x5, Ordering::Release);
        let table = import
            .checked_ref(&TEST_TABLE as *const TestTable as usize)
            .unwrap();
        assert_eq!(table.generation(), 1);
        assert_eq!(table.token().grant_id(), 7);
        assert_eq!(table.capabilities(), 0x5);

        let mut missing_grant = namespace;
        missing_grant.grant_id = 0;
        assert_eq!(
            import.validate_namespace(&missing_grant),
            Err(ApiImportError::InvalidTable)
        );

        let mut truncated = namespace;
        truncated.table_size = core::mem::size_of::<ApiTableHeaderV1>() as u32;
        assert_eq!(
            import.validate_namespace(&truncated),
            Err(ApiImportError::InvalidTable)
        );
    }

    #[test]
    fn import_rejects_type_contract_mismatch_before_query() {
        let wrong_namespace = ApiImport::<TestTable>::new("kernel.other", 1, 0x1);
        assert_eq!(
            wrong_namespace.validate_declaration(),
            Err(ApiImportError::InvalidDeclaration)
        );
        let wrong_capability = ApiImport::<TestTable>::new("kernel.test", 1, 0x8);
        assert_eq!(
            wrong_capability.validate_declaration(),
            Err(ApiImportError::InvalidDeclaration)
        );
    }
}
