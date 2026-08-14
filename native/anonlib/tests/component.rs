use std::sync::Mutex;

use anonlib::{Component, HandleTransfer, Image, Process, stdout};

#[allow(dead_code)]
mod binding {
    include!(env!("MYGO_PROGRAM_RS"));
}

static TEST_LOCK: Mutex<()> = Mutex::new(());

const PROCESS: u64 = 0x0000_0001_0000_0001;
const STDOUT: u64 = 0x0000_0001_0000_0002;
const ROOT_IMAGE: u64 = 0x0000_0001_0000_0003;
const COMPONENT: u64 = 0x0000_0001_0000_0004;

#[repr(C)]
struct NativeResult {
    status: u32,
    reserved: u32,
    value0: u64,
    value1: u64,
}

#[repr(C)]
struct ComponentResult {
    status: u32,
    handle: u64,
}

#[unsafe(no_mangle)]
extern "C" fn mrt_initial_handle(requirement_id: u32) -> u64 {
    match requirement_id {
        binding::MYGO_REQUIREMENT_self_process => PROCESS,
        binding::MYGO_REQUIREMENT_stdout => STDOUT,
        _ => 0,
    }
}

#[unsafe(no_mangle)]
extern "C" fn mrt_call(
    slot: u64,
    object_handle: u64,
    _arg0: u64,
    arg1: u64,
    arg2: u64,
    arg3: u64,
    arg4: u64,
) -> NativeResult {
    assert_eq!(slot, binding::MYGO_SLOT_image_create);
    assert_eq!(object_handle, PROCESS);
    assert_eq!((arg1, arg2, arg3, arg4), (4, 0, 0, 0));
    NativeResult {
        status: binding::MYGO_STATUS_ok,
        reserved: 0,
        value0: ROOT_IMAGE,
        value1: 0,
    }
}

#[unsafe(no_mangle)]
extern "C" fn mrt_component_load(
    process: u64,
    request: *const binding::MygoComponentLoadRequest,
) -> ComponentResult {
    assert_eq!(process, PROCESS);
    let request = unsafe { &*request };
    assert_eq!(request.root_image, ROOT_IMAGE);
    assert_eq!(request.images.count, 0);
    assert_eq!(request.bindings.count, 1);
    let binding = unsafe { &*(request.bindings.ptr as *const binding::MygoHandleTransfer) };
    assert_eq!(binding.requirement_id, binding::MYGO_REQUIREMENT_stdout);
    assert_eq!(binding.source_handle, STDOUT);
    assert_eq!(binding.requested_rights, binding::MYGO_RIGHT_write);
    assert_eq!(binding.flags, 0);
    ComponentResult {
        status: binding::MYGO_STATUS_ok,
        handle: COMPONENT,
    }
}

#[unsafe(no_mangle)]
extern "C" fn mrt_terminate(_status: u32) -> ! {
    panic!("test must not terminate")
}

#[unsafe(no_mangle)]
extern "C" fn mrt_abort() -> ! {
    panic!("test must not abort")
}

#[test]
fn component_load_passes_explicit_capability_bindings() {
    let _guard = TEST_LOCK.lock().unwrap();
    let process = Process::current().unwrap();
    let root = Image::create(&process, b"soyo").unwrap();
    let stdout = stdout().unwrap();
    let bindings = [HandleTransfer::stdout(&stdout)];
    let component = Component::load_with_bindings(&process, &root, [], &bindings).unwrap();

    std::mem::forget(component);
    std::mem::forget(root);
}
