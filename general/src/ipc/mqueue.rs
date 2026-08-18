//! POSIX 消息队列的通用对象管理器（`mqueue`）。
//!
//! 语义对齐 Linux `ipc/mqueue.c`：
//!
//! - 队列按优先级出队（同一优先级 FIFO）；优先级范围 `0..MQ_PRIO_MAX-1`；
//! - `mq_timedsend` 满队列阻塞（`O_NONBLOCK` → `EAGAIN`）；
//!   `mq_timedreceive` 空队列阻塞（`O_NONBLOCK` → `EAGAIN`），接收缓冲区
//!   小于 `mq_msgsize` 返回 `EMSGSIZE`；
//! - `mq_notify` 注册一次性通知：队列从空变为非空时触发一次，随后注册失效；
//!   同一队列只允许一个注册者（重复注册 `EBUSY`）；`mq_receive` 取走消息
//!   不会触发通知；
//! - `mq_unlink` 后已打开的 fd 仍可用（引用计数语义）；
//! - 队列名必须以 `/` 开头且不再含其它 `/`（Linux 校验）。

use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::{String, ToString};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;

use errno::Errno;
use sched::WaitQueue;
use spin::Mutex;
use vfs::cred::{Capability, Credentials, Gid, Uid};
use vfs::stat::FileMode;

/// 最大消息优先级 + 1（Linux `MQ_PRIO_MAX`）。
pub const MQ_PRIO_MAX: i32 = 32768;
/// `mq_open` 默认 `mq_maxmsg`（Linux `/proc/sys/fs/mqueue/msg_max` 默认）。
pub const MQ_DEFAULT_MAXMSG: i64 = 10;
/// `mq_open` 默认 `mq_msgsize`（Linux `/proc/sys/fs/mqueue/msgsize_max` 默认）。
pub const MQ_DEFAULT_MSGSIZE: i64 = 8192;
/// 特权进程（`CAP_SYS_RESOURCE`）可设的 `mq_maxmsg` 系统上限。
pub const MQ_MAXMSG_MAX: i64 = 65536;
/// 特权进程（`CAP_SYS_RESOURCE`）可设的 `mq_msgsize` 系统上限。
pub const MQ_MSGSIZE_MAX: i64 = 65536;
/// 系统队列总数上限（Linux `/proc/sys/fs/mqueue/queues_max` 默认）。
pub const MQ_QUEUES_MAX: usize = 256;
/// 队列名最大长度（不含 NUL）。
pub const MQ_NAME_MAX: usize = 255;

/// `mq_notify` 的 `sigev_notify` 值（Linux `signal.h`）。
pub const SIGEV_NONE: i32 = 1;
pub const SIGEV_SIGNAL: i32 = 0;
pub const SIGEV_THREAD: i32 = 2;

/// `mq_attr` 字段偏移（Linux `mqueue.h`，64 位布局：4 个 `long`）。
pub const MQ_ATTR_FLAGS: usize = 0;
pub const MQ_ATTR_MAXMSG: usize = 8;
pub const MQ_ATTR_MSGSIZE: usize = 16;
pub const MQ_ATTR_CURMSGS: usize = 24;
/// `struct mq_attr` 的字节大小（64 位）。
pub const MQ_ATTR_SIZE: usize = 32;

/// 通知触发时 `siginfo.si_code`（Linux `SI_MESGQ`）。
pub const SI_MESGQ: i32 = -3;

/// 队列属性快照（Linux `struct mq_attr` 的字段语义）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MqAttr {
    pub maxmsg: i64,
    pub msgsize: i64,
    pub curmsgs: i64,
}

impl MqAttr {
    /// 默认属性（Linux `mq_open` 未提供 attr 时的默认值）。
    pub const fn default_new() -> Self {
        Self {
            maxmsg: MQ_DEFAULT_MAXMSG,
            msgsize: MQ_DEFAULT_MSGSIZE,
            curmsgs: 0,
        }
    }
}

/// 一条消息：优先级 + 负载。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MqMessage {
    pub priority: u32,
    pub data: Vec<u8>,
}

/// `mq_notify` 注册的通知类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MqNotifyKind {
    None,
    /// `SIGEV_SIGNAL`：向注册者投递 `signo`，`siginfo.si_value = value`。
    Signal {
        signo: i32,
        value: usize,
    },
    /// `SIGEV_THREAD`：在注册者进程上下文创建线程执行 `function(value)`。
    Thread {
        function: usize,
        value: usize,
    },
}

/// 一次已注册的队列通知。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MqNotification {
    pub kind: MqNotifyKind,
    /// 注册者 pid（通知投递目标：`SIGEV_SIGNAL` 投递信号、`SIGEV_THREAD` 克隆线程）。
    pub notify_pid: i32,
    /// 触发时 `siginfo.si_pid`（Linux 语义为发送消息的进程）。
    pub sender_pid: i32,
    /// 触发时 `siginfo.si_uid`。
    pub sender_uid: u32,
}

/// 队列状态变化观察者（供 fd 层的 poll/epoll 通知）。
pub trait MqStateObserver: Send + Sync {
    fn mq_state_changed(&self);
}

struct MqInner {
    perm_uid: Uid,
    perm_gid: Gid,
    perm_mode: FileMode,
    maxmsg: i64,
    msgsize: i64,
    /// 优先级 → FIFO 消息队列。
    messages: BTreeMap<u32, VecDeque<MqMessage>>,
    curmsgs: usize,
    /// 队列从空变非空时触发的通知（一次性）。
    notify: Option<MqNotification>,
    removed: bool,
    /// 状态观察者（Weak，避免 fd 与队列互相保活）。
    observers: Vec<Weak<dyn MqStateObserver>>,
}

/// 单个 POSIX 消息队列。阻塞者持有该对象的 `Arc`；`mq_unlink` 只从注册表
/// 摘除，已打开的 fd 通过 `Arc` 继续使用队列（Linux 引用计数语义）。
pub struct MqObject {
    inner: Mutex<MqInner>,
    /// 等待空间（发送者）。
    senders: WaitQueue,
    /// 等待消息（接收者）。
    receivers: WaitQueue,
}

impl MqObject {
    /// `mode` 是创建者经 umask 掩码后的队列权限位（Linux `mq_open` 语义）。
    fn new(attr: MqAttr, mode: FileMode, cred: &Credentials) -> Self {
        Self {
            inner: Mutex::new(MqInner {
                perm_uid: cred.euid,
                perm_gid: cred.egid,
                perm_mode: mode.mask(FileMode::PERM_MASK),
                maxmsg: attr.maxmsg,
                msgsize: attr.msgsize,
                messages: BTreeMap::new(),
                curmsgs: 0,
                notify: None,
                removed: false,
                observers: Vec::new(),
            }),
            senders: WaitQueue::new(),
            receivers: WaitQueue::new(),
        }
    }

    /// 注册状态观察者（fd 层在打开时调用）。
    pub fn subscribe(&self, observer: Weak<dyn MqStateObserver>) {
        self.inner.lock().observers.push(observer);
    }

    /// 在队列可观测状态变化后通知观察者（在 inner 锁外调用）。
    fn notify_state_changed(&self) {
        let observers: Vec<Arc<dyn MqStateObserver>> = {
            let mut inner = self.inner.lock();
            inner
                .observers
                .retain(|observer| observer.strong_count() != 0);
            inner
                .observers
                .iter()
                .filter_map(|observer| observer.upgrade())
                .collect()
        };
        for observer in observers {
            observer.mq_state_changed();
        }
    }

    pub fn senders(&self) -> &WaitQueue {
        &self.senders
    }

    pub fn receivers(&self) -> &WaitQueue {
        &self.receivers
    }

    pub fn attr(&self) -> MqAttr {
        let inner = self.inner.lock();
        MqAttr {
            maxmsg: inner.maxmsg,
            msgsize: inner.msgsize,
            curmsgs: inner.curmsgs as i64,
        }
    }

    pub fn removed(&self) -> bool {
        self.inner.lock().removed
    }

    /// 校验 `mq_open` 的 attr（Linux `mq_attr_valid` 语义）。
    ///
    /// 持有 `CAP_SYS_RESOURCE` 的进程可设置超过默认值（`MQ_DEFAULT_MAXMSG`/
    /// `MQ_DEFAULT_MSGSIZE`）的上限，但不得超过系统级上限。
    pub fn validate_attr(attr: &MqAttr, cred: &Credentials) -> Result<(), Errno> {
        if attr.maxmsg < 1 || attr.msgsize < 1 {
            return Err(Errno::EINVAL);
        }
        let maxmsg_max = if cred.has_cap(Capability::SysResource) {
            MQ_MAXMSG_MAX
        } else {
            MQ_DEFAULT_MAXMSG
        };
        let msgsize_max = if cred.has_cap(Capability::SysResource) {
            MQ_MSGSIZE_MAX
        } else {
            MQ_DEFAULT_MSGSIZE
        };
        if attr.maxmsg > maxmsg_max || attr.msgsize > msgsize_max {
            return Err(Errno::EINVAL);
        }
        Ok(())
    }

    /// 校验 `mq_timedsend` 的优先级。
    pub fn validate_priority(priority: u32) -> Result<(), Errno> {
        // 按无符号比较（Linux `msg_prio >= MQ_PRIO_MAX` 语义），避免高位为 1
        // 的 u32 经 `as i32` 变成负数而绕过校验。
        if priority >= MQ_PRIO_MAX as u32 {
            return Err(Errno::EINVAL);
        }
        Ok(())
    }

    /// 校验一次 `mq_open` 的访问请求（Linux `ipcperms` 语义）。由 syscall 层在
    /// 打开时调用；fd 层的收发不再重复检查（打开即授权）。
    pub fn check_access(&self, write: bool, cred: &Credentials) -> Result<(), Errno> {
        let inner = self.inner.lock();
        if inner.removed {
            return Err(Errno::EINVAL);
        }
        let allowed = if write {
            cred.can_write(inner.perm_uid, inner.perm_gid, inner.perm_mode)
        } else {
            cred.can_read(inner.perm_uid, inner.perm_gid, inner.perm_mode)
        };
        if allowed { Ok(()) } else { Err(Errno::EACCES) }
    }

    /// 原子尝试发送一条消息。
    ///
    /// 成功时若队列从空变为非空且注册了通知，返回需要触发的通知（一次性）。
    /// 队列满时：`nonblock` → `EAGAIN`；否则返回 `(false, None)` 供调用方阻塞。
    pub fn try_send(
        &self,
        priority: u32,
        data: &[u8],
        sender_pid: i32,
        sender_uid: u32,
        nonblock: bool,
    ) -> Result<(bool, Option<MqNotification>), Errno> {
        Self::validate_priority(priority)?;
        if data.len() as i64 > self.inner.lock().msgsize {
            return Err(Errno::EMSGSIZE);
        }

        let mut inner = self.inner.lock();
        if inner.curmsgs as i64 >= inner.maxmsg {
            if nonblock {
                return Err(Errno::EAGAIN);
            }
            return Ok((false, None));
        }

        let was_empty = inner.curmsgs == 0;
        inner
            .messages
            .entry(priority)
            .or_default()
            .push_back(MqMessage {
                priority,
                data: data.to_vec(),
            });
        inner.curmsgs += 1;
        // Linux：通知的 si_pid/si_uid 是触发该通知的消息发送者身份。
        let notify = if was_empty {
            inner.notify.take().map(|mut notification| {
                notification.sender_pid = sender_pid;
                notification.sender_uid = sender_uid;
                notification
            })
        } else {
            None
        };
        drop(inner);
        self.receivers.wake_all();
        self.notify_state_changed();
        Ok((true, notify))
    }

    /// 原子尝试接收最高优先级消息。
    ///
    /// 成功返回 `Some(消息)`；队列为空且 `nonblock` 时返回 `EAGAIN`，否则返回
    /// `None` 供调用方决定阻塞。`maxsize < msgsize` 时返回 `EMSGSIZE`
    /// （Linux 语义：即使消息实际更短也拒绝，见 `ipc/mqueue.c`）。
    pub fn try_receive(&self, maxsize: usize, nonblock: bool) -> Result<Option<MqMessage>, Errno> {
        if (maxsize as i64) < self.inner.lock().msgsize {
            return Err(Errno::EMSGSIZE);
        }

        let mut inner = self.inner.lock();
        let Some((&priority, queue)) = inner.messages.iter_mut().next_back() else {
            if nonblock {
                return Err(Errno::EAGAIN);
            }
            return Ok(None);
        };
        let message = queue.pop_front().expect("最高优先级队列非空必有消息");
        if queue.is_empty() {
            inner.messages.remove(&priority);
        }
        inner.curmsgs -= 1;
        drop(inner);
        self.senders.wake_all();
        self.notify_state_changed();
        Ok(Some(message))
    }

    /// `mq_notify`：注册/替换通知。已有**其它**注册者时返回 `EBUSY`。
    /// 读权限校验由 syscall 层在调用前完成。
    pub fn register_notify(&self, kind: MqNotifyKind, notify_pid: i32) -> Result<(), Errno> {
        if kind == MqNotifyKind::None {
            // SIGEV_NONE：清除注册（Linux 语义）。
            self.inner.lock().notify = None;
            return Ok(());
        }
        let mut inner = self.inner.lock();
        if inner.removed {
            return Err(Errno::EINVAL);
        }
        if let Some(existing) = inner.notify.as_ref() {
            if existing.notify_pid != notify_pid {
                return Err(Errno::EBUSY);
            }
        }
        inner.notify = Some(MqNotification {
            kind,
            notify_pid,
            sender_pid: 0,
            sender_uid: 0,
        });
        Ok(())
    }

    /// 队列是否可读（有消息）——供 poll/select 使用。
    pub fn has_messages(&self) -> bool {
        self.inner.lock().curmsgs != 0
    }

    /// 队列是否可写（有空间）——供 poll/select 使用。
    pub fn has_space(&self) -> bool {
        let inner = self.inner.lock();
        (inner.curmsgs as i64) < inner.maxmsg && !inner.removed
    }
}

/// 队列注册表（`/dev/mqueue` 的目录视图 + `mq_open` 查找）。
pub struct MqRegistry {
    inner: Mutex<BTreeMap<alloc::string::String, Arc<MqObject>>>,
}

impl MqRegistry {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(BTreeMap::new()),
        }
    }

    /// 校验队列名（Linux `do_mq_open` 的 `dir_name` 校验）。
    pub fn validate_name(name: &str) -> Result<(), Errno> {
        if !name.starts_with('/') {
            return Err(Errno::EINVAL);
        }
        if name[1..].contains('/') {
            return Err(Errno::EINVAL);
        }
        if name.len() > MQ_NAME_MAX {
            return Err(Errno::ENAMETOOLONG);
        }
        Ok(())
    }

    /// `mq_open`：按 `O_CREAT/O_EXCL` 查找或创建队列。
    ///
    /// `mode` 仅在创建时生效（已由调用方按 umask 掩码）；查找已有队列时忽略。
    pub fn open(
        &self,
        name: &str,
        create: bool,
        excl: bool,
        attr: Option<&MqAttr>,
        mode: FileMode,
        cred: &Credentials,
    ) -> Result<Arc<MqObject>, Errno> {
        Self::validate_name(name)?;
        let mut inner = self.inner.lock();
        if let Some(queue) = inner.get(name) {
            if create && excl {
                return Err(Errno::EEXIST);
            }
            return Ok(Arc::clone(queue));
        }
        if !create {
            return Err(Errno::ENOENT);
        }
        if inner.len() >= MQ_QUEUES_MAX {
            return Err(Errno::ENOSPC);
        }
        let attr = attr.copied().unwrap_or(MqAttr::default_new());
        MqObject::validate_attr(&attr, cred)?;
        let queue = Arc::new(MqObject::new(attr, mode, cred));
        inner.insert(name.to_string(), Arc::clone(&queue));
        Ok(queue)
    }

    /// `mq_unlink`：从注册表摘除（已打开的 fd 继续可用）。
    pub fn unlink(&self, name: &str) -> Result<(), Errno> {
        Self::validate_name(name)?;
        let mut inner = self.inner.lock();
        let queue = inner.get(name).cloned().ok_or(Errno::ENOENT)?;
        {
            let mut queue_inner = queue.inner.lock();
            if queue_inner.removed {
                return Err(Errno::ENOENT);
            }
            queue_inner.removed = true;
        }
        inner.remove(name);
        queue.senders.wake_all();
        queue.receivers.wake_all();
        queue.notify_state_changed();
        Ok(())
    }

    /// 目录视图：当前全部队列名。
    pub fn names(&self) -> Vec<alloc::string::String> {
        self.inner.lock().keys().cloned().collect()
    }
}

impl Default for MqRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// 宿主测试（`cargo test -p general --target x86_64-unknown-linux-gnu`）。
#[cfg(test)]
mod tests {
    use super::*;
    use vfs::cred::{Credentials, Gid, Uid};
    use vfs::stat::FileMode;

    fn new_queue(maxmsg: i64, msgsize: i64) -> MqObject {
        MqObject::new(
            MqAttr {
                maxmsg,
                msgsize,
                curmsgs: 0,
            },
            FileMode::new(0o600),
            &Credentials::root(),
        )
    }

    /// F5：`mq_unlink` 只摘除名字，已打开的句柄继续可收发。
    #[test]
    fn unlink_keeps_open_handles_usable() {
        let registry = MqRegistry::new();
        let cred = Credentials::root();
        let queue = registry
            .open("/q", true, false, None, FileMode::new(0o600), &cred)
            .expect("创建队列");
        registry.unlink("/q").expect("摘除名字");

        let (sent, _) = queue.try_send(0, b"hello", 1, 1, true).expect("发送");
        assert!(sent);
        let received = queue
            .try_receive(8192, true)
            .expect("接收")
            .expect("有消息");
        assert_eq!(received.data, b"hello");
    }

    /// F6：通知携带的是"触发消息的发送者"身份，投递目标仍是注册者。
    #[test]
    fn notify_reports_sender_identity() {
        let queue = new_queue(1, 16);
        queue
            .register_notify(
                MqNotifyKind::Signal {
                    signo: 1,
                    value: 42,
                },
                100,
            )
            .expect("注册通知");

        let (sent, notify) = queue.try_send(0, b"x", 200, 300, true).expect("发送");
        assert!(sent);
        let notification = notify.expect("队列从空变非空应触发通知");
        assert_eq!(notification.notify_pid, 100);
        assert_eq!(notification.sender_pid, 200);
        assert_eq!(notification.sender_uid, 300);
    }

    /// F7：优先级必须按无符号语义校验，高位为 1 的 u32 不得绕过。
    #[test]
    fn priority_at_or_above_max_is_rejected() {
        assert_eq!(MqObject::validate_priority(0), Ok(()));
        assert_eq!(MqObject::validate_priority(MQ_PRIO_MAX as u32 - 1), Ok(()));
        assert_eq!(
            MqObject::validate_priority(MQ_PRIO_MAX as u32),
            Err(Errno::EINVAL)
        );
        assert_eq!(MqObject::validate_priority(u32::MAX), Err(Errno::EINVAL));
    }

    /// F8：队列总数达到上限时返回 `ENOSPC` 而非 `EMFILE`。
    #[test]
    fn queue_count_limit_returns_enospc() {
        let registry = MqRegistry::new();
        let cred = Credentials::root();
        for i in 0..MQ_QUEUES_MAX {
            registry
                .open(
                    &alloc::format!("/q{i}"),
                    true,
                    false,
                    None,
                    FileMode::new(0o600),
                    &cred,
                )
                .unwrap_or_else(|e| panic!("第 {i} 个队列创建失败: {e:?}"));
        }
        let overflow = registry.open("/overflow", true, false, None, FileMode::new(0o600), &cred);
        assert!(matches!(overflow, Err(Errno::ENOSPC)));
    }

    /// F9：`CAP_SYS_RESOURCE` 可超过默认上限，但不得超过系统上限。
    #[test]
    fn attr_sys_resource_raises_limits() {
        let root = Credentials::root();
        let nobody = Credentials::unprivileged(Uid(1000), Gid(1000));
        let big = MqAttr {
            maxmsg: MQ_DEFAULT_MAXMSG + 1,
            msgsize: MQ_DEFAULT_MSGSIZE,
            curmsgs: 0,
        };
        assert_eq!(MqObject::validate_attr(&big, &nobody), Err(Errno::EINVAL));
        assert_eq!(MqObject::validate_attr(&big, &root), Ok(()));

        let huge = MqAttr {
            maxmsg: MQ_MAXMSG_MAX + 1,
            msgsize: MQ_DEFAULT_MSGSIZE,
            curmsgs: 0,
        };
        assert_eq!(MqObject::validate_attr(&huge, &root), Err(Errno::EINVAL));

        let zero = MqAttr {
            maxmsg: 0,
            msgsize: MQ_DEFAULT_MSGSIZE,
            curmsgs: 0,
        };
        assert_eq!(MqObject::validate_attr(&zero, &root), Err(Errno::EINVAL));
    }
}
