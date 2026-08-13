#![no_main]
#![no_std]

use core::panic::PanicInfo;

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    anonlib::abort()
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let Some(stdout) = anonlib::stdout() else {
        return 1;
    };
    match stdout.write(b"Rust child\n") {
        Ok(written) if written == 11 => 0x4a12,
        _ => 1,
    }
}
