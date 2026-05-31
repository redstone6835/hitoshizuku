#[cfg(not(feature = "ktest-kernel"))]
extern crate std;
#[cfg(feature = "ktest-kernel")]
extern crate alloc;

mod test_errno;
