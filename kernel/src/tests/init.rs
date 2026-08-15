use alloc::string::String;

use ktest::ktest;

use crate::boot_root::{root_command, root_device_node};
use crate::sched::{
    INIT_CANDIDATES, InitCommandLine, init_args_after_delimiter, parse_init_command_line,
    ramdisk_init_command,
};

#[ktest]
fn linux_init_command_line_uses_last_explicit_value() {
    assert_eq!(
        parse_init_command_line(Some(
            b"rdinit=/early-a init=/first rdinit=/early-b init=/last"
        )),
        InitCommandLine {
            rdinit: Some("/early-b"),
            init: Some("/last"),
        }
    );
    assert_eq!(
        parse_init_command_line(Some(b"init=/good -- init=/bad")),
        InitCommandLine {
            rdinit: None,
            init: Some("/good"),
        }
    );
    assert_eq!(
        parse_init_command_line(Some(br#"init="/bin/my init" root="/dev/vd0""#)),
        InitCommandLine {
            rdinit: None,
            init: Some("/bin/my init"),
        }
    );
}

#[ktest]
fn linux_init_command_line_preserves_explicit_empty_values() {
    assert_eq!(
        parse_init_command_line(Some(b"rdinit= init=")),
        InitCommandLine {
            rdinit: Some(""),
            init: Some(""),
        }
    );
    assert_eq!(parse_init_command_line(None), InitCommandLine::default());
    assert_eq!(ramdisk_init_command(None), "/init");
    assert_eq!(ramdisk_init_command(Some(b"rdinit=/early")), "/early");
}

#[ktest]
fn linux_init_fallback_order_matches_kernel_init() {
    assert_eq!(
        INIT_CANDIDATES,
        ["/sbin/init", "/etc/init", "/bin/init", "/bin/sh"]
    );
}

#[ktest]
fn linux_init_arguments_after_double_dash_are_preserved() {
    assert_eq!(
        init_args_after_delimiter(Some(br#"console=uart0 -- -c "hello world" FOO=bar"#)),
        [
            String::from("-c"),
            String::from("hello world"),
            String::from("FOO=bar")
        ]
    );
    assert_eq!(
        init_args_after_delimiter(Some("-- \"\" \"你好 world\" KEY=\"two words\"".as_bytes())),
        [
            String::new(),
            String::from("你好 world"),
            String::from("KEY=two words")
        ]
    );
    assert_eq!(
        init_args_after_delimiter(Some(b"\"--\" quoted-delimiter")),
        [String::from("quoted-delimiter")]
    );
    assert!(init_args_after_delimiter(Some(b"init=/bin/sh")).is_empty());
}

#[ktest]
fn linux_root_command_line_uses_last_explicit_value() {
    assert_eq!(
        root_command(Some(b"root=/dev/vd0 root=/dev/vd1")),
        Some("/dev/vd1")
    );
    assert_eq!(
        root_command(Some(b"root=/dev/vd0 -- root=/dev/vd1")),
        Some("/dev/vd0")
    );
    assert_eq!(root_command(Some(br#"root="/dev/vd0""#)), Some("/dev/vd0"));
    assert_eq!(root_command(None), None);
}

#[ktest]
fn boot_root_accepts_direct_devtmpfs_nodes() {
    assert_eq!(root_device_node("/dev/vd0"), Some("vd0"));
    assert_eq!(root_device_node("vd1"), Some("vd1"));
    assert_eq!(root_device_node(""), None);
    assert_eq!(root_device_node("UUID=dead/beef"), None);
}
