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

    // 实机 bringup 测试开关：命令行带 mygo.reboot=1 时 panic 优先热重启，
    // 便于一次性测试引导后自动回到 Debian（U-Boot 菜单默认项）。
    let prefer_reboot = general::start_cmdline()
        .map(general::cmdline::Cmdline::new)
        .and_then(|cmdline| cmdline.find("mygo.reboot"))
        .is_some_and(|value| value == "1");
    if prefer_reboot {
        if power::reboot().is_ok() {
            log::emergency!("[panic] reboot request issued");
        } else {
            log::emergency!("[panic] power control unavailable");
        }
    } else if power::shutdown().is_ok() {
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