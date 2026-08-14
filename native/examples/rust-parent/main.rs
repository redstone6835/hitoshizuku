#![no_main]
#![no_std]

use core::panic::PanicInfo;

use anonlib::{EventPort, HandleTransfer, Image, Process, SpawnRequest};

unsafe extern "C" {
    static mygo_child_image_start: u8;
    static mygo_child_image_end: u8;
}

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    anonlib::abort()
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let Some(parent) = Process::current() else {
        return 1;
    };
    let Some(stdout) = anonlib::stdout() else {
        return 1;
    };
    let child_bytes = {
        let start = core::ptr::addr_of!(mygo_child_image_start) as usize;
        let end = core::ptr::addr_of!(mygo_child_image_end) as usize;
        let Some(length) = end.checked_sub(start) else {
            return 1;
        };
        if length == 0 {
            return 1;
        }
        unsafe { core::slice::from_raw_parts(start as *const u8, length) }
    };
    let Ok(image) = Image::create(&parent, child_bytes) else {
        return 1;
    };
    let Ok(event_port) = EventPort::create(&parent, 4) else {
        return 1;
    };
    let transfer = HandleTransfer::stdout(&stdout);
    let transfers = [transfer];
    let request = SpawnRequest::new(&image).with_transfers(&transfers);
    let Ok(child) = parent.spawn(&request) else {
        return 1;
    };
    let Ok(token) = event_port.bind_process_exit(&child, 0x52555354) else {
        return 1;
    };
    let mut records = [anonlib::EventRecord::default(); 1];
    let Ok(count) = event_port.wait(&mut records, 0) else {
        return 1;
    };
    let Ok(result) = child.wait(0) else {
        return 1;
    };
    let valid = token != 0
        && count == 1
        && child.event_matches(&records[0])
        && records[0].value0 == 0x4a12
        && result.exit_code == 0x4a12;
    if valid {
        let _ = stdout.write(b"Rust parent\n");
        0
    } else {
        1
    }
}
