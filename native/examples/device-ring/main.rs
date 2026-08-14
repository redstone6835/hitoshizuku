#![no_main]
#![no_std]

use core::panic::PanicInfo;

use anonlib::{AddressSpace, Completion, DeviceFunction, MemoryPermissions, Process, Submission};

const PAGE_SIZE: u64 = 4096;
const OUTPUT_SIZE: u64 = 32;
const PASS_MESSAGE: &[u8] = b"SOYO Ring Device/DMA PASS\n";

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    anonlib::abort()
}

unsafe fn output_is_nonzero(address: *const u8) -> bool {
    let mut index = 0;
    while index < OUTPUT_SIZE as usize {
        if unsafe { address.add(index).read() } != 0 {
            return true;
        }
        index += 1;
    }
    false
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let Some(process) = Process::current() else {
        return 10;
    };
    let Some(address_space) = AddressSpace::current() else {
        return 11;
    };
    let Some(device) = DeviceFunction::initial() else {
        return 12;
    };
    let Ok(info) = device.query() else {
        return 13;
    };
    if info.generation == 0 || info.state == 0 {
        return 14;
    }

    let Ok(memory) = device.create_dma_memory(&process, PAGE_SIZE, PAGE_SIZE, false, true) else {
        return 15;
    };
    let Ok(mapping) = memory.map(
        &address_space,
        0,
        PAGE_SIZE,
        MemoryPermissions::READ_WRITE,
    ) else {
        return 16;
    };
    unsafe { core::ptr::write_bytes(mapping.address() as *mut u8, 0, OUTPUT_SIZE as usize) };

    let Ok(ring) = process.create_ring(4) else {
        return 17;
    };
    let Ok(registration) = ring.register(&memory, 0, PAGE_SIZE) else {
        return 18;
    };
    let request = Submission::device_invoke(
        &device,
        1,
        None,
        Some((&registration, OUTPUT_SIZE)),
        1,
    );
    if ring.kick(&[request]).ok() != Some(1) {
        return 19;
    }
    let mut completion = [Completion::default(); 1];
    if ring.wait(&mut completion, 1, 0).ok() != Some(1)
        || completion[0].user_data() != 1
        || !completion[0].status().is_ok()
        || completion[0].values() != (OUTPUT_SIZE, 0)
        || !unsafe { output_is_nonzero(mapping.address() as *const u8) }
    {
        return 20;
    }

    if ring.unregister(registration).is_err() {
        return 21;
    }
    drop(ring);
    if address_space.unmap(mapping).is_err() || memory.revoke().is_err() {
        return 22;
    }
    let Some(stdout) = anonlib::stdout() else {
        return 23;
    };
    match stdout.write(PASS_MESSAGE) {
        Ok(written) if written == PASS_MESSAGE.len() => 0,
        _ => 24,
    }
}
