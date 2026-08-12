#![no_main]
#![no_std]

use core::panic::PanicInfo;

use anonlib::{
    AddressSpace, Completion, MemoryCreate, MemoryPermissions, NetworkAddress, Process,
    SocketConfig, Submission,
};

const PAGE_SIZE: usize = 4096;
const OBJECT_SIZE: u64 = (PAGE_SIZE * 2) as u64;
const PAYLOAD: &[u8] = b"SOYO SubmissionRing UDP payload";
const PASS_MESSAGE: &[u8] = b"SOYO Ring Socket PASS\n";

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    anonlib::abort()
}

unsafe fn write_payload(target: *mut u8) {
    unsafe { core::ptr::copy_nonoverlapping(PAYLOAD.as_ptr(), target, PAYLOAD.len()) };
}

unsafe fn clear_payload(target: *mut u8) {
    unsafe { core::ptr::write_bytes(target, 0, PAYLOAD.len()) };
}

unsafe fn payload_matches(target: *const u8) -> bool {
    let source = PAYLOAD.as_ptr();
    let mut index = 0;
    while index < PAYLOAD.len() {
        if unsafe { target.add(index).read() } != unsafe { source.add(index).read() } {
            return false;
        }
        index += 1;
    }
    true
}

fn valid_completion(completion: &Completion, user_data: u64) -> bool {
    completion.user_data() == user_data
        && completion.status().is_ok()
        && completion.values() == (PAYLOAD.len() as u64, 0)
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let Some(process) = Process::current() else {
        return 10;
    };
    let Some(address_space) = AddressSpace::current() else {
        return 11;
    };
    let Ok(memory) = process.create_memory(
        MemoryCreate::anonymous(OBJECT_SIZE, PAGE_SIZE as u64).shared(),
    ) else {
        return 12;
    };
    let Ok(mapping) = memory.map(
        &address_space,
        0,
        OBJECT_SIZE,
        MemoryPermissions::READ_WRITE,
    ) else {
        return 13;
    };
    if mapping.length() != OBJECT_SIZE {
        return 14;
    }
    let source = mapping.address() as *mut u8;
    let target = unsafe { source.add(PAGE_SIZE) };
    unsafe {
        write_payload(source);
        clear_payload(target);
    }

    let sender_address = NetworkAddress::ipv4([127, 0, 0, 1], 39031);
    let receiver_address = NetworkAddress::ipv4([127, 0, 0, 1], 39032);
    let Ok(sender) = process.create_socket(SocketConfig::udp_ipv4()) else {
        return 15;
    };
    let Ok(receiver) = process.create_socket(SocketConfig::udp_ipv4()) else {
        return 16;
    };
    if sender.bind(&sender_address).is_err()
        || receiver.bind(&receiver_address).is_err()
        || sender.connect(&receiver_address).is_err()
        || receiver.connect(&sender_address).is_err()
    {
        return 17;
    }

    let Ok(ring) = process.create_ring(8) else {
        return 18;
    };
    let Ok(registration) = ring.register(&memory, 0, OBJECT_SIZE) else {
        return 19;
    };
    let calls = [
        Submission::socket_send(
            &sender,
            &registration,
            0,
            PAYLOAD.len() as u64,
            None,
            0,
            1,
        ),
        Submission::socket_receive(
            &receiver,
            &registration,
            PAGE_SIZE as u64,
            PAYLOAD.len() as u64,
            None,
            0,
            2,
        ),
    ];
    if ring.kick(&calls).ok() != Some(2) {
        return 20;
    }
    let mut completions = [Completion::default(); 2];
    if ring.wait(&mut completions, 2, 0).ok() != Some(2)
        || !valid_completion(&completions[0], 1)
        || !valid_completion(&completions[1], 2)
        || !unsafe { payload_matches(target) }
    {
        return 21;
    }

    if ring.unregister(registration).is_err() {
        return 22;
    }
    drop((ring, sender, receiver));
    if address_space.unmap(mapping).is_err() || memory.revoke().is_err() {
        return 23;
    }
    let Some(stdout) = anonlib::stdout() else {
        return 24;
    };
    match stdout.write(PASS_MESSAGE) {
        Ok(written) if written == PASS_MESSAGE.len() => 0,
        _ => 25,
    }
}
