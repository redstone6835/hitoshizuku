#[cfg(feature = "ktest-kernel")]
extern crate alloc;
#[cfg(not(feature = "ktest-kernel"))]
extern crate std;

mod test_vm_area;
mod test_vm_flags;
mod test_vma_set;
