//! SysV shm contract tests for VM-facing behavior.
//!
//! The production manager lives in `general::ipc::shm`, while this crate only
//! owns the `FileLike` contract used by file-backed VMA.  These host tests use a
//! small in-test model to pin the observable shm behavior expected by VM-facing
//! objects without introducing a dependency from `libs/mm` back to `general`.

#[cfg(feature = "ktest-kernel")]
extern crate alloc;
#[cfg(not(feature = "ktest-kernel"))]
extern crate std;

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use errno::Errno;
use ktest::ktest;

use crate::FileLike;

const IPC_PRIVATE: i32 = 0;
const IPC_CREAT: u32 = 0o1000;
const IPC_EXCL: u32 = 0o2000;

#[derive(Clone, Copy)]
enum ShmAccess {
    ReadOnly,
    ReadWrite,
}

struct TestShmObject {
    id: i32,
    bytes: UnsafeCell<Vec<u8>>,
}

// The scaffold runs single-threaded in host tests/ktest.  The production shm
// object should replace this with real synchronization around sparse storage.
unsafe impl Send for TestShmObject {}
unsafe impl Sync for TestShmObject {}

impl TestShmObject {
    fn new(id: i32) -> Self {
        Self {
            id,
            bytes: UnsafeCell::new(Vec::new()),
        }
    }
}

impl FileLike for TestShmObject {
    fn cache_key(&self) -> usize {
        self.id as usize
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, Errno> {
        let offset = usize::try_from(offset).map_err(|_| Errno::EINVAL)?;
        let bytes = unsafe { &*self.bytes.get() };
        buf.fill(0);
        if offset < bytes.len() {
            let available = bytes.len() - offset;
            let copied = available.min(buf.len());
            buf[..copied].copy_from_slice(&bytes[offset..offset + copied]);
        }
        Ok(buf.len())
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<usize, Errno> {
        let offset = usize::try_from(offset).map_err(|_| Errno::EINVAL)?;
        let end = offset.checked_add(buf.len()).ok_or(Errno::EINVAL)?;
        let bytes = unsafe { &mut *self.bytes.get() };
        if end > bytes.len() {
            bytes.resize(end, 0);
        }
        bytes[offset..end].copy_from_slice(buf);
        Ok(buf.len())
    }

    fn sync(&self) -> Result<(), Errno> {
        Ok(())
    }

    fn size(&self) -> u64 {
        unsafe { (&*self.bytes.get()).len() as u64 }
    }

    fn is_sysv_shm(&self) -> bool {
        true
    }

    fn sysv_shm_id(&self) -> Option<i32> {
        Some(self.id)
    }
}

struct TestSegment {
    key: Option<i32>,
    size: usize,
    mode: u16,
    marked_removed: bool,
    attaches: usize,
    object: Arc<TestShmObject>,
}

struct TestShmManager {
    next_id: i32,
    segments: BTreeMap<i32, TestSegment>,
    keyed: BTreeMap<i32, i32>,
}

impl TestShmManager {
    fn new() -> Self {
        Self {
            next_id: 1,
            segments: BTreeMap::new(),
            keyed: BTreeMap::new(),
        }
    }

    fn shmget(&mut self, key: i32, size: usize, flags: u32, mode: u16) -> Result<i32, Errno> {
        if size == 0 {
            return Err(Errno::EINVAL);
        }

        if key != IPC_PRIVATE {
            if let Some(id) = self.keyed.get(&key).copied() {
                if flags & IPC_CREAT != 0 && flags & IPC_EXCL != 0 {
                    return Err(Errno::EEXIST);
                }
                let segment = self.segments.get(&id).ok_or(Errno::ENOENT)?;
                if size > segment.size {
                    return Err(Errno::EINVAL);
                }
                return Ok(id);
            }
            if flags & IPC_CREAT == 0 {
                return Err(Errno::ENOENT);
            }
        }

        let id = self.next_id;
        self.next_id += 1;
        let segment = TestSegment {
            key: (key != IPC_PRIVATE).then_some(key),
            size,
            mode,
            marked_removed: false,
            attaches: 0,
            object: Arc::new(TestShmObject::new(id)),
        };
        if let Some(key) = segment.key {
            self.keyed.insert(key, id);
        }
        self.segments.insert(id, segment);
        Ok(id)
    }

    fn attach(&mut self, id: i32, access: ShmAccess) -> Result<Arc<TestShmObject>, Errno> {
        let segment = self.segments.get_mut(&id).ok_or(Errno::ENOENT)?;
        let read_allowed = segment.mode & 0o400 != 0;
        let write_allowed = segment.mode & 0o200 != 0;
        match access {
            ShmAccess::ReadOnly if !read_allowed => return Err(Errno::EACCES),
            ShmAccess::ReadWrite if !(read_allowed && write_allowed) => return Err(Errno::EACCES),
            _ => {}
        }
        segment.attaches += 1;
        Ok(segment.object.clone())
    }

    fn detach(&mut self, id: i32) -> Result<(), Errno> {
        let segment = self.segments.get_mut(&id).ok_or(Errno::ENOENT)?;
        segment.attaches = segment.attaches.checked_sub(1).ok_or(Errno::EINVAL)?;
        if segment.attaches == 0 && segment.marked_removed {
            self.segments.remove(&id);
        }
        Ok(())
    }

    fn rmid(&mut self, id: i32) -> Result<(), Errno> {
        let segment = self.segments.get_mut(&id).ok_or(Errno::ENOENT)?;
        segment.marked_removed = true;
        if let Some(key) = segment.key.take() {
            self.keyed.remove(&key);
        }
        if segment.attaches == 0 {
            self.segments.remove(&id);
        }
        Ok(())
    }

    fn contains(&self, id: i32) -> bool {
        self.segments.contains_key(&id)
    }
}

#[ktest]
fn ipc_private_creates_unique_segments() {
    let mut manager = TestShmManager::new();

    let first = manager.shmget(IPC_PRIVATE, 4096, 0, 0o600).unwrap();
    let second = manager.shmget(IPC_PRIVATE, 4096, 0, 0o600).unwrap();

    assert_ne!(first, second);
    assert!(manager.contains(first));
    assert!(manager.contains(second));
}

#[ktest]
fn keyed_create_reuse_and_excl_behavior() {
    let mut manager = TestShmManager::new();

    assert_eq!(manager.shmget(7, 4096, 0, 0o600), Err(Errno::ENOENT));
    let created = manager.shmget(7, 4096, IPC_CREAT, 0o600).unwrap();

    assert_eq!(manager.shmget(7, 4096, 0, 0o600), Ok(created));
    assert_eq!(
        manager.shmget(7, 4096, IPC_CREAT | IPC_EXCL, 0o600),
        Err(Errno::EEXIST)
    );
}

#[ktest]
fn requested_size_larger_than_existing_returns_einval() {
    let mut manager = TestShmManager::new();
    let created = manager.shmget(9, 4096, IPC_CREAT, 0o600).unwrap();

    assert_eq!(manager.shmget(9, 4096, 0, 0o600), Ok(created));
    assert_eq!(manager.shmget(9, 4097, 0, 0o600), Err(Errno::EINVAL));
}

#[ktest]
fn ipc_rmid_deletes_only_after_last_detach() {
    let mut manager = TestShmManager::new();
    let id = manager.shmget(11, 4096, IPC_CREAT, 0o600).unwrap();
    let object = manager.attach(id, ShmAccess::ReadWrite).unwrap();

    manager.rmid(id).unwrap();
    assert!(manager.contains(id));
    assert_eq!(manager.shmget(11, 4096, 0, 0o600), Err(Errno::ENOENT));

    assert_eq!(object.write_at(32, b"live"), Ok(4));
    let mut buf = [0; 4];
    assert_eq!(object.read_at(32, &mut buf), Ok(4));
    assert_eq!(&buf, b"live");

    manager.detach(id).unwrap();
    assert!(!manager.contains(id));
}

#[ktest]
fn attach_checks_read_and_write_permissions() {
    let mut manager = TestShmManager::new();
    let readonly = manager.shmget(IPC_PRIVATE, 4096, 0, 0o400).unwrap();
    let writeonly = manager.shmget(IPC_PRIVATE, 4096, 0, 0o200).unwrap();

    assert!(manager.attach(readonly, ShmAccess::ReadOnly).is_ok());
    assert_eq!(
        manager.attach(readonly, ShmAccess::ReadWrite).map(|_| ()),
        Err(Errno::EACCES)
    );
    assert_eq!(
        manager.attach(writeonly, ShmAccess::ReadOnly).map(|_| ()),
        Err(Errno::EACCES)
    );
}

#[ktest]
fn shm_filelike_sparse_read_zero_fill_and_write_persistence() {
    let object = TestShmObject::new(42);
    let mut zeros = [0xaa; 8];

    assert_eq!(object.read_at(128, &mut zeros), Ok(8));
    assert_eq!(zeros, [0; 8]);

    assert_eq!(object.write_at(4, b"mygo"), Ok(4));
    let mut buf = [0xff; 10];
    assert_eq!(object.read_at(0, &mut buf), Ok(10));
    assert_eq!(&buf, &[0, 0, 0, 0, b'm', b'y', b'g', b'o', 0, 0]);
    assert_eq!(object.size(), 8);
    assert!(object.is_sysv_shm());
    assert_eq!(object.sysv_shm_id(), Some(42));
}
