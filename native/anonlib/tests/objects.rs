use anonlib::{
    AddressSpace, Channel, Completion, DeviceFunction, Directory, DirectoryRights, FileRights,
    MemoryCreate, MemoryPermissions, Process, Ring, Socket, SocketConfig, Thread, ThreadCreate,
};
use std::alloc::{Layout, alloc_zeroed};
use std::sync::atomic::{AtomicUsize, Ordering};

const RING_ENTRIES: u32 = 8;
const RING_SQ_OFFSET: usize = 64;
const RING_CQ_OFFSET: usize =
    RING_SQ_OFFSET + RING_ENTRIES as usize * size_of::<binding::MygoSubmissionDescriptor>();
const RING_BYTES: usize =
    RING_CQ_OFFSET + RING_ENTRIES as usize * size_of::<binding::MygoCompletionRecord>();

static RING_BASE: AtomicUsize = AtomicUsize::new(0);

#[allow(dead_code)]
mod binding {
    include!(env!("MYGO_PROGRAM_RS"));
}

#[repr(C)]
struct NativeResult {
    status: u32,
    reserved: u32,
    value0: u64,
    value1: u64,
}

#[unsafe(no_mangle)]
extern "C" fn mrt_initial_handle(requirement_id: u32) -> u64 {
    match requirement_id {
        binding::MYGO_REQUIREMENT_self_process => 1,
        binding::MYGO_REQUIREMENT_current_address_space => 2,
        binding::MYGO_REQUIREMENT_root_directory => 3,
        binding::MYGO_REQUIREMENT_device_function => 4,
        _ => 0,
    }
}

#[unsafe(no_mangle)]
extern "C" fn mrt_current_component() -> u64 {
    0
}

#[unsafe(no_mangle)]
extern "C" fn mrt_call(
    slot: u64,
    _object_handle: u64,
    arg0: u64,
    _arg1: u64,
    _arg2: u64,
    _arg3: u64,
    _arg4: u64,
) -> NativeResult {
    let mut result = NativeResult {
        status: binding::MYGO_STATUS_ok,
        reserved: 0,
        value0: 0,
        value1: 0,
    };
    if slot == binding::MYGO_SLOT_memory_create {
        result.value0 = 10;
    } else if slot == binding::MYGO_SLOT_memory_map {
        result.value0 = 0x1000_0000;
        result.value1 = 0x4000;
    } else if slot == binding::MYGO_SLOT_channel_create {
        result.value0 = 20;
        result.value1 = 21;
    } else if slot == binding::MYGO_SLOT_ring_create {
        let layout = Layout::from_size_align(RING_BYTES, 64).unwrap();
        let base = unsafe { alloc_zeroed(layout) };
        assert!(!base.is_null());
        unsafe {
            base.cast::<binding::MygoRingSharedState>()
                .write(binding::MygoRingSharedState {
                    magic: binding::MYGO_RING_SHARED_MAGIC,
                    version: binding::MYGO_RING_SHARED_VERSION,
                    flags: 0,
                    entries: RING_ENTRIES,
                    mask: RING_ENTRIES - 1,
                    sq_head: 0,
                    sq_tail: 0,
                    cq_head: 0,
                    cq_tail: 0,
                    sq_offset: RING_SQ_OFFSET as u64,
                    cq_offset: RING_CQ_OFFSET as u64,
                    generation: 1,
                    reserved: 0,
                });
        }
        RING_BASE.store(base as usize, Ordering::Release);
        result.value0 = 30;
        result.value1 = base as u64;
    } else if slot == binding::MYGO_SLOT_ring_register {
        result.value0 = 31;
    } else if slot == binding::MYGO_SLOT_ring_wait {
        let base = RING_BASE.load(Ordering::Acquire) as *mut u8;
        assert!(!base.is_null());
        unsafe {
            base.add(RING_CQ_OFFSET)
                .cast::<binding::MygoCompletionRecord>()
                .write(binding::MygoCompletionRecord {
                    user_data: 71,
                    status: binding::MYGO_STATUS_ok,
                    reserved: 0,
                    value0: 4096,
                    value1: 0,
                });
            (*base.cast::<binding::MygoRingSharedState>()).cq_tail = 1;
        }
        result.value0 = 1;
    } else if slot == binding::MYGO_SLOT_socket_create {
        result.value0 = 40;
    } else if slot == binding::MYGO_SLOT_thread_create {
        result.value0 = 50;
    } else if slot == binding::MYGO_SLOT_directory_open {
        result.value0 = 60;
    } else if slot == binding::MYGO_SLOT_device_query {
        unsafe {
            *(arg0 as *mut binding::MygoDeviceInfo) = binding::MygoDeviceInfo::default();
        }
    }
    result
}

#[unsafe(no_mangle)]
extern "C" fn mrt_terminate(_status: u32) -> ! {
    panic!("测试不得终止进程")
}

#[unsafe(no_mangle)]
extern "C" fn mrt_abort() -> ! {
    panic!("测试不得进入 abort")
}

unsafe extern "C" fn thread_entry(_argument: u64) -> ! {
    loop {
        core::hint::spin_loop();
    }
}

#[test]
fn typed_native_objects_use_explicit_lifecycle_operations() {
    let process = Process::current().unwrap();
    let address_space = AddressSpace::current().unwrap();
    let memory = process
        .create_memory(MemoryCreate::anonymous(0x4000, 0x1000))
        .unwrap();
    let mapping = memory
        .map(
            &address_space,
            0,
            0x4000,
            MemoryPermissions::READ_WRITE,
        )
        .unwrap();

    let (left, right): (Channel, Channel) = process.create_channel(8).unwrap();
    let ring: Ring = process.create_ring(8).unwrap();
    let mut impossible = [Completion::default(); 9];
    assert_eq!(
        ring.wait(&mut impossible, 9, 0).unwrap_err().raw(),
        binding::MYGO_STATUS_core_invalid_argument
    );
    let registration = ring.register(&memory, 0, 0x4000).unwrap();
    let mut completions = [Completion::default(); 1];
    assert_eq!(ring.wait(&mut completions, 1, 0).unwrap(), 1);
    assert!(completions[0].status().is_ok());
    assert_eq!(completions[0].values(), (4096, 0));
    ring.unregister(registration).unwrap();

    let socket: Socket = process.create_socket(SocketConfig::tcp_ipv4()).unwrap();
    let root = Directory::root().unwrap();
    let _file = root.open_file(b"example", FileRights::READ).unwrap();
    let _directory = root
        .open_directory(b"subdir", DirectoryRights::OPEN | DirectoryRights::INSPECT)
        .unwrap();
    let device = DeviceFunction::initial().unwrap();
    let _ = device.query().unwrap();

    let thread: Thread = process
        .create_thread(ThreadCreate::new(
            thread_entry,
            &memory,
            0,
            0x4000,
            0,
        ))
        .unwrap();
    thread.terminate(0).unwrap();

    address_space.unmap(mapping).unwrap();
    drop((left, right, socket, thread));
}
