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

/// fanotify FAN_EVENT_ON_CHILD（与 inotify 常量数值一致）。
pub const FAN_EVENT_ON_CHILD: u32 = 0x0800_0000;
/// fanotify 权限事件位（与 inotify 常量同源）。
pub const FAN_OPEN_EXEC: u32 = 0x0000_1000;
pub const FAN_OPEN_PERM: u32 = 0x0001_0000;
pub const FAN_ACCESS_PERM: u32 = 0x0002_0000;
pub const FAN_OPEN_EXEC_PERM: u32 = 0x0004_0000;

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
    /// 权限事件：生成事件并阻塞当前任务直到用户态响应；`Ok(true)`=允许、
    /// `Ok(false)`=拒绝、`Err(EINTR)`=被信号中断。默认实现不阻塞（直接放行）。
    fn await_permission(
        &self,
        _inode: &Arc<Inode>,
        _mount: Option<&Arc<crate::mount::Mount>>,
        _mask: u32,
    ) -> Result<bool, errno::Errno> {
        Ok(true)
    }
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
    /// 事件 fd 打开用的 dentry/mount（fanotify；inotify 可留空）。
    pub dentry: Weak<crate::vfs::dentry::Dentry>,
    pub mount: Weak<crate::mount::Mount>,
    pub target: Weak<dyn WatchTarget>,
    /// 监视作用域（inode/mount/filesystem；fanotify 标记）。
    pub scope: WatchScope,
    /// fanotify FAN_MARK_IGNORED_MASK：命中时抑制投递。
    pub ignored_mask: AtomicU32,
    /// 该监视是否携带权限事件（PERM 门控维护）。
    pub perm: bool,
    /// fanotify：命名（子对象）事件要求监视掩码带 FAN_EVENT_ON_CHILD。
    pub named_requires_echild: bool,
}

/// 投递给监视目标的最终事件。
#[derive(Clone, Debug)]
pub struct NotifyEvent {
    pub wd: i32,
    pub mask: u32,
    pub cookie: u32,
    pub name: Vec<u8>,
    /// 事件对象 inode（fanotify 读取时打开事件 fd 用；弱引用）。
    pub obj_inode: Option<Weak<Inode>>,
    /// 事件对象所在 mount（fanotify 事件 fd 打开用）。
    pub obj_mount: Option<Weak<crate::mount::Mount>>,
}

/// inode 键：`(fs_id, ino)`（InodeId 未实现 Ord）。
type InodeKey = (u64, u64);

/// 监视作用域（Linux fanotify：inode / mount / filesystem 三类标记）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WatchScope {
    Inode,
    Mount,
    Filesystem,
}

/// 注册表键：inode 用 `(fs_id, ino)`、mount 用 Mount 对象地址、文件系统用 fs_id。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) enum WatchKey {
    Inode(InodeKey),
    Mount(usize),
    Filesystem(u64),
}

static NOTIFY_ENABLED: AtomicBool = AtomicBool::new(false);
static PERM_ENABLED: AtomicBool = AtomicBool::new(false);
static WATCHES: Spinlock<BTreeMap<WatchKey, Vec<Weak<Watch>>>> = Spinlock::new(BTreeMap::new());

fn inode_key(inode: &Inode) -> InodeKey {
    (inode.id.fs_id.0, inode.id.ino)
}

impl WatchKey {
    /// 由作用域与目标构造注册表键。
    pub(crate) fn from_scope(
        scope: WatchScope,
        inode: Option<&Arc<Inode>>,
        mount: Option<&Arc<crate::mount::Mount>>,
        sb_id: u64,
    ) -> WatchKey {
        match scope {
            WatchScope::Inode => WatchKey::Inode(inode_key(inode.expect("inode scope 需要 inode"))),
            WatchScope::Mount => {
                let m = mount.expect("mount scope 需要 mount");
                WatchKey::Mount(Arc::as_ptr(m) as usize)
            }
            WatchScope::Filesystem => WatchKey::Filesystem(sb_id),
        }
    }
}

#[allow(dead_code)]
fn watch_key(
    scope: WatchScope,
    inode: Option<&Inode>,
    mount: Option<&crate::mount::Mount>,
    sb_id: u64,
) -> WatchKey {
    match scope {
        WatchScope::Inode => WatchKey::Inode(inode_key(inode.expect("inode scope 需要 inode"))),
        WatchScope::Mount => {
            WatchKey::Mount(mount.expect("mount scope 需要 mount") as *const _ as usize)
        }
        WatchScope::Filesystem => WatchKey::Filesystem(sb_id),
    }
}

/// 有权限事件监视（PERM 门控：普通注入路径零开销）。
#[inline]
pub fn perm_enabled() -> bool {
    PERM_ENABLED.load(Ordering::Acquire)
}

/// 全局门控：是否存在任何监视。
#[inline]
pub fn is_enabled() -> bool {
    NOTIFY_ENABLED.load(Ordering::Acquire)
}

/// 注册监视（按作用域键）。
pub fn register_key(key: WatchKey, watch: Weak<Watch>, perm: bool) {
    let mut map = WATCHES.lock();
    map.entry(key).or_default().push(watch);
    NOTIFY_ENABLED.store(true, Ordering::Release);
    if perm {
        PERM_ENABLED.store(true, Ordering::Release);
    }
}

/// 注册 inode 作用域监视（inotify 使用）。
pub fn register(inode: &Inode, watch: Weak<Watch>) {
    register_key(WatchKey::Inode(inode_key(inode)), watch, false);
}

/// 注销监视（按作用域键）。
pub fn unregister_key(key: WatchKey, watch: &Arc<Watch>) {
    let mut map = WATCHES.lock();
    if let Some(list) = map.get_mut(&key) {
        list.retain(|w| {
            w.upgrade()
                .map(|up| !Arc::ptr_eq(&up, watch))
                .unwrap_or(false)
        });
        if list.is_empty() {
            map.remove(&key);
        }
    }
    if map.is_empty() {
        NOTIFY_ENABLED.store(false, Ordering::Release);
        PERM_ENABLED.store(false, Ordering::Release);
    }
}

/// 注销 inode 作用域监视（inotify 使用）。
pub fn unregister(inode: &Inode, watch: &Arc<Watch>) {
    unregister_key(WatchKey::Inode(inode_key(inode)), watch);
}

/// 收集匹配的监视事件（不持注册表锁投递，避免实例锁交叉）。
///
/// 返回 `(target, event, ignored_wd)`；`ignored_wd = Some(wd)` 表示该监视
/// 已被移除（ONESHOT），投递后需补发 `IN_IGNORED`。
fn collect_matching(
    inode: &Inode,
    mount: Option<&Arc<crate::mount::Mount>>,
    mask: u32,
    cookie: u32,
    name: Vec<u8>,
    is_child_event: bool,
    obj_inode: Option<&Arc<Inode>>,
) -> Vec<(Arc<dyn WatchTarget>, NotifyEvent, Option<i32>)> {
    let mut out = Vec::new();
    let sb_id = inode
        .superblock
        .upgrade()
        .map(|sb| sb.fs_id.raw())
        .unwrap_or(0);
    let mount_key = mount.map(|m| WatchKey::Mount(Arc::as_ptr(m) as usize));
    let keys = [
        Some(WatchKey::Inode(inode_key(inode))),
        mount_key,
        Some(WatchKey::Filesystem(sb_id)),
    ];
    let is_dir = inode.kind() == FileType::Directory;
    let mut map = WATCHES.lock();
    for key in keys.into_iter().flatten() {
        let Some(list) = map.get_mut(&key) else {
            continue;
        };
        let mut i = 0;
        while i < list.len() {
            let Some(watch) = list[i].upgrade() else {
                list.swap_remove(i);
                continue;
            };
            let wmask = watch.mask.load(Ordering::Acquire);
            if watch.unlinked.load(Ordering::Acquire)
                || wmask & mask == 0
                || watch.ignored_mask.load(Ordering::Acquire) & mask != 0
                || (is_child_event
                    && watch.named_requires_echild
                    && wmask & FAN_EVENT_ON_CHILD == 0)
            {
                i += 1;
                continue;
            }
            let event = NotifyEvent {
                wd: watch.wd,
                mask: mask | if is_dir { IN_ISDIR } else { 0 },
                cookie,
                name: name.clone(),
                obj_inode: obj_inode.map(Arc::downgrade),
                obj_mount: mount.map(Arc::downgrade),
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
            map.remove(&key);
        }
    }
    out
}

/// 收集匹配的权限监视（仅 `perm` 监视；fanotify 组使用）。
fn collect_perm_matching(
    inode: &Arc<Inode>,
    mount: Option<&Arc<crate::mount::Mount>>,
    mask: u32,
) -> Vec<(Arc<dyn WatchTarget>, NotifyEvent)> {
    let mut out = Vec::new();
    let sb_id = inode
        .superblock
        .upgrade()
        .map(|sb| sb.fs_id.raw())
        .unwrap_or(0);
    let mount_key = mount.map(|m| WatchKey::Mount(Arc::as_ptr(m) as usize));
    let keys = [
        Some(WatchKey::Inode(inode_key(inode))),
        mount_key,
        Some(WatchKey::Filesystem(sb_id)),
    ];
    let is_dir = inode.kind() == FileType::Directory;
    let map = WATCHES.lock();
    for key in keys.into_iter().flatten() {
        let Some(list) = map.get(&key) else {
            continue;
        };
        for w in list {
            let Some(watch) = w.upgrade() else {
                continue;
            };
            if !watch.perm || watch.unlinked.load(Ordering::Acquire) {
                continue;
            }
            let wmask = watch.mask.load(Ordering::Acquire);
            if wmask & mask == 0 || watch.ignored_mask.load(Ordering::Acquire) & mask != 0 {
                continue;
            }
            if let Some(target) = watch.target.upgrade() {
                out.push((
                    target,
                    NotifyEvent {
                        wd: watch.wd,
                        mask: mask | if is_dir { IN_ISDIR } else { 0 },
                        cookie: 0,
                        name: Vec::new(),
                        obj_inode: Some(Arc::downgrade(inode)),
                        obj_mount: mount.map(Arc::downgrade),
                    },
                ));
            }
        }
    }
    out
}

/// 权限事件注入结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermOutcome {
    /// 无权限监视（调用方直接放行）。
    NoWatchers,
    /// 全部组允许。
    Allow,
    /// 至少一个组拒绝（调用方返回 EACCES）。
    Deny,
    /// 等待被信号中断（调用方返回 EINTR；Linux ERESTARTSYS）。
    Interrupted,
}

impl PermOutcome {
    /// 把结果映射为 `VfsResult`：允许/无监视 → Ok；拒绝 → EACCES；中断 → EINTR。
    pub fn map_deny(self) -> crate::vfs::error::VfsResult<()> {
        match self {
            PermOutcome::NoWatchers | PermOutcome::Allow => Ok(()),
            PermOutcome::Deny => Err(crate::vfs::error::VfsError::PermissionDenied),
            PermOutcome::Interrupted => Err(crate::vfs::error::VfsError::Interrupted),
        }
    }
}

/// 权限事件注入：有匹配的权限监视时逐个等待用户态响应。
pub fn emit_perm_at(
    inode: &Arc<Inode>,
    mount: Option<&Arc<crate::mount::Mount>>,
    mask: u32,
) -> PermOutcome {
    if !perm_enabled() {
        return PermOutcome::NoWatchers;
    }
    let collected = collect_perm_matching(inode, mount, mask);
    if collected.is_empty() {
        return PermOutcome::NoWatchers;
    }
    let mut allow = true;
    for (target, _event) in collected {
        match target.await_permission(inode, mount, mask) {
            Ok(true) => {}
            Ok(false) => allow = false,
            Err(_) => return PermOutcome::Interrupted,
        }
    }
    if allow {
        PermOutcome::Allow
    } else {
        PermOutcome::Deny
    }
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
/// DELETE_SELF...）。`mount` 供 mount 作用域的 fanotify 标记匹配。
pub fn emit_at(
    inode: &Arc<Inode>,
    mount: Option<&Arc<crate::mount::Mount>>,
    mask: u32,
    cookie: u32,
) {
    if !is_enabled() {
        return;
    }
    dispatch(collect_matching(
        inode,
        mount,
        mask,
        cookie,
        Vec::new(),
        false,
        Some(inode),
    ));
}

/// 事件注入：对象自身事件 + 父目录监视匹配（Linux fsnotify 语义：
/// 子对象的 OPEN/MODIFY/CLOSE 等事件也投递给父目录上的目录监视，
/// fanotify 需要监视掩码带 FAN_EVENT_ON_CHILD，inotify 目录监视默认收）。
/// `dentry` 提供父链（沿父目录逐层收集，覆盖挂载点嵌套）。
pub fn emit_at_with_parents(
    inode: &Arc<Inode>,
    dentry: Option<&Arc<crate::vfs::dentry::Dentry>>,
    mount: Option<&Arc<crate::mount::Mount>>,
    mask: u32,
    cookie: u32,
) {
    if !is_enabled() {
        return;
    }
    let mut collected =
        collect_matching(inode, mount, mask, cookie, Vec::new(), false, Some(inode));
    if let Some(d) = dentry {
        // 父链从 dentry 的父开始逐层向上（含挂载点嵌套）。
        let mut cur = d.meta.lock().parent_cloned();
        while let Some(parent) = cur {
            if let Some(pinode) = parent.inode() {
                collected.append(&mut collect_matching(
                    &pinode,
                    mount,
                    mask,
                    cookie,
                    Vec::new(),
                    true,
                    Some(inode),
                ));
            }
            cur = parent.meta.lock().parent_cloned();
        }
    }
    dispatch(collected);
}

/// 事件注入（inode 作用域，无 mount 上下文）。
pub fn emit(inode: &Arc<Inode>, mask: u32, cookie: u32) {
    emit_at(inode, None, mask, cookie);
}

/// 事件注入：目录 + 名字（CREATE/DELETE/MOVED_FROM/MOVED_TO）。
///
/// 父目录监视按 `mask` 匹配并携带 `name`；若子对象自身被监视，DELETE 附带
/// `IN_DELETE_SELF`（`IN_EXCL_UNLINK` 时跳过并标记失效，监视保留 wd）、
/// MOVE 附带 `IN_MOVE_SELF`。DELETE_SELF 投递后监视被移除并补发
/// `IN_IGNORED`（Linux 语义）。
pub fn emit_named_at(
    parent: &Arc<Inode>,
    parent_mount: Option<&Arc<crate::mount::Mount>>,
    child: &Arc<Inode>,
    mask: u32,
    name: &[u8],
    cookie: u32,
) {
    if !is_enabled() {
        return;
    }
    let mut collected = collect_matching(
        parent,
        parent_mount,
        mask,
        cookie,
        name.to_vec(),
        true,
        Some(child),
    );
    match mask {
        IN_DELETE => {
            // EXCL_UNLINK：监视转为失效（保留 wd，不投递 DELETE_SELF）。
            {
                let map = WATCHES.lock();
                if let Some(list) = map.get(&WatchKey::Inode(inode_key(child))) {
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
            let mut map = WATCHES.lock();
            if let Some(list) = map.get_mut(&WatchKey::Inode(inode_key(child))) {
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
                        obj_inode: Some(watch.inode.clone()),
                        obj_mount: parent_mount.map(Arc::downgrade),
                    };
                    let target = watch.target.upgrade();
                    let wd = watch.wd;
                    list.swap_remove(i);
                    if let Some(target) = target {
                        collected.push((target, event, Some(wd)));
                    }
                }
            }
            if map
                .get(&WatchKey::Inode(inode_key(child)))
                .map(|l| l.is_empty())
                .unwrap_or(false)
            {
                map.remove(&WatchKey::Inode(inode_key(child)));
            }
        }
        IN_MOVED_FROM | IN_MOVED_TO => {
            let mut child_events = collect_matching(
                child,
                parent_mount,
                IN_MOVE_SELF,
                cookie,
                Vec::new(),
                false,
                Some(child),
            );
            collected.append(&mut child_events);
        }
        _ => {}
    }
    dispatch(collected);
}

/// 事件注入（inode 作用域，无 mount 上下文）。
pub fn emit_named(parent: &Arc<Inode>, child: &Arc<Inode>, mask: u32, name: &[u8], cookie: u32) {
    emit_named_at(parent, None, child, mask, name, cookie);
}

/// 同 [`emit_named`] 但不投递子对象自身事件（rename 的 TO 侧复用，
/// 避免 MOVE_SELF 在同一 rename 中重复投递——Linux 只发一次）。
pub fn emit_named_no_self(
    parent: &Arc<Inode>,
    child: &Arc<Inode>,
    mask: u32,
    name: &[u8],
    cookie: u32,
) {
    if !is_enabled() {
        return;
    }
    dispatch(collect_matching(
        parent,
        None,
        mask,
        cookie,
        name.to_vec(),
        true,
        Some(child),
    ));
}

/// 同 [`emit_named_no_self`] 但携带父目录 mount（fanotify mount 标记）。
pub fn emit_named_no_self_at(
    parent: &Arc<Inode>,
    parent_mount: Option<&Arc<crate::mount::Mount>>,
    child: &Arc<Inode>,
    mask: u32,
    name: &[u8],
    cookie: u32,
) {
    if !is_enabled() {
        return;
    }
    dispatch(collect_matching(
        parent,
        parent_mount,
        mask,
        cookie,
        name.to_vec(),
        true,
        Some(child),
    ));
}

/// 硬链接创建后复位 `IN_EXCL_UNLINK` 失效标记（Linux 语义：重新 link 恢复监视）。
pub fn rearm(inode: &Inode) {
    if !is_enabled() {
        return;
    }
    let map = WATCHES.lock();
    if let Some(list) = map.get(&WatchKey::Inode(inode_key(inode))) {
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
        let mut map = WATCHES.lock();
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
                        obj_inode: Some(watch.inode.clone()),
                        obj_mount: None,
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
