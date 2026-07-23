//! Dentry 预算驱逐与文件系统能力测试。

extern crate std;

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use ktest::ktest;
use std::sync::Barrier;
use std::thread;

use crate::cred::{Credentials, Gid, Uid};
use crate::dentry::{Dentry, DentryCache};
use crate::error::{VfsError, VfsResult};
use crate::file::{FileOps, OpenOptions};
use crate::inode::{Inode, InodeId, InodeMeta, InodeOps};
use crate::mount::MountFlags;
use crate::stat::{DevId, FileMode, FileType, FsId, FsStat, Timespec};
use crate::superblock::{InodeCache, Superblock, SuperblockOps};

struct EmptyInodeOps;

impl InodeOps for EmptyInodeOps {
    fn lookup(&self, _inode: &Inode, _name: &str) -> VfsResult<Arc<Inode>> {
        Err(VfsError::NotFound)
    }

    fn open(
        &self,
        _inode: &Inode,
        _opts: &OpenOptions,
        _cred: &Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        Err(VfsError::NotSupported)
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

struct DefaultSuperblockOps;

impl SuperblockOps for DefaultSuperblockOps {
    fn alloc_inode(&self, _sb: &Arc<Superblock>) -> VfsResult<Arc<Inode>> {
        Err(VfsError::NotSupported)
    }

    fn write_inode(&self, _inode: &Arc<Inode>) -> VfsResult<()> {
        Ok(())
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn statfs(&self, _sb: &Arc<Superblock>) -> VfsResult<FsStat> {
        Err(VfsError::NotSupported)
    }

    fn sync_fs(&self, _sb: &Arc<Superblock>) -> VfsResult<()> {
        Ok(())
    }

    fn remount(&self, _sb: &Arc<Superblock>, _new_flags: MountFlags) -> VfsResult<()> {
        Ok(())
    }
}

struct PersistentSuperblockOps;

impl SuperblockOps for PersistentSuperblockOps {
    fn alloc_inode(&self, _sb: &Arc<Superblock>) -> VfsResult<Arc<Inode>> {
        Err(VfsError::NotSupported)
    }

    fn write_inode(&self, _inode: &Arc<Inode>) -> VfsResult<()> {
        Ok(())
    }

    fn can_evict_positive_dentry(&self) -> bool {
        true
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn statfs(&self, _sb: &Arc<Superblock>) -> VfsResult<FsStat> {
        Err(VfsError::NotSupported)
    }

    fn sync_fs(&self, _sb: &Arc<Superblock>) -> VfsResult<()> {
        Ok(())
    }

    fn remount(&self, _sb: &Arc<Superblock>, _new_flags: MountFlags) -> VfsResult<()> {
        Ok(())
    }
}

fn inode_meta(kind: FileType) -> InodeMeta {
    InodeMeta {
        size: 0,
        nlink: if kind == FileType::Directory { 2 } else { 1 },
        mode: FileMode::new(0o755),
        uid: Uid(0),
        gid: Gid(0),
        atime: Timespec::ZERO,
        mtime: Timespec::ZERO,
        ctime: Timespec::ZERO,
        blocks: 0,
    }
}

fn test_superblock(
    ops: Box<dyn SuperblockOps + Send + Sync>,
    dev_id: Option<DevId>,
) -> Arc<Superblock> {
    let fs_id = FsId::new(0xdeca_f000);
    Superblock::new(move |weak| {
        let root_inode = Inode::new(
            InodeId { fs_id, ino: 1 },
            FileType::Directory,
            DevId::new(0, 0),
            4096,
            dev_id,
            inode_meta(FileType::Directory),
            Arc::new(EmptyInodeOps),
            weak.clone(),
        );
        let root_dentry = Dentry::new_positive("", None, Arc::clone(&root_inode));
        Superblock {
            fs_type: "dentry-test",
            fs_id,
            dev_id,
            block_size: 4096,
            name_max: 255,
            root_inode,
            root_dentry,
            inode_cache: InodeCache::new(),
            ops,
            self_weak: weak,
        }
    })
}

fn positive_child(sb: &Arc<Superblock>, name: &str, ino: u64) -> Arc<Dentry> {
    let inode = Inode::new(
        InodeId {
            fs_id: sb.fs_id,
            ino,
        },
        FileType::Regular,
        DevId::new(0, 0),
        4096,
        sb.dev_id,
        inode_meta(FileType::Regular),
        Arc::new(EmptyInodeOps),
        Arc::downgrade(sb),
    );
    Dentry::new_positive(name, Some(Arc::clone(&sb.root_dentry)), inode)
}

fn directory_child(sb: &Arc<Superblock>, name: &str, ino: u64) -> Arc<Dentry> {
    let inode = Inode::new(
        InodeId {
            fs_id: sb.fs_id,
            ino,
        },
        FileType::Directory,
        DevId::new(0, 0),
        4096,
        sb.dev_id,
        inode_meta(FileType::Directory),
        Arc::new(EmptyInodeOps),
        Arc::downgrade(sb),
    );
    Dentry::new_positive(name, Some(Arc::clone(&sb.root_dentry)), inode)
}

fn shard_index(parent: &Arc<Dentry>, name: &str) -> usize {
    const FNV_OFFSET: u64 = 14_695_981_039_346_656_037;
    const FNV_PRIME: u64 = 1_099_511_628_211;
    let mut hash = FNV_OFFSET;
    for byte in (Arc::as_ptr(parent) as usize).to_ne_bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    for byte in name.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash as usize & 15
}

fn names_in_one_shard(parent: &Arc<Dentry>, count: usize) -> Vec<String> {
    let target = shard_index(parent, "budget-0");
    let mut names = Vec::with_capacity(count);
    let mut candidate = 0usize;
    while names.len() < count {
        let name = format!("budget-{candidate}");
        if shard_index(parent, &name) == target {
            names.push(name);
        }
        candidate += 1;
    }
    names
}

#[ktest]
fn positive_eviction_capability_is_independent_from_dev_id() {
    let persistent = test_superblock(Box::new(PersistentSuperblockOps), None);
    let persistent_child = positive_child(&persistent, "persistent", 2);
    assert!(persistent.dev_id.is_none());
    assert!(persistent_child.is_evictable());

    let memory = test_superblock(Box::new(DefaultSuperblockOps), Some(DevId::new(8, 1)));
    let memory_child = positive_child(&memory, "memory", 2);
    assert!(memory.dev_id.is_some());
    assert!(!memory_child.is_evictable());
}

#[ktest]
fn budget_eviction_preserves_externally_referenced_positive_dentry() {
    const SHARD_BUDGET: usize = 1024;

    let sb = test_superblock(Box::new(PersistentSuperblockOps), None);
    let cache = DentryCache::new();
    let names = names_in_one_shard(&sb.root_dentry, SHARD_BUDGET + 1);

    let protected = cache.insert(positive_child(&sb, &names[0], 2));
    assert!(!protected.is_evictable());
    for (index, name) in names.iter().enumerate().take(SHARD_BUDGET).skip(1) {
        drop(cache.insert(positive_child(&sb, name, index as u64 + 2)));
    }
    assert_eq!(cache.len(), SHARD_BUDGET);

    drop(cache.insert(positive_child(
        &sb,
        &names[SHARD_BUDGET],
        SHARD_BUDGET as u64 + 2,
    )));

    assert_eq!(cache.len(), SHARD_BUDGET);
    let cached_protected = cache
        .get(&sb.root_dentry, &names[0])
        .expect("externally referenced dentry must stay cached");
    assert!(Arc::ptr_eq(&cached_protected, &protected));
    assert!(cache.get(&sb.root_dentry, &names[SHARD_BUDGET]).is_some());
    let evicted = names[1..SHARD_BUDGET]
        .iter()
        .filter(|name| cache.get(&sb.root_dentry, name).is_none())
        .count();
    assert_eq!(evicted, 1);
}

#[ktest]
fn removed_directory_drops_cached_negative_children() {
    let sb = test_superblock(Box::new(PersistentSuperblockOps), None);
    let cache = DentryCache::new();
    let directory = cache.insert(directory_child(&sb, "removed", 2));
    drop(cache.insert(Dentry::new_negative(
        "missing",
        Some(Arc::clone(&directory)),
    )));

    assert_eq!(cache.len(), 2);
    assert_eq!(Arc::strong_count(&directory), 3);

    cache.invalidate_removed_directory(&directory);

    assert!(directory.is_invalid());
    assert_eq!(cache.len(), 0);
    assert_eq!(Arc::strong_count(&directory), 1);
}

#[ktest]
fn removed_directory_rejects_late_negative_child() {
    let sb = test_superblock(Box::new(PersistentSuperblockOps), None);
    let cache = DentryCache::new();
    let directory = cache.insert(directory_child(&sb, "removed", 2));

    cache.invalidate_removed_directory(&directory);
    let late = cache.insert(Dentry::new_negative("late", Some(Arc::clone(&directory))));

    assert!(late.is_negative());
    assert!(cache.get(&directory, "late").is_none());
    assert_eq!(cache.len(), 0);
}

#[ktest]
fn removed_directory_serializes_concurrent_negative_insert() {
    let sb = test_superblock(Box::new(PersistentSuperblockOps), None);
    let cache = Arc::new(DentryCache::new());
    let directory = cache.insert(directory_child(&sb, "removed", 2));
    let ready = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));

    let worker = {
        let cache = Arc::clone(&cache);
        let directory = Arc::clone(&directory);
        let ready = Arc::clone(&ready);
        let stop = Arc::clone(&stop);
        thread::spawn(move || {
            drop(cache.insert(Dentry::new_negative("racing", Some(Arc::clone(&directory)))));
            ready.store(true, Ordering::Release);
            while !stop.load(Ordering::Acquire) {
                drop(cache.insert(Dentry::new_negative("racing", Some(Arc::clone(&directory)))));
                thread::yield_now();
            }
            drop(cache.insert(Dentry::new_negative("after", Some(Arc::clone(&directory)))));
        })
    };

    while !ready.load(Ordering::Acquire) {
        thread::yield_now();
    }
    cache.invalidate_removed_directory(&directory);
    stop.store(true, Ordering::Release);
    worker.join().expect("negative insert worker must finish");

    assert!(cache.get(&directory, "racing").is_none());
    assert!(cache.get(&directory, "after").is_none());
    assert_eq!(cache.len(), 0);
}

#[ktest]
fn subtree_invalidation_serializes_rename_without_lock_inversion() {
    let sb = test_superblock(Box::new(PersistentSuperblockOps), None);
    let cache = Arc::new(DentryCache::new());
    let directory = cache.insert(directory_child(&sb, "subtree", 2));
    let child = cache.insert(positive_child(&sb, "child", 3));
    cache.rename_dentry(&child, &directory, "child");
    let start = Arc::new(Barrier::new(3));

    let invalidate_worker = {
        let cache = Arc::clone(&cache);
        let directory = Arc::clone(&directory);
        let start = Arc::clone(&start);
        thread::spawn(move || {
            start.wait();
            cache.invalidate_subtree(&directory);
        })
    };
    let rename_worker = {
        let cache = Arc::clone(&cache);
        let child = Arc::clone(&child);
        let root = Arc::clone(&sb.root_dentry);
        let start = Arc::clone(&start);
        thread::spawn(move || {
            start.wait();
            cache.rename_dentry(&child, &root, "moved");
        })
    };

    start.wait();
    invalidate_worker
        .join()
        .expect("subtree invalidation worker must finish");
    rename_worker.join().expect("rename worker must finish");

    assert!(cache.get(&directory, "child").is_none());
    let moved = cache.get(&sb.root_dentry, "moved");
    assert!(moved.as_ref().is_none_or(|dentry| dentry.is_positive()));
    assert_eq!(cache.len(), usize::from(moved.is_some()));
}

#[ktest]
fn rename_to_invalid_parent_drops_stale_source_key() {
    let sb = test_superblock(Box::new(PersistentSuperblockOps), None);
    let cache = DentryCache::new();
    let invalid_parent = cache.insert(directory_child(&sb, "gone", 2));
    let source = cache.insert(positive_child(&sb, "source", 3));

    cache.invalidate_removed_directory(&invalid_parent);
    cache.rename_dentry(&source, &invalid_parent, "target");

    assert!(source.is_invalid());
    assert!(cache.get(&sb.root_dentry, "source").is_none());
    assert!(cache.get(&invalid_parent, "target").is_none());
    assert_eq!(cache.len(), 0);
}

#[ktest]
fn positive_insert_waits_for_unrelated_invalidation() {
    let sb = test_superblock(Box::new(DefaultSuperblockOps), Some(DevId::new(8, 1)));
    let cache = Arc::new(DentryCache::new());
    let ready = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let started = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));

    let invalidation_worker = {
        let cache = Arc::clone(&cache);
        let ready = Arc::clone(&ready);
        let release = Arc::clone(&release);
        thread::spawn(move || cache.hold_invalidation_for_test(&ready, &release))
    };
    while !ready.load(Ordering::Acquire) {
        thread::yield_now();
    }

    let insert_worker = {
        let cache = Arc::clone(&cache);
        let child = positive_child(&sb, "tmpfs-like", 2);
        let started = Arc::clone(&started);
        let done = Arc::clone(&done);
        thread::spawn(move || {
            started.store(true, Ordering::Release);
            let inserted = cache.insert(child);
            done.store(true, Ordering::Release);
            inserted
        })
    };
    while !started.load(Ordering::Acquire) {
        thread::yield_now();
    }
    for _ in 0..32 {
        thread::yield_now();
    }
    assert!(!done.load(Ordering::Acquire));

    release.store(true, Ordering::Release);
    invalidation_worker
        .join()
        .expect("invalidation worker must finish");
    let inserted = insert_worker.join().expect("positive insert must finish");

    assert!(inserted.is_positive());
    assert!(
        cache
            .get(&sb.root_dentry, "tmpfs-like")
            .is_some_and(|cached| Arc::ptr_eq(&cached, &inserted))
    );
    assert_eq!(cache.len(), 1);
}
