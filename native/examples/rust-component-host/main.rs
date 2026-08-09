#![no_main]
#![no_std]

use core::panic::PanicInfo;

use anonlib::{Component, Image, Process};

type AddFunction = unsafe extern "C" fn(u64, u64) -> u64;

const ADD_INTERFACE_ID: [u8; 16] = *b"mygo.add.iface01";
const ADD_SIGNATURE_HASH: [u8; 32] = [
    0xcc, 0x8b, 0xc7, 0x7a, 0x01, 0xe5, 0x03, 0xa7, 0xd4, 0x1b, 0x42, 0x4b, 0xf9, 0x39,
    0x62, 0x26, 0x16, 0xde, 0x5e, 0x75, 0x75, 0x44, 0x79, 0x13, 0x4e, 0xff, 0xf2, 0x4f,
    0x72, 0x7c, 0x8c, 0x25,
];
const PASS_MESSAGE: &[u8] = b"SOYO Rust component PASS: 42\n";

unsafe extern "C" {
    static mygo_plugin_image_start: u8;
    static mygo_plugin_image_end: u8;
}

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    anonlib::abort()
}

fn plugin_image() -> Option<&'static [u8]> {
    let start = core::ptr::addr_of!(mygo_plugin_image_start) as usize;
    let end = core::ptr::addr_of!(mygo_plugin_image_end) as usize;
    let length = end.checked_sub(start)?;
    Some(unsafe { core::slice::from_raw_parts(start as *const u8, length) })
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let Some(process) = Process::current() else {
        return 10;
    };
    let Some(bytes) = plugin_image() else {
        return 11;
    };
    let Ok(image) = Image::create(&process, bytes) else {
        return 12;
    };
    let Ok(component) = Component::load(&process, &image, []) else {
        return 13;
    };
    let Ok(interface) = component.interface::<AddFunction>(ADD_INTERFACE_ID, ADD_SIGNATURE_HASH)
    else {
        return 14;
    };
    let Ok(call) = interface.enter() else {
        return 15;
    };
    let add = unsafe { call.target() };
    let sum = unsafe { add(19, 23) };
    drop(call);
    if sum != 42 {
        return 16;
    }
    if component.unload(0).is_err() {
        return 17;
    }
    let Some(stdout) = anonlib::stdout() else {
        return 18;
    };
    match stdout.write(PASS_MESSAGE) {
        Ok(written) if written == PASS_MESSAGE.len() => 0,
        _ => 19,
    }
}
