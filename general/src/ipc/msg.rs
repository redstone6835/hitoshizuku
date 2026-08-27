//! System V 消息队列的通用对象管理器。
//!
//! 本模块维护消息队列的身份、权限、消息内容与队列统计。syscall 层负责用户
//! ABI 编解码以及阻塞调度；这里提供可原子重试的收发接口，语义对齐 Linux
//! `ipc/msg.c`：
//!
//! - `msgsnd` 按 `qbytes` 上限阻塞（`IPC_NOWAIT` → `EAGAIN`），消息按插入
//!   顺序排队（Linux `q_messages` 单链表），`msgtyp` 选择/`MSG_COPY` 序号
//!   都按这个顺序扫描；
//! - `msgrcv` 支持 `MSG_NOERROR`（截断）、`MSG_EXCEPT`（类型不等）、
//!   `MSG_COPY`（不取走 + `MSG_TRUNC` 返回全尺寸，需 `CAP_CHECKPOINT_RESTORE`
//!   或 `CAP_SYS_ADMIN`）；
//! - `IPC_RMID` 唤醒全部阻塞者并让它们观察到 `EIDRM`（稳定对象引用语义与
//!   semaphore 一致）。

use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::Arc;
#[cfg(feature = "kernel-tests")]
use alloc::vec;
use alloc::vec::Vec;

use errno::Errno;
use sched::WaitQueue;
use spin::Mutex;
use vfs::cred::{Capability, Credentials, Gid, Uid};
use vfs::stat::FileMode;

use super::shm::{IPC_CREAT, IPC_EXCL, IPC_PRIVATE};

/// `msgrcv` 非阻塞标志；`msgsnd` 满队列时同样适用。
pub const IPC_NOWAIT: u32 = 0o4000;
/// `msgrcv`：消息比缓冲区长时截断而不是返回 `E2BIG`。
pub const MSG_NOERROR: u32 = 0o10000;
/// `msgrcv`：读取第一条类型不等于 `msgtyp` 的消息。
pub const MSG_EXCEPT: u32 = 0o20000;
/// `msgrcv`：按队列序号拷贝消息而不取走。
pub const MSG_COPY: u32 = 0o40000;
/// `msgrcv`（仅与 `MSG_COPY` 同用）：返回消息完整长度而非拷贝长度。
pub const MSG_TRUNC: u32 = 0o100000;

/// `msgctl` 的枚举类命令（索引/信息查询族）。
pub const MSG_STAT: u32 = 11;
pub const MSG_INFO: u32 = 12;
pub const MSG_STAT_ANY: u32 = 13;

/// 单个消息的最大字节数（Linux `MSGMAX` 默认）。
pub const MSGMAX: usize = 8192;
/// 单队列字节上限（Linux `MSGMNB` 默认）。
pub const MSGMNB: usize = 16384;
/// 系统消息队列总数上限（Linux `MSGMNI` 默认）。
pub const MSGMNI: usize = 32_000;

const FIRST_MSG_ID: i32 = 1;

/// SysV 消息队列 id。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MsgId(pub i32);

/// SysV 消息队列 key。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MsgKey(pub i32);

impl MsgKey {
    /// `IPC_PRIVATE` 每次都创建新队列，不进入 key 查找表。
    pub const PRIVATE: Self = Self(IPC_PRIVATE);
}

/// 一条排队中的消息。`mtype` 必须为正；`data` 为 `mtext` 原文。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MsgMessage {
    pub mtype: i64,
    pub data: Vec<u8>,
}

/// `msgrcv` 的一次成功结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceivedMsg {
    pub mtype: i64,
    /// 已按 `msgsz`/`MSG_NOERROR` 规则截断后的内容。
    pub data: Vec<u8>,
    /// 队列中消息的完整长度（`MSG_COPY` + `MSG_TRUNC` 时用于返回值）。
    pub full_size: usize,
    /// 本次是 `MSG_COPY`（消息未从队列移除）。
    pub copied: bool,
}

/// `msgrcv` 一次尝试的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MsgRecvOutcome {
    Received(ReceivedMsg),
    /// 无匹配消息，调用方应决定阻塞或 `EAGAIN`。
    WouldBlock,
}

/// 一次原子收发尝试的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsgOpAttempt {
    /// 操作已提交（发送入队 / 接收取出）。
    Done,
    /// 条件不满足（队列满 / 无匹配消息），调用方应决定阻塞或 `EAGAIN`。
    WouldBlock,
}

/// `msgrcv` 的消息选择模式，对应 Linux `testmsg()` 的四种搜索方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchMode {
    /// `msgtyp == 0`：队首任意消息。
    Any,
    /// `msgtyp > 0`：第一条类型等于 `msgtyp` 的消息。
    Number(i64),
    /// `msgtyp < 0`：类型小于等于 `|msgtyp|` 的消息中 `mtype` 最小的一条
    /// （Linux `SEARCH_LESSEQUAL` 语义；同类型取入队最早的一条）。
    LessEqual(i64),
    /// `MSG_EXCEPT`：第一条类型不等于 `msgtyp` 的消息。
    NotEqual(i64),
}

impl SearchMode {
    fn matches(self, mtype: i64) -> bool {
        match self {
            Self::Any => true,
            Self::Number(want) => mtype == want,
            Self::LessEqual(bound) => mtype <= bound,
            Self::NotEqual(want) => mtype != want,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MsgPerm {
    key: MsgKey,
    uid: Uid,
    gid: Gid,
    cuid: Uid,
    cgid: Gid,
    mode: FileMode,
}

impl MsgPerm {
    fn new(key: MsgKey, flags: u32, cred: &Credentials) -> Self {
        Self {
            key,
            uid: cred.euid,
            gid: cred.egid,
            cuid: cred.euid,
            cgid: cred.egid,
            mode: mode_from_flags(flags),
        }
    }
}

/// `msgrcv`/`msgsnd` 需要区分读写权限：接收要读权限、发送要写权限。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Access {
    Receive,
    Send,
}

struct MsgQueueInner {
    perm: MsgPerm,
    /// 按插入顺序排列的消息（Linux `q_messages`）。
    messages: VecDeque<MsgMessage>,
    /// 当前队列中的字节总数。
    bytes: usize,
    /// `msg_qbytes`：队列字节上限。
    qbytes: usize,
    /// 最近一次 `msgsnd` 时间（秒）。
    stime: i64,
    /// 最近一次 `msgrcv` 时间（秒）。
    rtime: i64,
    /// 最近一次 `msgctl` 变更时间（秒）。
    ctime: i64,
    /// 最近一次发送者 pid。
    lspid: i32,
    /// 最近一次接收者 pid。
    lrpid: i32,
    removed: bool,
}

/// 单个消息队列。阻塞者持有该对象的 `Arc`，因此队列删除后仍能观察到
/// `EIDRM`，而不会误认成后来复用同一整数 id 的新队列。
pub struct MsgQueue {
    inner: Mutex<MsgQueueInner>,
    waiters: WaitQueue,
}

impl MsgQueue {
    fn new(key: MsgKey, flags: u32, cred: &Credentials) -> Self {
        Self {
            inner: Mutex::new(MsgQueueInner {
                perm: MsgPerm::new(key, flags, cred),
                messages: VecDeque::new(),
                bytes: 0,
                qbytes: MSGMNB,
                stime: 0,
                rtime: 0,
                ctime: 0,
                lspid: 0,
                lrpid: 0,
                removed: false,
            }),
            waiters: WaitQueue::new(),
        }
    }

    /// 返回该队列的等待队列，供 syscall 层执行 prepare/recheck/sleep 协议。
    pub fn waiters(&self) -> &WaitQueue {
        &self.waiters
    }

    /// 原子尝试入队一条消息。
    ///
    /// 队列满（`bytes + len > qbytes`）时返回 [`MsgOpAttempt::WouldBlock`]；
    /// 调用方随后可选择阻塞等待。成功时更新 `stime`/`lspid` 等统计。
    pub fn try_send(
        &self,
        mtype: i64,
        data: &[u8],
        flags: u32,
        cred: &Credentials,
        pid: i32,
        now_sec: i64,
    ) -> Result<MsgOpAttempt, Errno> {
        if mtype < 1 {
            return Err(Errno::EINVAL);
        }
        if data.len() > MSGMAX {
            return Err(Errno::EINVAL);
        }
        if flags & !(IPC_NOWAIT) != 0 {
            return Err(Errno::EINVAL);
        }
        let mut inner = self.inner.lock();
        if inner.removed {
            return Err(Errno::EIDRM);
        }
        check_operation_permissions(cred, &inner.perm, Access::Send)?;

        let added = data.len();
        if inner
            .bytes
            .checked_add(added)
            .map_or(true, |b| b > inner.qbytes)
        {
            if flags & IPC_NOWAIT != 0 {
                return Err(Errno::EAGAIN);
            }
            return Ok(MsgOpAttempt::WouldBlock);
        }

        inner.messages.push_back(MsgMessage {
            mtype,
            data: data.to_vec(),
        });
        inner.bytes += added;
        inner.stime = now_sec;
        inner.lspid = pid;
        Ok(MsgOpAttempt::Done)
    }

    /// 按 [`SearchMode`] 查找并（除 `MSG_COPY` 外）取走一条消息。
    pub fn try_receive(
        &self,
        msgtyp: i64,
        msgsz: usize,
        flags: u32,
        cred: &Credentials,
        pid: i32,
        now_sec: i64,
    ) -> Result<MsgRecvOutcome, Errno> {
        if flags & !(MSG_NOERROR | MSG_EXCEPT | MSG_COPY | MSG_TRUNC | IPC_NOWAIT) != 0 {
            return Err(Errno::EINVAL);
        }
        if flags & MSG_TRUNC != 0 && flags & MSG_COPY == 0 {
            return Err(Errno::EINVAL);
        }
        if flags & MSG_EXCEPT != 0 && msgtyp <= 0 {
            return Err(Errno::EINVAL);
        }
        if flags & MSG_COPY != 0 {
            if flags & MSG_EXCEPT != 0 || flags & IPC_NOWAIT == 0 {
                return Err(Errno::EINVAL);
            }
            if !cred.has_cap(Capability::CheckpointRestore) && !cred.has_cap(Capability::SysAdmin) {
                return Err(Errno::EPERM);
            }
        }

        let mode = if flags & MSG_EXCEPT != 0 {
            SearchMode::NotEqual(msgtyp)
        } else if msgtyp == 0 {
            SearchMode::Any
        } else if msgtyp < 0 {
            SearchMode::LessEqual(-msgtyp)
        } else {
            SearchMode::Number(msgtyp)
        };

        let mut inner = self.inner.lock();
        if inner.removed {
            return Err(Errno::EIDRM);
        }
        // MSG_COPY 是 CRIU 风格的只读诊断路径，按 Linux 语义不要求读权限
        // （调用者已通过 CAP_CHECKPOINT_RESTORE/CAP_SYS_ADMIN 门槛）。
        if flags & MSG_COPY == 0 {
            check_operation_permissions(cred, &inner.perm, Access::Receive)?;
        }

        // 先定位索引再取走，避免内容相同的两条消息被错误匹配。
        let found_index = if flags & MSG_COPY != 0 {
            // msgtyp 作为队列序号（从 0 起），按插入顺序。
            let index = usize::try_from(msgtyp).map_err(|_| Errno::EINVAL)?;
            if index >= inner.messages.len() {
                None
            } else {
                Some(index)
            }
        } else if let SearchMode::LessEqual(bound) = mode {
            // Linux `SEARCH_LESSEQUAL`：不是取第一条 `mtype <= bound`，而是取
            // `mtype <= bound` 的消息里 `mtype` 最小的一条（`min_by_key` 在并列
            // 时返回入队最早者，与 Linux `find_msg` 的扫描行为一致）。
            inner
                .messages
                .iter()
                .enumerate()
                .filter(|(_, message)| message.mtype <= bound)
                .min_by_key(|(_, message)| message.mtype)
                .map(|(index, _)| index)
        } else {
            inner
                .messages
                .iter()
                .position(|message| mode.matches(message.mtype))
        };
        let Some(index) = found_index else {
            if flags & IPC_NOWAIT != 0 {
                return Err(Errno::EAGAIN);
            }
            return Ok(MsgRecvOutcome::WouldBlock);
        };

        let message = &inner.messages[index];
        let full_size = message.data.len();
        let copy = flags & MSG_COPY != 0;
        if !copy && flags & MSG_NOERROR == 0 && msgsz < full_size {
            return Err(Errno::E2BIG);
        }
        let copied = full_size.min(msgsz);
        let received = ReceivedMsg {
            mtype: message.mtype,
            data: message.data[..copied].to_vec(),
            full_size,
            copied: copy,
        };

        if !copy {
            inner.messages.remove(index);
            inner.bytes = inner.bytes.saturating_sub(full_size);
            inner.rtime = now_sec;
            inner.lrpid = pid;
        }
        Ok(MsgRecvOutcome::Received(received))
    }

    fn check_requested_mode(&self, flags: u32, cred: &Credentials) -> Result<(), Errno> {
        let inner = self.inner.lock();
        if inner.removed {
            return Err(Errno::EIDRM);
        }
        check_mode_request(cred, &inner.perm, flags)
    }

    /// `msgctl(IPC_STAT)` 快照（要求读权限）。
    pub fn stat(&self, cred: &Credentials) -> Result<MsgMetadata, Errno> {
        self.stat_inner(cred, true)
    }

    /// `msgctl(MSG_STAT_ANY)` 快照（不检查读权限）。
    pub fn stat_any(&self) -> Result<MsgMetadata, Errno> {
        self.stat_inner(&Credentials::root(), false)
    }

    fn stat_inner(&self, cred: &Credentials, check_perms: bool) -> Result<MsgMetadata, Errno> {
        let inner = self.inner.lock();
        if inner.removed {
            return Err(Errno::EIDRM);
        }
        if check_perms {
            check_operation_permissions(cred, &inner.perm, Access::Receive)?;
        }
        Ok(MsgMetadata {
            perm: inner.perm,
            stime: inner.stime,
            rtime: inner.rtime,
            ctime: inner.ctime,
            bytes: inner.bytes,
            qnum: inner.messages.len(),
            qbytes: inner.qbytes,
            lspid: inner.lspid,
            lrpid: inner.lrpid,
        })
    }

    /// `msgctl(IPC_SET)`：更新权限元数据与队列上限。
    ///
    /// 提升 `qbytes` 超过 `MSGMNB` 需要 `CAP_SYS_RESOURCE`。
    pub fn set(
        &self,
        uid: Option<Uid>,
        gid: Option<Gid>,
        mode: Option<FileMode>,
        qbytes: Option<usize>,
        cred: &Credentials,
        now_sec: i64,
    ) -> Result<(), Errno> {
        let mut inner = self.inner.lock();
        if inner.removed {
            return Err(Errno::EIDRM);
        }
        check_control_owner(cred, &inner.perm)?;
        if let Some(qbytes) = qbytes {
            if qbytes > MSGMNB && !cred.has_cap(Capability::SysResource) {
                return Err(Errno::EPERM);
            }
            inner.qbytes = qbytes;
        }
        if let Some(uid) = uid {
            inner.perm.uid = uid;
        }
        if let Some(gid) = gid {
            inner.perm.gid = gid;
        }
        if let Some(mode) = mode {
            inner.perm.mode = mode.mask(FileMode::PERM_MASK);
        }
        inner.ctime = now_sec;
        Ok(())
    }
}

/// `msgctl(IPC_STAT)` 快照。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MsgMetadata {
    perm: MsgPerm,
    pub stime: i64,
    pub rtime: i64,
    pub ctime: i64,
    pub bytes: usize,
    pub qnum: usize,
    pub qbytes: usize,
    pub lspid: i32,
    pub lrpid: i32,
}

// MsgPerm 需要被 ABI 层读取。
impl MsgMetadata {
    pub fn key(&self) -> MsgKey {
        self.perm.key
    }
    pub fn uid(&self) -> Uid {
        self.perm.uid
    }
    pub fn gid(&self) -> Gid {
        self.perm.gid
    }
    pub fn cuid(&self) -> Uid {
        self.perm.cuid
    }
    pub fn cgid(&self) -> Gid {
        self.perm.cgid
    }
    pub fn mode(&self) -> FileMode {
        self.perm.mode
    }
}

struct MsgManagerState {
    by_id: BTreeMap<MsgId, Arc<MsgQueue>>,
    by_key: BTreeMap<MsgKey, MsgId>,
    next_id: i32,
}

impl MsgManagerState {
    fn new() -> Self {
        Self {
            by_id: BTreeMap::new(),
            by_key: BTreeMap::new(),
            next_id: FIRST_MSG_ID,
        }
    }
}

/// SysV 消息队列全局管理器。
pub struct MsgManager {
    state: Mutex<MsgManagerState>,
}

impl MsgManager {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(MsgManagerState::new()),
        }
    }

    /// `msgget`：`IPC_PRIVATE` 总是创建；普通 key 按 `IPC_CREAT/IPC_EXCL`
    /// 查找或创建。存在 key 时按请求 mode 校验权限。
    pub fn msgget(&self, key: MsgKey, flags: u32, cred: &Credentials) -> Result<MsgId, Errno> {
        let mut state = self.state.lock();
        if key != MsgKey::PRIVATE {
            if let Some(id) = state.by_key.get(&key).copied() {
                let queue = state.by_id.get(&id).ok_or(Errno::EINVAL)?;
                if flags & IPC_CREAT != 0 && flags & IPC_EXCL != 0 {
                    return Err(Errno::EEXIST);
                }
                queue.check_requested_mode(flags, cred)?;
                return Ok(id);
            }
            if flags & IPC_CREAT == 0 {
                return Err(Errno::ENOENT);
            }
        }
        if state.by_id.len() >= MSGMNI {
            return Err(Errno::ENOSPC);
        }
        let id = allocate_id(&mut state)?;
        let queue = Arc::new(MsgQueue::new(key, flags, cred));
        state.by_id.insert(id, queue);
        if key != MsgKey::PRIVATE {
            state.by_key.insert(key, id);
        }
        Ok(id)
    }

    /// 获取一次 `msgsnd`/`msgrcv` 使用的稳定对象引用。
    pub fn queue_for_operation(&self, id: MsgId) -> Result<Arc<MsgQueue>, Errno> {
        self.state
            .lock()
            .by_id
            .get(&id)
            .cloned()
            .ok_or(Errno::EINVAL)
    }

    /// `MSG_STAT`/`MSG_STAT_ANY`：按 id 排序后的序号取队列，返回 (真实 id, 队列)。
    pub fn queue_by_index(&self, index: i32) -> Result<(MsgId, Arc<MsgQueue>), Errno> {
        if index < 0 {
            return Err(Errno::EINVAL);
        }
        let state = self.state.lock();
        let Some((&id, queue)) = state.by_id.iter().nth(index as usize) else {
            return Err(Errno::EINVAL);
        };
        Ok((id, Arc::clone(queue)))
    }

    /// 删除队列并标记稳定对象，使已阻塞任务返回 `EIDRM`。
    pub fn remove(&self, id: MsgId, cred: &Credentials) -> Result<Arc<MsgQueue>, Errno> {
        let mut state = self.state.lock();
        let queue = state.by_id.get(&id).cloned().ok_or(Errno::EINVAL)?;
        let key = {
            let mut inner = queue.inner.lock();
            check_control_owner(cred, &inner.perm)?;
            inner.removed = true;
            inner.perm.key
        };
        state.by_id.remove(&id);
        if key != MsgKey::PRIVATE {
            state.by_key.remove(&key);
        }
        Ok(queue)
    }

    /// `IPC_INFO`/`MSG_INFO` 系统级诊断快照。
    pub fn info(&self) -> MsgSystemInfo {
        let state = self.state.lock();
        let mut messages = 0usize;
        let mut bytes = 0usize;
        for queue in state.by_id.values() {
            let inner = queue.inner.lock();
            messages += inner.messages.len();
            bytes += inner.bytes;
        }
        MsgSystemInfo {
            queues: state.by_id.len(),
            messages,
            bytes,
            max_index: state.by_id.keys().next_back().map_or(0, |id| id.0),
        }
    }
}

impl Default for MsgManager {
    fn default() -> Self {
        Self::new()
    }
}

/// `IPC_INFO`/`MSG_INFO` 返回的统计。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MsgSystemInfo {
    pub queues: usize,
    pub messages: usize,
    pub bytes: usize,
    pub max_index: i32,
}

fn allocate_id(state: &mut MsgManagerState) -> Result<MsgId, Errno> {
    for _ in 0..MSGMNI {
        let raw = state.next_id;
        state.next_id = if raw == i32::MAX {
            FIRST_MSG_ID
        } else {
            raw + 1
        };
        let id = MsgId(raw);
        if !state.by_id.contains_key(&id) {
            return Ok(id);
        }
    }
    Err(Errno::ENOSPC)
}

fn mode_from_flags(flags: u32) -> FileMode {
    FileMode::new((flags as u16) & FileMode::PERM_MASK.bits())
}

fn check_mode_request(cred: &Credentials, perm: &MsgPerm, flags: u32) -> Result<(), Errno> {
    let requested = mode_from_flags(flags);
    if requested.has_any(FileMode::IRUSR.with(FileMode::IRGRP).with(FileMode::IROTH))
        && !cred.can_read(perm.uid, perm.gid, perm.mode)
    {
        return Err(Errno::EACCES);
    }
    if requested.has_any(FileMode::IWUSR.with(FileMode::IWGRP).with(FileMode::IWOTH))
        && !cred.can_write(perm.uid, perm.gid, perm.mode)
    {
        return Err(Errno::EACCES);
    }
    Ok(())
}

fn check_operation_permissions(
    cred: &Credentials,
    perm: &MsgPerm,
    access: Access,
) -> Result<(), Errno> {
    // CAP_IPC_OWNER 允许绕过 SysV IPC 对象的权限检查。
    if cred.has_cap(Capability::IpcOwner) {
        return Ok(());
    }
    let allowed = match access {
        Access::Receive => cred.can_read(perm.uid, perm.gid, perm.mode),
        Access::Send => cred.can_write(perm.uid, perm.gid, perm.mode),
    };
    if allowed { Ok(()) } else { Err(Errno::EACCES) }
}

fn check_control_owner(cred: &Credentials, perm: &MsgPerm) -> Result<(), Errno> {
    if cred.is_owner(perm.uid) || cred.is_owner(perm.cuid) || cred.has_cap(Capability::SysAdmin) {
        return Ok(());
    }
    Err(Errno::EPERM)
}

#[cfg(feature = "kernel-tests")]
mod tests {
    use super::*;
    use ktest::ktest;

    fn send(manager: &MsgManager, id: MsgId, mtype: i64, data: &[u8], flags: u32) {
        let queue = manager.queue_for_operation(id).expect("查找队列");
        assert_eq!(
            queue.try_send(mtype, data, flags, &Credentials::root(), 1, 0),
            Ok(MsgOpAttempt::Done),
            "发送消息 {mtype}"
        );
    }

    /// 期望队列中必有消息的接收 helper。
    fn recv(
        queue: &MsgQueue,
        msgtyp: i64,
        msgsz: usize,
        flags: u32,
        cred: &Credentials,
        pid: i32,
        now_sec: i64,
    ) -> ReceivedMsg {
        match queue
            .try_receive(msgtyp, msgsz, flags, cred, pid, now_sec)
            .expect("接收")
        {
            MsgRecvOutcome::Received(received) => received,
            MsgRecvOutcome::WouldBlock => panic!("队列应有消息"),
        }
    }

    #[ktest]
    fn message_type_selection_and_order() {
        let manager = MsgManager::new();
        let cred = Credentials::root();
        let id = manager
            .msgget(MsgKey::PRIVATE, IPC_CREAT | 0o700, &cred)
            .expect("创建队列");
        let queue = manager.queue_for_operation(id).expect("查找队列");

        send(&manager, id, 3, b"c", 0);
        send(&manager, id, 1, b"a", 0);
        send(&manager, id, 2, b"b", 0);
        send(&manager, id, 3, b"c2", 0);

        // msgtyp = 0：队首（插入顺序）。
        let got = recv(&queue, 0, 16, 0, &cred, 2, 0);
        assert_eq!((got.mtype, got.data.as_slice()), (3, b"c".as_slice()));

        // msgtyp > 0：该类型第一条（3 的 c2 排在 1/2 之前）。
        let got = recv(&queue, 2, 16, 0, &cred, 2, 0);
        assert_eq!((got.mtype, got.data.as_slice()), (2, b"b".as_slice()));

        // msgtyp < 0：类型 <= |msgtyp| 的最小类型。
        let got = recv(&queue, -2, 16, 0, &cred, 2, 0);
        assert_eq!((got.mtype, got.data.as_slice()), (1, b"a".as_slice()));

        // MSG_EXCEPT：跳过队首的 type=3，选择第一条其它类型。
        send(&manager, id, 4, b"except", 0);
        let got = recv(&queue, 3, 16, MSG_EXCEPT, &cred, 2, 0);
        assert_eq!((got.mtype, got.data.as_slice()), (4, b"except".as_slice()));

        // MSG_EXCEPT 不应移除被排除的队首消息。
        let got = recv(&queue, 0, 16, 0, &cred, 2, 0);
        assert_eq!((got.mtype, got.data.as_slice()), (3, b"c2".as_slice()));
    }

    #[ktest]
    fn msgrcv_negative_type_returns_minimum_type() {
        let manager = MsgManager::new();
        let cred = Credentials::root();
        let id = manager
            .msgget(MsgKey::PRIVATE, IPC_CREAT | 0o700, &cred)
            .expect("创建队列");
        let queue = manager.queue_for_operation(id).expect("查找队列");

        // 大 type 排在小 type 之前：msgtyp=-2 必须返回 type=1（最小），
        // 而非按入队顺序命中的第一条 type=3。
        send(&manager, id, 3, b"big", 0);
        send(&manager, id, 1, b"small", 0);
        let got = recv(&queue, -2, 16, 0, &cred, 2, 0);
        assert_eq!((got.mtype, got.data.as_slice()), (1, b"small".as_slice()));

        // 并列最小 type 取入队最早者。
        send(&manager, id, 2, b"first-two", 0);
        send(&manager, id, 2, b"second-two", 0);
        send(&manager, id, 1, b"one", 0);
        let got = recv(&queue, -2, 16, 0, &cred, 2, 0);
        assert_eq!((got.mtype, got.data.as_slice()), (1, b"one".as_slice()));
    }

    #[ktest]
    fn msg_copy_noerror_and_e2big() {
        let manager = MsgManager::new();
        let cred = Credentials::root();
        let id = manager
            .msgget(MsgKey::PRIVATE, IPC_CREAT | 0o700, &cred)
            .expect("创建队列");
        let queue = manager.queue_for_operation(id).expect("查找队列");
        send(&manager, id, 1, b"0123456789", 0);

        // 无 MSG_NOERROR 且缓冲区太短 → E2BIG，消息保留。
        assert_eq!(queue.try_receive(0, 4, 0, &cred, 2, 0), Err(Errno::E2BIG));
        let got = recv(&queue, 0, 4, MSG_NOERROR, &cred, 2, 0);
        assert_eq!(got.data, b"0123");
        assert_eq!(got.full_size, 10);

        // 重新入队并验证 MSG_COPY 不取走消息。
        send(&manager, id, 5, b"hello", 0);
        let copied = recv(&queue, 0, 16, MSG_COPY | IPC_NOWAIT, &cred, 2, 0);
        assert!(copied.copied);
        assert_eq!(copied.data, b"hello");
        let again = recv(&queue, 0, 16, 0, &cred, 2, 0);
        assert_eq!(again.data, b"hello");

        // MSG_TRUNC 仅与 MSG_COPY 同用。
        assert_eq!(
            queue.try_receive(0, 16, MSG_TRUNC, &cred, 2, 0),
            Err(Errno::EINVAL)
        );
        // MSG_EXCEPT 要求 msgtyp > 0。
        assert_eq!(
            queue.try_receive(0, 16, MSG_EXCEPT, &cred, 2, 0),
            Err(Errno::EINVAL)
        );
    }

    #[ktest]
    fn msg_full_queue_nowait_and_removal() {
        let manager = MsgManager::new();
        let cred = Credentials::root();
        let id = manager
            .msgget(MsgKey::PRIVATE, IPC_CREAT | 0o700, &cred)
            .expect("创建队列");
        let queue = manager.queue_for_operation(id).expect("查找队列");

        // 塞满 qbytes（默认 MSGMNB）后 NOWAIT 发送 → EAGAIN。
        let chunk = vec![0u8; MSGMAX];
        assert_eq!(
            queue.try_send(1, &chunk, 0, &cred, 1, 0),
            Ok(MsgOpAttempt::Done)
        );
        assert_eq!(
            queue.try_send(2, &chunk, 0, &cred, 1, 0),
            Ok(MsgOpAttempt::Done)
        );
        assert_eq!(
            queue.try_send(3, &[1u8], IPC_NOWAIT, &cred, 1, 0),
            Err(Errno::EAGAIN)
        );

        // IPC_RMID 后稳定对象返回 EIDRM。
        manager.remove(id, &cred).expect("删除队列");
        assert_eq!(
            queue.try_send(4, &[1u8], IPC_NOWAIT, &cred, 1, 0),
            Err(Errno::EIDRM)
        );
        assert!(matches!(
            queue.try_receive(0, 16, 0, &cred, 2, 0),
            Err(Errno::EIDRM)
        ));
        assert!(matches!(
            manager.queue_for_operation(id),
            Err(Errno::EINVAL)
        ));
    }

    #[ktest]
    fn msg_permissions_and_limits() {
        let manager = MsgManager::new();
        let root = Credentials::root();
        let nobody = Credentials::nobody();
        let id = manager
            .msgget(MsgKey::PRIVATE, IPC_CREAT | 0o700, &root)
            .expect("创建队列");
        let queue = manager.queue_for_operation(id).expect("查找队列");

        // nobody 无读/写权限。
        assert_eq!(
            queue.try_send(1, &[1u8], IPC_NOWAIT, &nobody, 9, 0),
            Err(Errno::EACCES)
        );
        assert_eq!(
            queue.try_receive(0, 16, IPC_NOWAIT, &nobody, 9, 0),
            Err(Errno::EACCES)
        );
        // mtype < 1 与超长消息。
        assert_eq!(
            queue.try_send(0, &[1u8], 0, &root, 1, 0),
            Err(Errno::EINVAL)
        );
        let oversized = vec![0u8; MSGMAX + 1];
        assert_eq!(
            queue.try_send(1, &oversized, 0, &root, 1, 0),
            Err(Errno::EINVAL)
        );
    }
}

/// 宿主测试（`cargo test -p general --target x86_64-unknown-linux-gnu`）。
#[cfg(test)]
mod host_tests {
    use super::super::shm::IPC_CREAT;
    use super::{MsgKey, MsgManager, MsgRecvOutcome};
    use vfs::cred::Credentials;

    #[test]
    fn msgrcv_negative_type_returns_minimum_type() {
        let manager = MsgManager::new();
        let cred = Credentials::root();
        let id = manager
            .msgget(MsgKey::PRIVATE, IPC_CREAT | 0o700, &cred)
            .expect("创建队列");
        let queue = manager.queue_for_operation(id).expect("查找队列");

        // 大 type 排在小 type 之前：msgtyp=-2 必须返回 type=1（最小），
        // 而非按入队顺序命中的第一条 type=3。
        queue.try_send(3, b"big", 0, &cred, 1, 0).expect("发送");
        queue.try_send(1, b"small", 0, &cred, 1, 0).expect("发送");
        match queue.try_receive(-2, 16, 0, &cred, 2, 0).expect("接收") {
            MsgRecvOutcome::Received(received) => {
                assert_eq!(received.mtype, 1);
                assert_eq!(received.data, b"small");
            }
            MsgRecvOutcome::WouldBlock => panic!("队列应有消息"),
        }
    }
}
