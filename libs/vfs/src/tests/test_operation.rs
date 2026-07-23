use alloc::boxed::Box;
use alloc::sync::{Arc, Weak};

use crate::cred::{CapSet, Capability, Credentials, Gid, Uid};
use crate::error::VfsError;
use crate::inode::{Inode, InodeId, InodeMeta, InodeOps};
use crate::operation::derive_create_attributes;
use crate::stat::{DevId, FileMode, FileType, FsId, Timespec};
use ktest::ktest;

struct EmptyInodeOps;

impl InodeOps for EmptyInodeOps {
    fn supports_private_page_cache(&self) -> bool {
        true
    }

    fn lookup(&self, _inode: &Inode, _name: &str) -> crate::error::VfsResult<Arc<Inode>> {
        Err(VfsError::NotFound)
    }

    fn open(
        &self,
        _inode: &Inode,
        _opts: &crate::file::OpenOptions,
        _cred: &Credentials,
    ) -> crate::error::VfsResult<Box<dyn crate::file::FileOps + Send + Sync>> {
        Err(VfsError::NotSupported)
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

fn regular_inode() -> Arc<Inode> {
    Inode::new(
        InodeId {
            fs_id: FsId::new(1),
            ino: 1,
        },
        FileType::Regular,
        DevId::new(0, 0),
        4096,
        None,
        parent(1000, 0o755),
        Arc::new(EmptyInodeOps),
        Weak::new(),
    )
}

fn caller(gid: u32, groups: &[u32], caps: CapSet) -> Credentials {
    let mut cred = Credentials::unprivileged(Uid(1000), Gid(gid));
    cred.groups = groups.iter().copied().map(Gid).collect();
    cred.caps = caps;
    cred
}

fn parent(gid: u32, mode: u16) -> InodeMeta {
    InodeMeta {
        size: 0,
        nlink: 2,
        mode: FileMode::new(mode),
        uid: Uid(1000),
        gid: Gid(gid),
        atime: Timespec::default(),
        mtime: Timespec::default(),
        ctime: Timespec::default(),
        blocks: 0,
    }
}

#[ktest]
fn create_in_setgid_directory_inherits_group() {
    let (mode, owner) = derive_create_attributes(
        FileMode::new(0o777),
        &caller(1000, &[], CapSet::EMPTY),
        &parent(2000, 0o2777),
        FileType::Regular,
    );
    assert_eq!(owner.fsgid, Gid(2000));
    assert_eq!(mode.bits(), 0o777);
}

#[ktest]
fn child_directory_inherits_setgid_bit() {
    let (mode, owner) = derive_create_attributes(
        FileMode::new(0o755),
        &caller(1000, &[], CapSet::EMPTY),
        &parent(2000, 0o2777),
        FileType::Directory,
    );
    assert_eq!(owner.fsgid, Gid(2000));
    assert!(mode.has(FileMode::ISGID));
}

#[ktest]
fn requested_setgid_requires_final_group_or_capability() {
    let plain_parent = parent(1000, 0o777);
    let (member_mode, _) = derive_create_attributes(
        FileMode::new(0o2777),
        &caller(1000, &[], CapSet::EMPTY),
        &plain_parent,
        FileType::Regular,
    );
    assert!(member_mode.has(FileMode::ISGID));

    let setgid_parent = parent(2000, 0o2777);
    let (stripped_mode, _) = derive_create_attributes(
        FileMode::new(0o2777),
        &caller(1000, &[], CapSet::EMPTY),
        &setgid_parent,
        FileType::Regular,
    );
    assert!(!stripped_mode.has(FileMode::ISGID));

    let privileged = CapSet::single(Capability::FSetId);
    let (privileged_mode, _) = derive_create_attributes(
        FileMode::new(0o2777),
        &caller(1000, &[], privileged),
        &setgid_parent,
        FileType::Regular,
    );
    assert!(privileged_mode.has(FileMode::ISGID));
}

#[ktest]
fn executable_access_rejects_writers_until_last_lease_drops() {
    let inode = regular_inode();
    let first = inode.acquire_exec_access().unwrap();
    let second = inode.acquire_exec_access().unwrap();

    assert_eq!(
        inode.acquire_write_access().err(),
        Some(VfsError::TextFileBusy)
    );
    drop(first);
    assert_eq!(
        inode.acquire_write_access().err(),
        Some(VfsError::TextFileBusy)
    );
    drop(second);

    let writer = inode.acquire_write_access().unwrap();
    drop(writer);
}

#[ktest]
fn writable_access_rejects_exec_until_last_lease_drops() {
    let inode = regular_inode();
    let first = inode.acquire_write_access().unwrap();
    let second = inode.acquire_write_access().unwrap();

    assert_eq!(
        inode.acquire_exec_access().err(),
        Some(VfsError::TextFileBusy)
    );
    drop(first);
    assert_eq!(
        inode.acquire_exec_access().err(),
        Some(VfsError::TextFileBusy)
    );
    drop(second);

    let executable = inode.acquire_exec_access().unwrap();
    drop(executable);
}

#[ktest]
fn inode_data_mutation_stays_unstable_until_last_guard_drops() {
    let inode = regular_inode();
    let initial = inode.data_generation();

    let first = inode.begin_data_mutation();
    let second = inode.begin_data_mutation();
    assert_eq!(inode.private_page_cache_generation(), None);

    drop(first);
    assert_eq!(inode.private_page_cache_generation(), None);
    drop(second);

    assert_eq!(inode.data_generation(), initial + 2);
    assert_eq!(inode.private_page_cache_generation(), Some(initial + 2));
}

#[ktest]
fn writable_shared_mapping_permanently_disables_private_cache() {
    let inode = regular_inode();
    let initial = inode.data_generation();

    inode.disable_private_page_cache();
    assert_eq!(inode.private_page_cache_generation(), None);

    let mutation = inode.begin_data_mutation();
    drop(mutation);
    assert_eq!(inode.private_page_cache_generation(), None);
    assert_eq!(inode.data_generation(), initial + 2);
}

#[ktest]
fn private_page_cache_identity_is_unique_across_inode_lifetimes() {
    let first = regular_inode();
    let second = regular_inode();
    let first_key = first.private_page_cache_key();
    let second_key = second.private_page_cache_key();

    assert!(first_key.is_some());
    assert_ne!(first_key, second_key);

    drop(first);
    let replacement = regular_inode();
    assert_ne!(first_key, replacement.private_page_cache_key());
    assert_ne!(second_key, replacement.private_page_cache_key());
}
