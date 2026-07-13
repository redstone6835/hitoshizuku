use elm::{DeviceIrqResult, HookResult, LifecycleContext};
use kernel_api::device::{
    KernelDeviceIoFrameV1, KernelDeviceIrqFrameV1, KernelDeviceSnapshotV1,
};

#[elm::device_match]
fn matches_device(_device: &KernelDeviceSnapshotV1) -> bool {
    true
}

#[elm::device_probe]
fn probe_device(_device: &KernelDeviceSnapshotV1) -> HookResult {
    Ok(())
}

#[elm::device_remove]
fn remove_device(_device: &KernelDeviceSnapshotV1) -> HookResult {
    Ok(())
}

#[elm::device_driver(
    name = "test-driver",
    bus = "pci",
    priority = 100,
    generic = false,
    match_callback = "matches_device",
    probe_callback = "probe_device",
    remove_callback = "remove_device"
)]
fn test_driver() {}

#[elm::device_function(contract = "test.device.function@1")]
fn device_function(frame: &mut KernelDeviceIoFrameV1) -> HookResult {
    frame.output_len = 0;
    Ok(())
}

#[elm::device_irq(mode = "deferred", contract = "test.device.irq@1")]
fn device_irq(_frame: &KernelDeviceIrqFrameV1) -> DeviceIrqResult {
    Ok(true)
}

#[elm::device_discovery(bus = "test-bus")]
fn discover_devices(_context: &LifecycleContext) -> HookResult {
    Ok(())
}

#[test]
fn device_attributes_generate_complete_requests_and_callbacks() {
    let request = test_driver();
    assert!(request.is_well_formed());
    assert_eq!(request.name.as_str(), Some("test-driver"));
    assert_eq!(request.bus.as_str(), Some("pci"));
    assert_eq!(request.priority, 100);
    assert_ne!(device_function_function_callback_address(), 0);
    assert_ne!(device_irq_irq_callback_address(), 0);
    let _ = discover_devices as fn(&LifecycleContext) -> HookResult;
}
