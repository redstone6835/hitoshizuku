use alloc::vec;

use ktest::ktest;

use crate::{PathKey, Readiness, SocketLinger, SocketTimeval, UnixAddress};

#[ktest]
fn readiness_bits_compose_and_query() {
    let ready = Readiness::READABLE
        .with(Readiness::WRITABLE)
        .with(Readiness::READ_HANGUP);

    assert!(ready.has(Readiness::READABLE));
    assert!(ready.has(Readiness::WRITABLE));
    assert!(ready.has(Readiness::READ_HANGUP));
    assert!(!ready.has(Readiness::FAULT));
    assert_eq!(
        ready.bits(),
        Readiness::READABLE.bits() | Readiness::WRITABLE.bits() | Readiness::READ_HANGUP.bits()
    );
}

#[ktest]
fn unix_address_value_semantics() {
    let path_key = PathKey { fs: 7, ino: 9 };
    let a = UnixAddress::Path {
        key: path_key.clone(),
        display: b"/tmp/sock".to_vec(),
    };
    let b = UnixAddress::Path {
        key: path_key,
        display: b"/tmp/sock".to_vec(),
    };
    let abstract_name = UnixAddress::Abstract(vec![1, 2, 3, 0, 4]);

    assert_eq!(a, b);
    assert_ne!(a, UnixAddress::Unnamed);
    assert_ne!(abstract_name, UnixAddress::Abstract(vec![1, 2, 3]));
}

#[ktest]
fn socket_option_value_types_roundtrip() {
    let linger = SocketLinger {
        enabled: true,
        seconds: 17,
    };
    let timeout = SocketTimeval {
        secs: 3,
        micros: 250_000,
    };

    assert!(linger.enabled);
    assert_eq!(linger.seconds, 17);
    assert_eq!(timeout.secs, 3);
    assert_eq!(timeout.micros, 250_000);
}
