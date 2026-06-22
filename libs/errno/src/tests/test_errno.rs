#[cfg(feature = "ktest-kernel")]
extern crate alloc;
#[cfg(not(feature = "ktest-kernel"))]
extern crate std;

use crate::Errno;
use ktest::ktest;

const KNOWN_ERRNOS: &[(i32, Errno)] = &[
    (0, Errno::ESUCCESS),
    (1, Errno::EPERM),
    (2, Errno::ENOENT),
    (3, Errno::ESRCH),
    (4, Errno::EINTR),
    (5, Errno::EIO),
    (6, Errno::ENXIO),
    (7, Errno::E2BIG),
    (8, Errno::ENOEXEC),
    (9, Errno::EBADF),
    (10, Errno::ECHILD),
    (11, Errno::EAGAIN),
    (12, Errno::ENOMEM),
    (13, Errno::EACCES),
    (14, Errno::EFAULT),
    (16, Errno::EBUSY),
    (17, Errno::EEXIST),
    (18, Errno::EXDEV),
    (19, Errno::ENODEV),
    (20, Errno::ENOTDIR),
    (21, Errno::EISDIR),
    (22, Errno::EINVAL),
    (23, Errno::ENFILE),
    (24, Errno::EMFILE),
    (25, Errno::ENOTTY),
    (26, Errno::ETXTBSY),
    (27, Errno::EFBIG),
    (28, Errno::ENOSPC),
    (29, Errno::ESPIPE),
    (30, Errno::EROFS),
    (31, Errno::EMLINK),
    (32, Errno::EPIPE),
    (33, Errno::EDOM),
    (34, Errno::ERANGE),
    (35, Errno::EDEADLK),
    (36, Errno::ENAMETOOLONG),
    (37, Errno::ENOLCK),
    (38, Errno::ENOSYS),
    (39, Errno::ENOTEMPTY),
    (40, Errno::ELOOP),
    (42, Errno::ENOMSG),
    (43, Errno::EIDRM),
    (60, Errno::ENOSTR),
    (61, Errno::ENODATA),
    (62, Errno::ETIME),
    (63, Errno::ENOSR),
    (67, Errno::ENOLINK),
    (71, Errno::EPROTO),
    (72, Errno::EMULTIHOP),
    (74, Errno::EBADMSG),
    (75, Errno::EOVERFLOW),
    (84, Errno::EILSEQ),
    (88, Errno::ENOTSOCK),
    (89, Errno::EDESTADDRREQ),
    (90, Errno::EMSGSIZE),
    (91, Errno::EPROTOTYPE),
    (92, Errno::ENOPROTOOPT),
    (93, Errno::EPROTONOSUPPORT),
    (95, Errno::EOPNOTSUPP),
    (97, Errno::EAFNOSUPPORT),
    (98, Errno::EADDRINUSE),
    (99, Errno::EADDRNOTAVAIL),
    (100, Errno::ENETDOWN),
    (101, Errno::ENETUNREACH),
    (102, Errno::ENETRESET),
    (103, Errno::ECONNABORTED),
    (104, Errno::ECONNRESET),
    (105, Errno::ENOBUFS),
    (106, Errno::EISCONN),
    (107, Errno::ENOTCONN),
    (110, Errno::ETIMEDOUT),
    (111, Errno::ECONNREFUSED),
    (113, Errno::EHOSTUNREACH),
    (114, Errno::EALREADY),
    (115, Errno::EINPROGRESS),
    (116, Errno::ESTALE),
    (122, Errno::EDQUOT),
    (125, Errno::ECANCELED),
    (130, Errno::EOWNERDEAD),
    (131, Errno::ENOTRECOVERABLE),
];

// ── from_i32 ──────────────────────────────────────────────────────

#[ktest]
fn from_i32_known_codes() {
    for &(code, errno) in KNOWN_ERRNOS {
        assert_eq!(
            Errno::from_i32(code),
            errno,
            "from_i32 failed for code {}",
            code
        );
    }
}

#[ktest]
fn from_i32_other() {
    assert_eq!(Errno::from_i32(-1), Errno::Other(-1));
    assert_eq!(Errno::from_i32(999), Errno::Other(999));
    assert_eq!(Errno::from_i32(-999), Errno::Other(-999));
}

#[ktest]
fn from_i32_unknown_positive() {
    assert_eq!(Errno::from_i32(15), Errno::Other(15));
}

// ── as_i32 ────────────────────────────────────────────────────────

#[ktest]
fn as_i32_roundtrip_known() {
    for &(code, _) in KNOWN_ERRNOS {
        let e = Errno::from_i32(code);
        assert_eq!(e.as_i32(), code, "roundtrip failed for code {}", code);
    }
}

#[ktest]
fn as_i32_other_roundtrip() {
    assert_eq!(Errno::Other(42).as_i32(), 42);
    assert_eq!(Errno::Other(-7).as_i32(), -7);
}

#[ktest]
fn as_i32_esuccess_is_zero() {
    assert_eq!(Errno::ESUCCESS.as_i32(), 0);
}

#[ktest]
fn posix_aliases_match_linux_values() {
    assert_eq!(Errno::EWOULDBLOCK, Errno::EAGAIN);
    assert_eq!(Errno::EWOULDBLOCK.as_i32(), 11);
    assert_eq!(Errno::ENOTSUP, Errno::EOPNOTSUPP);
    assert_eq!(Errno::ENOTSUP.as_i32(), 95);
}

// ── as_usize ──────────────────────────────────────────────────────

#[ktest]
fn as_usize_is_negated() {
    assert_eq!(Errno::ESUCCESS.as_usize(), 0);
    // as_usize 计算 (-self.as_i32()) as usize，i32→usize 会符号扩展
    assert_eq!(Errno::EPERM.as_usize(), (-1i32) as usize);
    assert_eq!(Errno::ENOENT.as_usize(), (-2i32) as usize);
    assert_eq!(Errno::EINTR.as_usize(), (-4i32) as usize);
    assert_eq!(Errno::EAGAIN.as_usize(), (-11i32) as usize);
    assert_eq!(Errno::Other(42).as_usize(), (-42i32) as usize);
}

// ── From trait ────────────────────────────────────────────────────

#[ktest]
fn from_i32_trait_matches_from_i32() {
    for code in -50..150 {
        let a: Errno = Errno::from_i32(code);
        let b: Errno = code.into();
        assert_eq!(a, b, "From<i32> mismatch for code {}", code);
    }
}

#[ktest]
fn into_i32_matches_as_i32() {
    let codes: &[i32] = &[0, 1, 2, -1, 13, 22, 38, 110, 111, 999];
    for &code in codes {
        let e = Errno::from_i32(code);
        let into_val: i32 = e.into();
        assert_eq!(into_val, e.as_i32(), "Into<i32> mismatch for code {}", code);
    }
}
