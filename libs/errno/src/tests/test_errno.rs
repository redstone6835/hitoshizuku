#[cfg(feature = "ktest-kernel")]
extern crate alloc;
#[cfg(not(feature = "ktest-kernel"))]
extern crate std;

use crate::Errno;
use ktest::ktest;

// ── from_i32 ──────────────────────────────────────────────────────

#[ktest]
fn from_i32_known_codes() {
    assert_eq!(Errno::from_i32(0), Errno::ESUCCESS);
    assert_eq!(Errno::from_i32(1), Errno::EPERM);
    assert_eq!(Errno::from_i32(2), Errno::ENOENT);
    assert_eq!(Errno::from_i32(3), Errno::ESRCH);
    assert_eq!(Errno::from_i32(4), Errno::EINTR);
    assert_eq!(Errno::from_i32(5), Errno::EIO);
    assert_eq!(Errno::from_i32(8), Errno::ENOEXEC);
    assert_eq!(Errno::from_i32(9), Errno::EBADF);
    assert_eq!(Errno::from_i32(10), Errno::ECHILD);
    assert_eq!(Errno::from_i32(11), Errno::EAGAIN);
    assert_eq!(Errno::from_i32(12), Errno::ENOMEM);
    assert_eq!(Errno::from_i32(13), Errno::EACCES);
    assert_eq!(Errno::from_i32(14), Errno::EFAULT);
    assert_eq!(Errno::from_i32(16), Errno::EBUSY);
    assert_eq!(Errno::from_i32(17), Errno::EEXIST);
    assert_eq!(Errno::from_i32(18), Errno::EXDEV);
    assert_eq!(Errno::from_i32(19), Errno::ENODEV);
    assert_eq!(Errno::from_i32(20), Errno::ENOTDIR);
    assert_eq!(Errno::from_i32(21), Errno::EISDIR);
    assert_eq!(Errno::from_i32(23), Errno::ENFILE);
    assert_eq!(Errno::from_i32(24), Errno::EMFILE);
    assert_eq!(Errno::from_i32(25), Errno::ENOTTY);
    assert_eq!(Errno::from_i32(31), Errno::EMLINK);
    assert_eq!(Errno::from_i32(32), Errno::EPIPE);
    assert_eq!(Errno::from_i32(22), Errno::EINVAL);
    assert_eq!(Errno::from_i32(27), Errno::EFBIG);
    assert_eq!(Errno::from_i32(28), Errno::ENOSPC);
    assert_eq!(Errno::from_i32(30), Errno::EROFS);
    assert_eq!(Errno::from_i32(34), Errno::ERANGE);
    assert_eq!(Errno::from_i32(36), Errno::ENAMETOOLONG);
    assert_eq!(Errno::from_i32(38), Errno::ENOSYS);
    assert_eq!(Errno::from_i32(39), Errno::ENOTEMPTY);
    assert_eq!(Errno::from_i32(40), Errno::ELOOP);
    assert_eq!(Errno::from_i32(95), Errno::EOPNOTSUPP);
    assert_eq!(Errno::from_i32(97), Errno::EAFNOSUPPORT);
    assert_eq!(Errno::from_i32(104), Errno::ECONNRESET);
    assert_eq!(Errno::from_i32(110), Errno::ETIMEDOUT);
    assert_eq!(Errno::from_i32(111), Errno::ECONNREFUSED);
}

#[ktest]
fn from_i32_other() {
    assert_eq!(Errno::from_i32(-1), Errno::Other(-1));
    assert_eq!(Errno::from_i32(999), Errno::Other(999));
    assert_eq!(Errno::from_i32(-999), Errno::Other(-999));
}

#[ktest]
fn from_i32_unknown_positive() {
    assert_eq!(Errno::from_i32(100), Errno::Other(100));
}

// ── as_i32 ────────────────────────────────────────────────────────

#[ktest]
fn as_i32_roundtrip_known() {
    let codes: &[i32] = &[
        0, 1, 2, 3, 4, 5, 8, 9, 10, 11, 12, 13, 14, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 27, 28,
        30, 31, 32, 34, 36, 38, 39, 40, 95, 97, 104, 110, 111,
    ];
    for &code in codes {
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
