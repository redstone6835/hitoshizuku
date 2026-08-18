//! inotify 实例与 fsnotify 事件核心的宿主单测。

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::fsnotify::{self, IN_DELETE, IN_MODIFY, IN_MOVED_FROM, IN_MOVED_TO, IN_OPEN};
use crate::inode::{Inode, InodeId, InodeMeta, InodeOps};
use crate::inotify::InotifyInstance;
use crate::stat::{FileMode, FileType, Timespec};
use crate::vfs::cred::{Credentials, Gid, Uid};
use crate::vfs::error::{VfsError, VfsResult};
use crate::vfs::file::OpenOptions;

struct EmptyOps;
impl InodeOps for EmptyOps {
    fn lookup(&self, _inode: &Inode, _name: &str) -> VfsResult<Arc<Inode>> {
        Err(VfsError::NotFound)
    }
    fn open(
        &self,
        _inode: &Inode,
        _opts: &OpenOptions,
        _cred: &Credentials,
    ) -> VfsResult<Box<dyn crate::file::FileOps + Send + Sync>> {
        Err(VfsError::NotSupported)
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

fn make_inode(ino: u64, kind: FileType) -> Arc<Inode> {
    Inode::new(
        InodeId {
            fs_id: crate::stat::FsId::new(0x55aa),
            ino,
        },
        kind,
        crate::vfs::stat::DevId::new(0, 0),
        4096,
        None,
        InodeMeta {
            size: 0,
            nlink: 1,
            mode: FileMode::new(0o644),
            uid: Uid(0),
            gid: Gid(0),
            atime: Timespec::ZERO,
            mtime: Timespec::ZERO,
            ctime: Timespec::ZERO,
            blocks: 0,
        },
        Arc::new(EmptyOps),
        alloc::sync::Weak::new(),
    )
}

fn read_all(inst: &InotifyInstance) -> Vec<(i32, u32, u32, Vec<u8>)> {
    let mut out = Vec::new();
    loop {
        let mut buf = [0u8; 256];
        match inst.read_events_for_test(&mut buf) {
            Ok(n) => {
                let wd = i32::from_le_bytes(buf[0..4].try_into().unwrap());
                let mask = u32::from_le_bytes(buf[4..8].try_into().unwrap());
                let cookie = u32::from_le_bytes(buf[8..12].try_into().unwrap());
                let len = u32::from_le_bytes(buf[12..16].try_into().unwrap()) as usize;
                out.push((wd, mask, cookie, buf[16..16 + len].to_vec()));
            }
            Err(VfsError::WouldBlock) => break,
            Err(e) => panic!("read_events: {e:?}"),
        }
    }
    out
}

/// 基本投递：掩码匹配 + 读取。
#[test]
fn watch_delivers_matching_events() {
    let inst = InotifyInstance::new_for_test();
    let file = make_inode(1, FileType::Regular);
    let wd = inst.add_watch(&file, IN_MODIFY, 0).unwrap();
    assert_eq!(wd, 1);
    fsnotify::emit(&file, IN_MODIFY, 0);
    let events = read_all(&inst);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].0, 1);
    assert_eq!(events[0].1, IN_MODIFY);
    assert!(events[0].3.is_empty());
}

/// 掩码过滤：不匹配的事件不投递。
#[test]
fn watch_filters_by_mask() {
    let inst = InotifyInstance::new_for_test();
    let file = make_inode(2, FileType::Regular);
    inst.add_watch(&file, IN_OPEN, 0).unwrap();
    fsnotify::emit(&file, IN_MODIFY, 0);
    assert!(read_all(&inst).is_empty());
    fsnotify::emit(&file, IN_OPEN, 0);
    let events = read_all(&inst);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].1, IN_OPEN);
}

/// IN_MASK_ADD 合并 / 普通 add 替换。
#[test]
fn mask_add_merges_replaces() {
    let inst = InotifyInstance::new_for_test();
    let file = make_inode(3, FileType::Regular);
    let wd1 = inst.add_watch(&file, IN_OPEN, 0).unwrap();
    // 替换
    let wd2 = inst.add_watch(&file, IN_MODIFY, 0).unwrap();
    assert_eq!(wd1, wd2);
    fsnotify::emit(&file, IN_OPEN, 0);
    assert!(read_all(&inst).is_empty());
    // 合并
    inst.add_watch(&file, IN_OPEN, fsnotify::IN_MASK_ADD)
        .unwrap();
    fsnotify::emit(&file, IN_OPEN, 0);
    fsnotify::emit(&file, IN_MODIFY, 0);
    let events = read_all(&inst);
    assert_eq!(events.len(), 2);
}

/// ONESHOT：投递一次后移除并补发 IGNORED。
#[test]
fn oneshot_delivers_once_then_ignored() {
    let inst = InotifyInstance::new_for_test();
    let file = make_inode(4, FileType::Regular);
    inst.add_watch(&file, IN_MODIFY, fsnotify::IN_ONESHOT)
        .unwrap();
    fsnotify::emit(&file, IN_MODIFY, 0);
    fsnotify::emit(&file, IN_MODIFY, 0);
    let events = read_all(&inst);
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].1, IN_MODIFY);
    assert_eq!(events[1].1, fsnotify::IN_IGNORED);
}

/// rm_watch：IGNORED 事件；未知 wd → EINVAL。
#[test]
fn rm_watch_emits_ignored() {
    let inst = InotifyInstance::new_for_test();
    let file = make_inode(5, FileType::Regular);
    let wd = inst.add_watch(&file, IN_MODIFY, 0).unwrap();
    inst.rm_watch(wd).unwrap();
    assert!(inst.rm_watch(wd).is_err());
    let events = read_all(&inst);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].1, fsnotify::IN_IGNORED);
}

/// read 语义：缓冲不足 EINVAL；空队列 EAGAIN。
#[test]
fn read_buffer_semantics() {
    let inst = InotifyInstance::new_for_test();
    let file = make_inode(6, FileType::Regular);
    inst.add_watch(&file, IN_MODIFY, 0).unwrap();
    fsnotify::emit(&file, IN_MODIFY, 0);
    let mut small = [0u8; 8];
    assert_eq!(
        inst.read_events_for_test(&mut small),
        Err(VfsError::InvalidArgument)
    );
    let mut buf = [0u8; 16];
    assert_eq!(inst.read_events_for_test(&mut buf), Ok(16));
    assert_eq!(
        inst.read_events_for_test(&mut buf),
        Err(VfsError::WouldBlock)
    );
}

/// 队列溢出：溢出标志 → 读取时合成 Q_OVERFLOW。
#[test]
fn queue_overflow_synthesizes_q_overflow() {
    let inst = InotifyInstance::new_for_test();
    let file = make_inode(7, FileType::Regular);
    inst.add_watch(&file, IN_MODIFY, 0).unwrap();
    for _ in 0..fsnotify::INOTIFY_QUEUE_LIMIT + 10 {
        fsnotify::emit(&file, IN_MODIFY, 0);
    }
    let events = read_all(&inst);
    // 队列满 16384 条，第 16385 条起丢弃并置溢出；读取时队首为 Q_OVERFLOW。
    assert_eq!(events.len(), fsnotify::INOTIFY_QUEUE_LIMIT + 1);
    assert_eq!(events[0].1, fsnotify::IN_Q_OVERFLOW);
}

/// emit_named：父目录带名字；子对象 DELETE_SELF + IGNORED；EXCL_UNLINK 例外。
#[test]
fn named_events_and_delete_self() {
    let inst = InotifyInstance::new_for_test();
    let dir = make_inode(10, FileType::Directory);
    let child = make_inode(11, FileType::Regular);
    let wd_dir = inst
        .add_watch(&dir, fsnotify::IN_CREATE | fsnotify::IN_DELETE, 0)
        .unwrap();
    let wd_child = inst.add_watch(&child, fsnotify::IN_DELETE_SELF, 0).unwrap();
    fsnotify::emit_named(&dir, &child, IN_DELETE, b"kid", 0);
    let events = read_all(&inst);
    assert_eq!(events.len(), 3);
    // 父目录：IN_DELETE|IN_ISDIR + 名字
    assert_eq!(events[0].0, wd_dir);
    assert_eq!(events[0].1, IN_DELETE | fsnotify::IN_ISDIR);
    assert_eq!(events[0].3, b"kid");
    // 子对象：DELETE_SELF + IGNORED
    assert_eq!(events[1].0, wd_child);
    assert_eq!(events[1].1, fsnotify::IN_DELETE_SELF);
    assert_eq!(events[2].0, wd_child);
    assert_eq!(events[2].1, fsnotify::IN_IGNORED);

    // EXCL_UNLINK：不投递 DELETE_SELF，监视保留（无 IGNORED）。
    let inst2 = InotifyInstance::new_for_test();
    let child2 = make_inode(12, FileType::Regular);
    inst2
        .add_watch(&child2, fsnotify::IN_DELETE_SELF, fsnotify::IN_EXCL_UNLINK)
        .unwrap();
    fsnotify::emit_named(&dir, &child2, IN_DELETE, b"kid2", 0);
    assert!(read_all(&inst2).is_empty());
    // 重新 link 后恢复。
    fsnotify::rearm(&child2);
    fsnotify::emit(&child2, fsnotify::IN_MODIFY, 0);
    assert!(read_all(&inst2).is_empty()); // 监视只含 DELETE_SELF
    fsnotify::emit(&child2, fsnotify::IN_DELETE_SELF, 0);
    let events = read_all(&inst2);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].1, fsnotify::IN_DELETE_SELF);
}

/// rename 配对 cookie。
#[test]
fn rename_events_share_cookie() {
    let inst = InotifyInstance::new_for_test();
    let dir = make_inode(20, FileType::Directory);
    let file = make_inode(21, FileType::Regular);
    inst.add_watch(&dir, fsnotify::IN_MOVE, 0).unwrap();
    let cookie = fsnotify::next_cookie();
    fsnotify::emit_named(&dir, &file, IN_MOVED_FROM, b"a", cookie);
    fsnotify::emit_named(&dir, &file, IN_MOVED_TO, b"b", cookie);
    let events = read_all(&inst);
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].1, IN_MOVED_FROM | fsnotify::IN_ISDIR);
    assert_eq!(events[1].1, IN_MOVED_TO | fsnotify::IN_ISDIR);
    assert_eq!(events[0].2, cookie);
    assert_eq!(events[1].2, cookie);
    assert_eq!(events[0].3, b"a");
    assert_eq!(events[1].3, b"b");
}

/// IN_ONLYDIR 对普通文件 → ENOTDIR。
#[test]
fn onlydir_rejects_regular_file() {
    let inst = InotifyInstance::new_for_test();
    let file = make_inode(30, FileType::Regular);
    assert!(
        inst.add_watch(&file, IN_MODIFY, fsnotify::IN_ONLYDIR)
            .is_err()
    );
}
