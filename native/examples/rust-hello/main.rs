#![no_main]
#![no_std]

use core::panic::PanicInfo;

const MESSAGE: &[u8] = b"Hello Soyo from Rust!\n";

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    anonlib::abort()
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let Some(stdout) = anonlib::stdout() else {
        return 1;
    };
    match stdout.write(MESSAGE) {
        Ok(written) if written == MESSAGE.len() => 38,
        _ => 1,
    }
}
