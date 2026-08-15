//! fsnotify 事件核心：inode 监视注册表与事件注入。
//!
//! 与 Linux 的 fsnotify 一样，inotify/fanotify 共享同一套"inode → 监视"
//! 注册表与投递机制。本模块只负责标记与分发；每个监视指向一个通知实例
//! （inotify 实例 / fanotify 组），由实例实现排队与读取语义。
//!
//! 性能：全局 `NOTIFY_ENABLED` 门控——没有任何监视时所有注入点只做一次
//! 原子读；监视存在时按 `(fs_id, ino)` 查表（BTreeMap）。

use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::inode::Inode;
use crate::vfs::stat::FileType;
use crate::vfs::sync::Spinlock;

// ── inotify 事件/标志常量（Linux UAPI 值）────────────────────────────────

pub const IN_ACCESS: u32 = 0x0000_0001;
pub const IN_MODIFY: u32 = 0x0000_0002;
pub const IN_ATTRIB: u32 = 0x0000_0004;
pub const IN_CLOSE_WRITE: u32 = 0x0000_0008;
pub const IN_CLOSE_NOWRITE: u32 = 0x0000_0010;
pub const IN_OPEN: u32 = 0x0000_0020;
pub const IN_MOVED_FROM: u32 = 0x0000_0040;
pub const IN_MOVED_TO: u32 = 0x0000_0080;
pub const IN_CREATE: u32 = 0x0000_0100;
pub const IN_DELETE: u32 = 0x0000_0200;
pub const IN_DELETE_SELF: u32 = 0x0000_0400;
pub const IN_MOVE_SELF: u32 = 0x0000_0800;
pub const IN_UNMOUNT: u32 = 0x0000_2000;
pub const IN_Q_OVERFLOW: u32 = 0x0000_4000;
pub const IN_IGNORED: u32 = 0x0000_8000;
pub const IN_CLOSE: u32 = IN_CLOSE_WRITE | IN_CLOSE_NOWRITE;
pub const IN_MOVE: u32 = IN_MOVED_FROM | IN_MOVED_TO;

pub const IN_ONLYDIR: u32 = 0x0100_0000;
pub const IN_DONT_FOLLOW: u32 = 0x0200_0000;
pub const IN_EXCL_UNLINK: u32 = 0x0400_0000;
pub const IN_MASK_ADD: u32 = 0x2000_0000;
pub const IN_ISDIR: u32 = 0x4000_0000;
pub const IN_ONESHOT: u32 = 0x8000_0000;

/// `add_watch` 掩码合法位（Linux 校验集）。
pub const IN_ADD_MASK: u32 = IN_ACCESS
    | IN_MODIFY
    | IN_ATTRIB
    | IN_CLOSE_WRITE
    | IN_CLOSE_NOWRITE
    | IN_OPEN
    | IN_MOVED_FROM
    | IN_MOVED_TO
    | IN_CREATE
    | IN_DELETE
    | IN_DELETE_SELF
    | IN_MOVE_SELF
    | IN_UNMOUNT
    | IN_ONLYDIR
    | IN_DONT_FOLLOW
    | IN_EXCL_UNLINK
    | IN_MASK_ADD
    | IN_ONESHOT;

/// 实例队列上限（Linux inotify 默认 16384）。
pub const INOTIFY_QUEUE_LIMIT: usize = 16384;

/// 监视目标：由 inotify 实例 / fanotify 组实现。
pub trait WatchTarget: Send + Sync {
    /// 掩码匹配时投递事件（调用方已组装 wd/mask/cookie/name）。
    fn deliver(&self, event: &NotifyEvent);
    /// 监视被移除（ONESHOT/删除/实例关闭）时通知；`ignored` 表示需排队
    /// `IN_IGNORED` 事件。
    fn on_watch_removed(&self, wd: i32, ignored: bool);
}

/// 一条监视：实例持有 `Arc`，inode 注册表持有 `Weak`。
pub struct Watch {
    pub wd: i32,
    /// 事件掩码（add_watch 的 IN_MASK_ADD 需要原子更新）。
    pub mask: AtomicU32,
    pub flags: u32,
    /// `IN_EXCL_UNLINK` 且 inode 已被 unlink：不再产生事件（重新 link 时复位）。
    pub unlinked: AtomicBool,
    pub inode: Weak<Inode>,
    pub target: Weak<dyn WatchTarget>,
}

/// 投递给监视目标的最终事件。
#[derive(Clone, Debug)]
pub struct NotifyEvent {
    pub wd: i32,
    pub mask: u32,
    pub cookie: u32,
    pub name: Vec<u8>,
}

/// inode 键：`(fs_id, ino)`（InodeId 未实现 Ord）。
type InodeKey = (u64, u64);

static NOTIFY_ENABLED: AtomicBool = AtomicBool::new(false);
static INODE_WATCHES: Spinlock<BTreeMap<InodeKey, Vec<Weak<Watch>>>> =
    Spinlock::new(BTreeMap::new());

fn inode_key(inode: &Inode) -> InodeKey {
    (inode.id.fs_id.0, inode.id.ino)
}

/// 全局门控：是否存在任何监视。
#[inline]
pub fn is_enabled() -> bool {
    NOTIFY_ENABLED.load(Ordering::Acquire)
}

/// 注册监视（inode 侧）。
pub fn register(inode: &Inode, watch: Weak<Watch>) {
    let mut map = INODE_WATCHES.lock();
    map.entry(inode_key(inode)).or_default().push(watch);
    NOTIFY_ENABLED.store(true, Ordering::Release);
}

/// 注销监视（inode 侧）。
pub fn unregister(inode: &Inode, watch: &Arc<Watch>) {
    let mut map = INODE_WATCHES.lock();
    if let Some(list) = map.get_mut(&inode_key(inode)) {
        list.retain(|w| {
            w.upgrade()
                .map(|up| !Arc::ptr_eq(&up, watch))
                .unwrap_or(false)
        });
        if list.is_empty() {
            map.remove(&inode_key(inode));
        }
    }
    if map.is_empty() {
        NOTIFY_ENABLED.store(false, Ordering::Release);
    }
}

/// 收集匹配的监视事件（不持注册表锁投递，避免实例锁交叉）。
///
/// 返回 `(target, event, ignored_wd)`；`ignored_wd = Some(wd)` 表示该监视
/// 已被移除（ONESHOT），投递后需补发 `IN_IGNORED`。
fn collect_matching(
    inode: &Inode,
    mask: u32,
    cookie: u32,
    name: Vec<u8>,
) -> Vec<(Arc<dyn WatchTarget>, NotifyEvent, Option<i32>)> {
    let mut out = Vec::new();
    let mut map = INODE_WATCHES.lock();
    let Some(list) = map.get_mut(&inode_key(inode)) else {
        return out;
    };
    let is_dir = inode.kind() == FileType::Directory;
    let mut i = 0;
    while i < list.len() {
        let Some(watch) = list[i].upgrade() else {
            list.swap_remove(i);
            continue;
        };
        if watch.unlinked.load(Ordering::Acquire) || watch.mask.load(Ordering::Acquire) & mask == 0 {
            i += 1;
            continue;
        }
        let event = NotifyEvent {
            wd: watch.wd,
            mask: mask | if is_dir { IN_ISDIR } else { 0 },
            cookie,
            name: name.clone(),
        };
        let target = watch.target.upgrade();
        let wd = watch.wd;
        let oneshot = watch.flags & IN_ONESHOT != 0;
        if oneshot {
            list.swap_remove(i);
        } else {
            i += 1;
        }
        if let Some(target) = target {
            out.push((target, event, oneshot.then_some(wd)));
        }
    }
    if list.is_empty() {
        map.remove(&inode_key(inode));
    }
    out
}

fn dispatch(collected: Vec<(Arc<dyn WatchTarget>, NotifyEvent, Option<i32>)>) {
    for (target, event, ignored_wd) in collected {
        target.deliver(&event);
        if let Some(wd) = ignored_wd {
            target.on_watch_removed(wd, true);
        }
    }
}

/// 事件注入：inode 自身事件（OPEN/CLOSE/ACCESS/MODIFY/ATTRIB/MOVE_SELF/
/// DELETE_SELF...）。
pub fn emit(inode: &Inode, mask: u32, cookie: u32) {
    if !is_enabled() {
        return;
    }
    dispatch(collect_matching(inode, mask, cookie, Vec::new()));
}

/// 事件注入：目录 + 名字（CREATE/DELETE/MOVED_FROM/MOVED_TO）。
///
/// 父目录监视按 `mask` 匹配并携带 `name`；若子对象自身被监视，DELETE 附带
/// `IN_DELETE_SELF`（`IN_EXCL_UNLINK` 时跳过并标记失效，监视保留 wd）、
/// MOVE 附带 `IN_MOVE_SELF`。DELETE_SELF 投递后监视被移除并补发
/// `IN_IGNORED`（Linux 语义）。
pub fn emit_named(parent: &Inode, child: &Inode, mask: u32, name: &[u8], cookie: u32) {
    if !is_enabled() {
        return;
    }
    let mut collected = collect_matching(parent, mask, cookie, name.to_vec());
    match mask {
        IN_DELETE => {
            // EXCL_UNLINK：监视转为失效（保留 wd，不投递 DELETE_SELF）。
            {
                let map = INODE_WATCHES.lock();
                if let Some(list) = map.get(&inode_key(child)) {
                    for w in list {
                        if let Some(watch) = w.upgrade() {
                            if watch.flags & IN_EXCL_UNLINK != 0 {
                                watch.unlinked.store(true, Ordering::Release);
                            }
                        }
                    }
                }
            }
            // 其余子监视：投递 DELETE_SELF 后移除 + IGNORED。
            let mut map = INODE_WATCHES.lock();
            if let Some(list) = map.get_mut(&inode_key(child)) {
                let mut i = 0;
                while i < list.len() {
                    let Some(watch) = list[i].upgrade() else {
                        list.swap_remove(i);
                        continue;
                    };
                    if watch.flags & IN_EXCL_UNLINK != 0
                        || watch.mask.load(Ordering::Acquire) & IN_DELETE_SELF == 0
                    {
                        i += 1;
                        continue;
                    }
                    let event = NotifyEvent {
                        wd: watch.wd,
                        mask: IN_DELETE_SELF,
                        cookie,
                        name: Vec::new(),
                    };
                    let target = watch.target.upgrade();
                    let wd = watch.wd;
                    list.swap_remove(i);
                    if let Some(target) = target {
                        collected.push((target, event, Some(wd)));
                    }
                }
            }
            if map.get(&inode_key(child)).map(|l| l.is_empty()).unwrap_or(false) {
                map.remove(&inode_key(child));
            }
        }
        IN_MOVED_FROM | IN_MOVED_TO => {
            let mut child_events = collect_matching(child, IN_MOVE_SELF, cookie, Vec::new());
            collected.append(&mut child_events);
        }
        _ => {}
    }
    dispatch(collected);
}

/// 同 [`emit_named`] 但不投递子对象自身事件（rename 的 TO 侧复用，
/// 避免 MOVE_SELF 在同一 rename 中重复投递——Linux 只发一次）。
pub fn emit_named_no_self(parent: &Inode, mask: u32, name: &[u8], cookie: u32) {
    if !is_enabled() {
        return;
    }
    dispatch(collect_matching(parent, mask, cookie, name.to_vec()));
}

/// 硬链接创建后复位 `IN_EXCL_UNLINK` 失效标记（Linux 语义：重新 link 恢复监视）。
pub fn rearm(inode: &Inode) {
    if !is_enabled() {
        return;
    }
    let map = INODE_WATCHES.lock();
    if let Some(list) = map.get(&inode_key(inode)) {
        for w in list {
            if let Some(watch) = w.upgrade() {
                watch.unlinked.store(false, Ordering::Release);
            }
        }
    }
}

/// rename 配对 cookie（MOVED_FROM/MOVED_TO 共享；全局递增）。
pub fn next_cookie() -> u32 {
    use core::sync::atomic::AtomicU32;
    static NEXT_COOKIE: AtomicU32 = AtomicU32::new(1);
    NEXT_COOKIE.fetch_add(1, Ordering::Relaxed)
}

/// 卸载文件系统：该 superblock 下所有监视收到 `IN_UNMOUNT` 并被移除。
pub fn unmount_sb(sb_id: u64) {
    if !is_enabled() {
        return;
    }
    let mut removed: Vec<(Arc<dyn WatchTarget>, NotifyEvent, Option<i32>)> = Vec::new();
    {
        let mut map = INODE_WATCHES.lock();
        let mut dead_keys = Vec::new();
        for (key, list) in map.iter_mut() {
            let mut i = 0;
            while i < list.len() {
                let Some(watch) = list[i].upgrade() else {
                    list.swap_remove(i);
                    continue;
                };
                let Some(inode) = watch.inode.upgrade() else {
                    list.swap_remove(i);
                    continue;
                };
                let same_sb = inode
                    .superblock
                    .upgrade()
                    .map(|sb| sb.fs_id.raw() == sb_id)
                    .unwrap_or(false);
                if same_sb {
                    let wd = watch.wd;
                    let target = watch.target.upgrade();
                    let event = NotifyEvent {
                        wd,
                        mask: IN_UNMOUNT | IN_ISDIR,
                        cookie: 0,
                        name: Vec::new(),
                    };
                    list.swap_remove(i);
                    if let Some(target) = target {
                        removed.push((target, event, Some(wd)));
                    }
                } else {
                    i += 1;
                }
            }
            if list.is_empty() {
                dead_keys.push(*key);
            }
        }
        for key in dead_keys {
            map.remove(&key);
        }
    }
    dispatch(removed);
}
