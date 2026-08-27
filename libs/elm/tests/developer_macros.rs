use elm::{
    DirectImport, ElmModule, ElmPayload, HookError, HookResult, KernelMixinContext,
    LifecycleContext, ManagedImport, ManagedRequest, ManagedResult, MigrationContext,
    MigrationExportResult, MixinControl, PointResult, ProviderReply, ProviderRequest,
    ProviderResult, SnapshotReply, SnapshotRequest, SnapshotResult,
};
#[cfg(not(feature = "elm-integrated"))]
use elm::{
    ElmCallFrame, ElmContext, ElmId, ElmLifecyclePhase, ElmNativeHookContextV1, ElmState,
    Generation,
};

#[elm::payload("test.frame@1")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TestFrame {
    value: u32,
    enabled: bool,
    bytes: [u8; 3],
}

struct TestModule;

#[elm::module]
impl ElmModule for TestModule {
    fn create(_context: &LifecycleContext) -> Result<Self, HookError> {
        Ok(Self)
    }

    fn initialize(&mut self, _context: &LifecycleContext) -> HookResult {
        Ok(())
    }

    fn finalize(&mut self, _context: &LifecycleContext) -> HookResult {
        Ok(())
    }

    fn migrate_export(
        &self,
        _context: &MigrationContext,
        output: &mut [u8],
    ) -> MigrationExportResult {
        output[0] = 7;
        Ok(1)
    }

    fn migrate_import(&mut self, _context: &MigrationContext, _input: &[u8]) -> HookResult {
        Ok(())
    }

    fn migrate_abort(&mut self, _context: &MigrationContext, _input: &[u8]) -> HookResult {
        Ok(())
    }

    #[elm::provider(
        contract = "test.provider@1",
        access = "public",
        direction = "control",
        mode = "shared"
    )]
    fn provide(&self, _request: &ProviderRequest) -> ProviderResult {
        Ok(ProviderReply::ok())
    }

    #[elm::provider_snapshot(contract = "test.provider@1")]
    fn snapshot(&self, _request: &SnapshotRequest, _output: &mut [u8]) -> SnapshotResult {
        Ok(SnapshotReply::complete(0, 0))
    }

    #[elm::export(
        name = "test.export",
        contract = "test.export@1",
        version = 1,
        visibility = "dependency"
    )]
    fn exported(&self, _request: &ManagedRequest) -> ManagedResult {
        Ok(ProviderReply::ok())
    }

    #[elm::export(
        name = "test.direct-add",
        contract = "test.direct-add@1",
        version = 1,
        mode = "direct-pinned",
        visibility = "dependency"
    )]
    fn direct_add(&self, left: u64, right: u64) -> u64 {
        left + right
    }

    #[elm::mixin_point(
        name = "test.point",
        contract = "test.frame@1",
        stages(ingress, substitute, egress, observe)
    )]
    fn point(&self, frame: &mut TestFrame) -> PointResult {
        frame.value += 1;
        Ok(())
    }

    #[elm::mixin(
        target = "test.target",
        point = "test.point",
        stage = "ingress",
        contract = "test.frame@1",
        priority = -100
    )]
    fn patch(&self, frame: &mut TestFrame) -> MixinControl {
        frame.value += 2;
        MixinControl::Replace
    }
}

#[elm::mixin(target = "allocator")]
impl TestModule {
    #[elm::inject(method = "GlobalAlloc.alloc", at = "head", priority = 10)]
    #[cfg_attr(feature = "elm-integrated", allow(dead_code))]
    fn trace_global_alloc(&self, _context: &mut KernelMixinContext<'_>) -> HookResult {
        Ok(())
    }
}

#[elm::import(
    name = "test.remote",
    contract = "test.remote@1",
    version = 1,
    optional = true
)]
static REMOTE: ManagedImport = ManagedImport::new();

#[elm::import(
    name = "test.direct-add",
    contract = "test.direct-add@1",
    version = 1,
    mode = "direct-pinned",
    optional = true
)]
static DIRECT_REMOTE: DirectImport<fn(u64, u64) -> u64> = DirectImport::new();

#[rustfmt::skip]
macro_rules! generated_kernel_symbol {
    ($name:literal, $contract:literal, $version:literal, $slot:ident) => {
        #[elm::kernel_symbol(
            name = $name,
            contract = $contract,
            version = $version,
            abi = "fn()->u64"
        )]
        static $slot: DirectImport<fn() -> u64> = DirectImport::new();
    };
}

generated_kernel_symbol!(
    "test.generated-kernel-symbol",
    "kernel.test.generated@1",
    1,
    GENERATED_KERNEL_SYMBOL
);

#[test]
fn payload_uses_canonical_little_endian_encoding() {
    let frame = TestFrame {
        value: 0x4433_2211,
        enabled: true,
        bytes: [5, 6, 7],
    };
    let mut bytes = [0u8; TestFrame::WIRE_SIZE];
    assert_eq!(frame.encode(&mut bytes), Ok(TestFrame::WIRE_SIZE));
    assert_eq!(bytes, [0x11, 0x22, 0x33, 0x44, 1, 5, 6, 7]);
    assert_eq!(TestFrame::decode(&bytes), Ok(frame));
    assert!(REMOTE.handle().is_none());
    // Safety: 单元测试没有装载器，因此生成的直接符号槽必须保持未绑定。
    assert!(unsafe { GENERATED_KERNEL_SYMBOL.get() }.is_none());
    // Safety: 单元测试没有装载器，因此类型化 ELM 直连槽同样必须保持未绑定。
    assert!(unsafe { DIRECT_REMOTE.get() }.is_none());
    let _ = point as fn(&mut TestFrame) -> PointResult;
}

#[test]
#[cfg(not(feature = "elm-integrated"))]
fn module_descriptor_binds_registered_methods_to_the_active_instance() {
    assert!(__ELM_MODULE_DESCRIPTOR_V1.valid_for::<TestModule>());
    let initialize = ElmContext::new(
        ElmId(7),
        None,
        Generation::FIRST,
        ElmState::Loaded,
        ElmLifecyclePhase::Initialize,
        0,
    );
    let mut initialize_frame = ElmNativeHookContextV1::from_context(&initialize);
    // Safety: frame 由规范构造器产生，函数指针来自已验证的静态模块描述符。
    assert_eq!(
        unsafe { (__ELM_MODULE_DESCRIPTOR_V1.initialize)(&mut initialize_frame) },
        0
    );

    let request = ProviderRequest {
        cell_id: 7,
        port_id: 1,
        lease_id: 1,
        frame: ElmCallFrame::empty(1, 1, 0),
    };
    assert!(provide(&request).is_ok());
    assert_eq!(direct_add(20, 22), 42);

    let finalize = ElmContext::new(
        ElmId(7),
        None,
        Generation::FIRST,
        ElmState::Quiescing,
        ElmLifecyclePhase::Finalize,
        0,
    );
    let mut finalize_frame = ElmNativeHookContextV1::from_context(&finalize);
    // Safety: 与初始化调用相同，且运行时语义要求同一 generation 在排空后终结。
    assert_eq!(
        unsafe { (__ELM_MODULE_DESCRIPTOR_V1.finalize)(&mut finalize_frame) },
        0
    );
}
