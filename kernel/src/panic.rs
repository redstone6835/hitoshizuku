use core::panic::PanicInfo;

use general::firmware::power;

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    log::emergency!("{}", info.message());
    if let Some(location) = info.location() {
        log::emergency!(
            "[panic] location={}#L{}C{}",
            location.file(),
            location.line(),
            location.column()
        );
    }

    if power::shutdown().is_ok() {
        log::emergency!("[panic] shutdown request issued");
    } else if power::reboot().is_ok() {
        log::emergency!("[panic] reboot request issued");
    } else {
        log::emergency!("[panic] power control unavailable");
    }

    log::emergency!("[panic] system halted");

    loop {
        core::hint::spin_loop();
    }
}
