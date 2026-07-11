use elm::{
    ElmPayload, HookResult, LifecycleContext, ManagedImport, ManagedRequest, ManagedResult,
    MigrationContext, MigrationExportResult, MixinControl, PointResult, ProviderReply,
    ProviderRequest, ProviderResult, SnapshotReply, SnapshotRequest, SnapshotResult,
};

#[elm::payload("test.frame@1")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TestFrame {
    value: u32,
    enabled: bool,
    bytes: [u8; 3],
}

#[elm::on_initialize]
fn initialize(_context: &LifecycleContext) -> HookResult {
    Ok(())
}

#[elm::on_finalize]
fn finalize(_context: &LifecycleContext) -> HookResult {
    Ok(())
}

#[elm::on_quiesce]
fn quiesce(_context: &LifecycleContext) -> HookResult {
    Ok(())
}

#[elm::on_pause]
fn pause(_context: &LifecycleContext) -> HookResult {
    Ok(())
}

#[elm::on_resume]
fn resume(_context: &LifecycleContext) -> HookResult {
    Ok(())
}

#[elm::on_migrate_export]
fn migrate_export(_context: &MigrationContext, output: &mut [u8]) -> MigrationExportResult {
    output[0] = 7;
    Ok(1)
}

#[elm::on_migrate_import]
fn migrate_import(_context: &MigrationContext, _input: &[u8]) -> HookResult {
    Ok(())
}

#[elm::on_migrate_abort]
fn migrate_abort(_context: &MigrationContext, _input: &[u8]) -> HookResult {
    Ok(())
}

#[elm::entry]
fn start(_context: &elm::EntryContext) -> elm::EntryResult {
    Ok(())
}

#[elm::provider(
    contract = "test.provider@1",
    access = "public",
    direction = "control",
    mode = "shared"
)]
fn provide(_request: &ProviderRequest) -> ProviderResult {
    Ok(ProviderReply::ok())
}

#[elm::provider_snapshot(contract = "test.provider@1")]
fn snapshot(_request: &SnapshotRequest, _output: &mut [u8]) -> SnapshotResult {
    Ok(SnapshotReply::complete(0, 0))
}

#[elm::export(
    name = "test.export",
    contract = "test.export@1",
    version = 1,
    visibility = "dependency"
)]
fn exported(_request: &ManagedRequest) -> ManagedResult {
    Ok(ProviderReply::ok())
}

#[elm::import(
    name = "test.remote",
    contract = "test.remote@1",
    version = 1,
    optional = true
)]
static REMOTE: ManagedImport = ManagedImport::new();

#[elm::mixin_point(
    name = "test.point",
    contract = "test.frame@1",
    stages(ingress, substitute, egress, observe)
)]
fn point(frame: &mut TestFrame) -> PointResult {
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
fn patch(frame: &mut TestFrame) -> MixinControl {
    frame.value += 2;
    MixinControl::Replace
}

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
    let _ = point as fn(&mut TestFrame) -> PointResult;
}
