//! Linux keyrings 子系统的通用对象管理器。
//!
//! 语义对齐 `security/keys/`：
//!
//! - key 类型：`user`（可读负载）、`keyring`（成员表）、`logon`（负载不可读）；
//! - key 序列号全局递增；keyring 成员按 (type, description) 有序；
//! - 权限模型与 Linux 相同：possessor/user/group/other 四档 ×
//!   view/read/write/search/link/setattr 六位（每组 8 位，低 8 位为 possessor）；
//! - 每 uid 配额：`maxkeys`（200）/`maxbytes`（20000）；
//! - `KEY_SPEC_*` 特殊 keyring 引用：thread/process/session 属进程级状态
//!   （kernel 层经 `ProcessKeyrings` 挂到任务扩展），user/user-session 是
//!   按 uid 的全局 keyring；
//! - key 状态机：uninstantiated → instantiated/negative；revoked/expired 后
//!   惰性从 keyring 摘除；
//! - `request_key` 未命中时经 upcall 回调（kernel 注入）执行
//!   `/sbin/request-key`，调用方等待实例化或否定。

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use errno::Errno;
use sched::current_task;
use spin::Mutex;
use vfs::cred::{Credentials, Gid, Uid};

/// `KEY_SPEC_*` 特殊 keyring 引用。
pub const KEY_SPEC_THREAD_KEYRING: i32 = -1;
pub const KEY_SPEC_PROCESS_KEYRING: i32 = -2;
pub const KEY_SPEC_SESSION_KEYRING: i32 = -3;
pub const KEY_SPEC_USER_KEYRING: i32 = -4;
pub const KEY_SPEC_USER_SESSION_KEYRING: i32 = -5;
pub const KEY_SPEC_GROUP_KEYRING: i32 = -6;
pub const KEY_SPEC_REQKEY_AUTH_KEY: i32 = -7;
pub const KEY_SPEC_REQKEY_AUTH_THREAD: i32 = -8;

/// 权限位（Linux `linux/keyctl.h` 布局：每组 8 位，低组为 possessor）。
pub const KEY_POS_VIEW: u32 = 0x01;
pub const KEY_POS_READ: u32 = 0x02;
pub const KEY_POS_WRITE: u32 = 0x04;
pub const KEY_POS_SEARCH: u32 = 0x08;
pub const KEY_POS_LINK: u32 = 0x10;
pub const KEY_POS_SETATTR: u32 = 0x20;
pub const KEY_POS_ALL: u32 = 0x3f;
pub const KEY_USR_VIEW: u32 = 0x0100;
pub const KEY_USR_READ: u32 = 0x0200;
pub const KEY_USR_WRITE: u32 = 0x0400;
pub const KEY_USR_SEARCH: u32 = 0x0800;
pub const KEY_USR_LINK: u32 = 0x1000;
pub const KEY_USR_SETATTR: u32 = 0x2000;
pub const KEY_USR_ALL: u32 = 0x3f00;
pub const KEY_GRP_VIEW: u32 = 0x010000;
pub const KEY_GRP_READ: u32 = 0x020000;
pub const KEY_GRP_WRITE: u32 = 0x040000;
pub const KEY_GRP_SEARCH: u32 = 0x080000;
pub const KEY_GRP_LINK: u32 = 0x100000;
pub const KEY_GRP_SETATTR: u32 = 0x200000;
pub const KEY_GRP_ALL: u32 = 0x3f0000;
pub const KEY_OTH_VIEW: u32 = 0x01000000;
pub const KEY_OTH_READ: u32 = 0x02000000;
pub const KEY_OTH_WRITE: u32 = 0x04000000;
pub const KEY_OTH_SEARCH: u32 = 0x08000000;
pub const KEY_OTH_LINK: u32 = 0x10000000;
pub const KEY_OTH_SETATTR: u32 = 0x20000000;
pub const KEY_OTH_ALL: u32 = 0x3f000000;
pub const KEY_ALL: u32 = KEY_POS_ALL | KEY_USR_ALL | KEY_GRP_ALL | KEY_OTH_ALL;

/// 默认权限：owner 全权 + 组/他人仅 view（Linux `KEY_USR_ALL | KEY_GRP_VIEW | KEY_OTH_VIEW`）。
pub const KEY_DEFAULT_PERM: u32 = KEY_USR_ALL | KEY_GRP_VIEW | KEY_OTH_VIEW;

/// 配额（Linux 默认 `/proc/sys/kernel/keys/maxkeys`、`maxbytes`）。
pub const KEY_MAXKEYS_PER_UID: usize = 200;
pub const KEY_MAXBYTES_PER_UID: usize = 20_000;

/// 搜索深度上限（Linux `KEYRING_SEARCH_MAX_DEPTH`）。
pub const KEYRING_SEARCH_MAX_DEPTH: usize = 4;

/// key 状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyState {
    Uninstantiated,
    Instantiated,
    /// 否定实例化：搜索时视为不存在，直到超时。
    Negative,
    Revoked,
}

/// key 类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum KeyType {
    User,
    Keyring,
    Logon,
}

impl KeyType {
    pub fn name(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Keyring => "keyring",
            Self::Logon => "logon",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "user" => Some(Self::User),
            "keyring" => Some(Self::Keyring),
            "logon" => Some(Self::Logon),
            _ => None,
        }
    }
}

/// key 序列号。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KeyId(pub i32);

/// key 快照（`/proc/keys` 与 `keyctl_describe` 使用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeySnapshot {
    pub id: KeyId,
    pub key_type: KeyType,
    pub description: String,
    pub uid: u32,
    pub gid: u32,
    pub perm: u32,
    pub state: KeyState,
    pub expiry: Option<u64>,
    pub payload_len: usize,
    /// keyring 的成员数。
    pub nkeys: usize,
}

struct KeyInner {
    key_type: KeyType,
    description: String,
    uid: u32,
    gid: u32,
    perm: u32,
    payload: Vec<u8>,
    state: KeyState,
    /// 到期时间（秒，单调时钟）。`None` 表示永不到期。
    expiry: Option<u64>,
    /// keyring 类型的成员（按 (type, desc) 有序）。
    members: Vec<(String, KeyId)>,
}

/// 一个 key 对象。keyring 与普通 key 统一表示（`members` 仅 keyring 使用）。
pub struct Key {
    pub id: KeyId,
    inner: Mutex<KeyInner>,
}

impl Key {
    pub fn snapshot(&self) -> KeySnapshot {
        let inner = self.inner.lock();
        KeySnapshot {
            id: self.id,
            key_type: inner.key_type,
            description: inner.description.clone(),
            uid: inner.uid,
            gid: inner.gid,
            perm: inner.perm,
            state: inner.state,
            expiry: inner.expiry,
            payload_len: inner.payload.len(),
            nkeys: if inner.key_type == KeyType::Keyring {
                inner.members.len()
            } else {
                0
            },
        }
    }

    pub fn key_type(&self) -> KeyType {
        self.inner.lock().key_type
    }

    /// 当前状态（供 `request_key` 等待循环与调试使用）。
    pub fn state(&self) -> KeyState {
        self.inner.lock().state
    }

    /// 是否已实例化且未到期（`request_key` 命中判定）。
    pub fn live(&self, now_sec: u64) -> bool {
        self.is_live(now_sec)
    }

    /// 当前时间是否已到期（`now_sec` 由调用方提供，统一时钟源）。
    fn is_expired(&self, now_sec: u64) -> bool {
        let inner = self.inner.lock();
        inner.expiry.is_some_and(|expiry| now_sec >= expiry)
    }

    /// key 是否"存在"（未撤销、未否定、未到期）。`KEYRING_SEARCH` 语义。
    fn is_live(&self, now_sec: u64) -> bool {
        let inner = self.inner.lock();
        inner.state == KeyState::Instantiated && inner.expiry.is_none_or(|e| now_sec < e)
    }

    pub fn is_keyring(&self) -> bool {
        self.inner.lock().key_type == KeyType::Keyring
    }

    pub fn set_state(&self, state: KeyState) {
        self.inner.lock().state = state;
    }

    fn set_payload(&self, payload: Vec<u8>) {
        self.inner.lock().payload = payload;
    }

    fn set_expiry(&self, expiry: Option<u64>) {
        self.inner.lock().expiry = expiry;
    }

    fn set_uid_gid(&self, uid: u32, gid: u32) {
        let mut inner = self.inner.lock();
        inner.uid = uid;
        inner.gid = gid;
    }

    fn set_perm(&self, perm: u32) {
        self.inner.lock().perm = perm & KEY_ALL;
    }

    fn payload(&self) -> Vec<u8> {
        self.inner.lock().payload.clone()
    }

    fn description(&self) -> String {
        self.inner.lock().description.clone()
    }

    pub fn add_member(&self, member: KeyId, type_name: &str, desc: &str) {
        let mut inner = self.inner.lock();
        let key = format!("{type_name}:{desc}");
        if let Some(entry) = inner.members.iter_mut().find(|(k, _)| *k == key) {
            entry.1 = member;
        } else {
            inner.members.push((key, member));
            inner.members.sort_by(|a, b| a.0.cmp(&b.0));
        }
    }

    fn remove_member(&self, member: KeyId) -> bool {
        let mut inner = self.inner.lock();
        let before = inner.members.len();
        inner.members.retain(|(_, id)| *id != member);
        inner.members.len() != before
    }

    fn clear_members(&self) {
        self.inner.lock().members.clear();
    }

    /// 迭代成员（供递归搜索；不可在遍历中修改）。
    fn member_ids(&self) -> Vec<KeyId> {
        self.inner
            .lock()
            .members
            .iter()
            .map(|(_, id)| *id)
            .collect()
    }
}

/// 进程级 keyring 引用（thread/process/session/reqkey-auth）。
///
/// kernel 层把它作为任务扩展挂载；`fork` 共享 process/session 引用，
/// thread keyring 不继承（`CLONE_THREAD` 除外）。
#[derive(Debug, Default)]
pub struct ProcessKeyrings {
    pub thread: Mutex<Option<KeyId>>,
    pub process: Mutex<Option<KeyId>>,
    pub session: Mutex<Option<KeyId>>,
    pub reqkey_auth: Mutex<Option<KeyId>>,
}

impl ProcessKeyrings {
    pub fn new() -> Self {
        Self::default()
    }
}

/// key 管理的全局状态。
struct KeyManagerState {
    keys: BTreeMap<KeyId, Arc<Key>>,
    /// 每 uid 的 user keyring 与 user-session keyring。
    user_keyring: BTreeMap<u32, KeyId>,
    user_session_keyring: BTreeMap<u32, KeyId>,
    /// 每 uid 配额记账。
    quota_keys: BTreeMap<u32, usize>,
    quota_bytes: BTreeMap<u32, usize>,
    next_serial: i32,
}

impl KeyManagerState {
    fn new() -> Self {
        Self {
            keys: BTreeMap::new(),
            user_keyring: BTreeMap::new(),
            user_session_keyring: BTreeMap::new(),
            quota_keys: BTreeMap::new(),
            quota_bytes: BTreeMap::new(),
            next_serial: 1,
        }
    }
}

/// 全局 key 管理器。
pub struct KeyManager {
    state: Mutex<KeyManagerState>,
}

impl KeyManager {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(KeyManagerState::new()),
        }
    }

    fn allocate_serial(state: &mut KeyManagerState) -> KeyId {
        let serial = state.next_serial;
        state.next_serial = if state.next_serial == i32::MAX {
            1
        } else {
            state.next_serial + 1
        };
        KeyId(serial)
    }

    /// 创建"未实例化"的 key（`request_key` upcall 前使用）。
    pub fn create_uninstantiated(
        &self,
        key_type: KeyType,
        description: &str,
        uid: u32,
        gid: u32,
        perm: u32,
    ) -> Result<Arc<Key>, Errno> {
        self.create_key(
            key_type,
            description,
            Vec::new(),
            uid,
            gid,
            perm,
            KeyState::Uninstantiated,
            0,
        )
    }

    /// `request_key` upcall 成功后由 `/sbin/request-key` 经 `KEYCTL_INSTANTIATE`
    /// 调用；`KEYCTL_NEGATE` 调用否定版本。`instantiate` 为假表示否定。
    pub fn instantiate(
        &self,
        key_id: KeyId,
        payload: Vec<u8>,
        instantiate: bool,
        keyring_id: KeyId,
        timeout_sec: Option<u64>,
        now_sec: u64,
    ) -> Result<(), Errno> {
        let key = self.key(key_id)?;
        key.set_state(if instantiate {
            KeyState::Instantiated
        } else {
            KeyState::Negative
        });
        key.set_payload(payload);
        key.set_expiry(timeout_sec.map(|secs| now_sec.saturating_add(secs)));
        if keyring_id.0 > 0 {
            if let Ok(dest) = self.key(keyring_id) {
                if dest.is_keyring() {
                    let (type_name, desc) = {
                        let inner = key.inner.lock();
                        (inner.key_type.name(), inner.description.clone())
                    };
                    dest.add_member(key_id, type_name, &desc);
                }
            }
        }
        Ok(())
    }

    /// 创建一个 key（不加入任何 keyring）。配额按 key 的 uid 记账。
    fn create_key(
        &self,
        key_type: KeyType,
        description: &str,
        payload: Vec<u8>,
        uid: u32,
        gid: u32,
        perm: u32,
        state: KeyState,
        now_sec: u64,
    ) -> Result<Arc<Key>, Errno> {
        if description.is_empty() {
            return Err(Errno::EINVAL);
        }
        let mut guard = self.state.lock();
        let quota_keys = guard.quota_keys.get(&uid).copied().unwrap_or(0);
        let quota_bytes = guard.quota_bytes.get(&uid).copied().unwrap_or(0);
        if quota_keys >= KEY_MAXKEYS_PER_UID {
            return Err(Errno::EDQUOT);
        }
        let bytes = payload.len().saturating_add(description.len());
        if quota_bytes.saturating_add(bytes) > KEY_MAXBYTES_PER_UID {
            return Err(Errno::EDQUOT);
        }
        let id = Self::allocate_serial(&mut guard);
        let key = Arc::new(Key {
            id,
            inner: Mutex::new(KeyInner {
                key_type,
                description: description.to_string(),
                uid,
                gid,
                perm,
                payload,
                state,
                expiry: if state == KeyState::Negative {
                    Some(now_sec.saturating_add(60))
                } else {
                    None
                },
                members: Vec::new(),
            }),
        });
        guard.keys.insert(id, Arc::clone(&key));
        guard.quota_keys.insert(uid, quota_keys + 1);
        guard.quota_bytes.insert(uid, quota_bytes + bytes);
        Ok(key)
    }

    /// 按序列号取 key（校验存在性，不校验权限）。
    pub fn key(&self, id: KeyId) -> Result<Arc<Key>, Errno> {
        self.state
            .lock()
            .keys
            .get(&id)
            .cloned()
            .ok_or(Errno::ENOKEY)
    }

    /// 销毁 key（引用为 0 时由 `drop` 自然回收；此处移除注册与配额）。
    fn destroy(&self, id: KeyId) {
        let mut guard = self.state.lock();
        if let Some(key) = guard.keys.remove(&id) {
            let inner = key.inner.lock();
            let bytes = inner.payload.len().saturating_add(inner.description.len());
            if let Some(quota_keys) = guard.quota_keys.get_mut(&inner.uid) {
                *quota_keys = quota_keys.saturating_sub(1);
            }
            if let Some(quota_bytes) = guard.quota_bytes.get_mut(&inner.uid) {
                *quota_bytes = quota_bytes.saturating_sub(bytes);
            }
        }
    }

    /// 从指定 keyring（含递归）搜索 `(type, description)` 匹配的 key。
    ///
    /// 返回第一个命中的 key；搜索过程按 Linux 语义只经过有 search 权限的
    /// keyring，并受 [`KEYRING_SEARCH_MAX_DEPTH`] 限制。
    pub fn search(
        &self,
        keyring_id: KeyId,
        key_type: KeyType,
        description: &str,
        cred: &Credentials,
        now_sec: u64,
    ) -> Result<Arc<Key>, Errno> {
        self.search_inner(keyring_id, key_type, description, cred, now_sec, 0)
            .ok_or(Errno::ENOKEY)
    }

    fn search_inner(
        &self,
        keyring_id: KeyId,
        key_type: KeyType,
        description: &str,
        cred: &Credentials,
        now_sec: u64,
        depth: usize,
    ) -> Option<Arc<Key>> {
        if depth > KEYRING_SEARCH_MAX_DEPTH {
            return None;
        }
        let keyring = self.key(keyring_id).ok()?;
        if !keyring.is_keyring() {
            return None;
        }
        if !permission_ok(&keyring, cred, KEY_POS_SEARCH) {
            return None;
        }
        let want = format!("{}:{description}", key_type.name());
        for member_id in keyring.member_ids() {
            let member = self.key(member_id).ok()?;
            if member.is_live(now_sec) {
                let key = {
                    let inner = member.inner.lock();
                    format!("{}:{}", inner.key_type.name(), inner.description)
                };
                if key == want {
                    return Some(member);
                }
            }
        }
        // 递归：成员 keyring 的成员。
        for member_id in keyring.member_ids() {
            let member = self.key(member_id).ok()?;
            if member.is_keyring() && member.is_live(now_sec) {
                if let Some(found) =
                    self.search_inner(member_id, key_type, description, cred, now_sec, depth + 1)
                {
                    return Some(found);
                }
            }
        }
        None
    }

    /// `KEY_SPEC_*` 或直接序列号 → key。
    ///
    /// `spec == 0` 表示"当前线程的默认 keyring"（Linux `KEY_SPEC_REQKEY_AUTH_KEY`
    /// 之后的默认链：thread → process → session → user-session → user）。
    /// 当前任务的根 ns pid（process keyring 描述用）。
    fn current_pid_for_keyring(&self) -> u32 {
        sched::current_task().pid_root_cached().unwrap_or(0) as u32
    }

    pub fn resolve_spec(
        &self,
        spec: i32,
        process: &ProcessKeyrings,
        cred: &Credentials,
        now_sec: u64,
    ) -> Result<Arc<Key>, Errno> {
        let id: KeyId = match spec {
            KEY_SPEC_THREAD_KEYRING => process.thread.lock().ok_or(Errno::ENOKEY)?,
            KEY_SPEC_PROCESS_KEYRING => {
                // Linux `install_process_keyring`：进程没有显式 process keyring
                // 时按需创建（描述 `_pid.<pid>`，权限走默认）。
                let mut guard = process.process.lock();
                if let Some(id) = *guard {
                    id
                } else {
                    drop(guard);
                    let pid = self.current_pid_for_keyring();
                    let key = self.create_key(
                        KeyType::Keyring,
                        &format!("_pid.{pid}"),
                        Vec::new(),
                        cred.euid.0,
                        0,
                        KEY_DEFAULT_PERM,
                        KeyState::Instantiated,
                        0,
                    )?;
                    *process.process.lock() = Some(key.id);
                    key.id
                }
            }
            KEY_SPEC_SESSION_KEYRING => {
                if let Some(id) = *process.session.lock() {
                    id
                } else {
                    // 无显式 session keyring 时退回 user-session（Linux 默认）。
                    self.user_session(cred.euid.0, cred, now_sec)?
                }
            }
            KEY_SPEC_USER_KEYRING => self.user_keyring(cred.euid.0, cred)?,
            KEY_SPEC_USER_SESSION_KEYRING => self.user_session(cred.euid.0, cred, now_sec)?,
            KEY_SPEC_REQKEY_AUTH_KEY => process.reqkey_auth.lock().ok_or(Errno::ENOKEY)?,
            spec if spec > 0 => KeyId(spec),
            _ => return Err(Errno::ENOKEY),
        };
        self.key(id)
    }

    /// 每 uid 的 user keyring（不存在则创建）。
    pub fn user_keyring(&self, uid: u32, cred: &Credentials) -> Result<KeyId, Errno> {
        let mut guard = self.state.lock();
        if let Some(id) = guard.user_keyring.get(&uid) {
            return Ok(*id);
        }
        drop(guard);
        let key = self.create_key(
            KeyType::Keyring,
            &format!("_uid.{uid}"),
            Vec::new(),
            uid,
            0,
            KEY_DEFAULT_PERM,
            KeyState::Instantiated,
            0,
        )?;
        let mut guard = self.state.lock();
        if let Some(id) = guard.user_keyring.get(&uid) {
            return Ok(*id);
        }
        guard.user_keyring.insert(uid, key.id);
        let _ = cred;
        Ok(key.id)
    }

    /// 每 uid 的 user-session keyring（不存在则创建）。
    pub fn user_session(&self, uid: u32, cred: &Credentials, now_sec: u64) -> Result<KeyId, Errno> {
        let mut guard = self.state.lock();
        if let Some(id) = guard.user_session_keyring.get(&uid) {
            return Ok(*id);
        }
        drop(guard);
        let key = self.create_key(
            KeyType::Keyring,
            &format!("_uid_ses.{uid}"),
            Vec::new(),
            uid,
            0,
            KEY_DEFAULT_PERM,
            KeyState::Instantiated,
            now_sec,
        )?;
        let mut guard = self.state.lock();
        if let Some(id) = guard.user_session_keyring.get(&uid) {
            return Ok(*id);
        }
        guard.user_session_keyring.insert(uid, key.id);
        Ok(key.id)
    }

    /// `add_key`：创建并加入目标 keyring。
    pub fn add_key(
        &self,
        key_type: KeyType,
        description: &str,
        payload: Vec<u8>,
        keyring_id: KeyId,
        cred: &Credentials,
        now_sec: u64,
    ) -> Result<KeyId, Errno> {
        let key = self.create_key(
            key_type,
            description,
            payload,
            cred.euid.0,
            cred.egid.0,
            KEY_DEFAULT_PERM,
            KeyState::Instantiated,
            now_sec,
        )?;
        // 同一 keyring 中已有同 (type, desc) 的 key → 替换（Linux `__key_create`）。
        let keyring = self.key(keyring_id)?;
        if !keyring.is_keyring() {
            return Err(Errno::ENOTDIR);
        }
        if !permission_ok(&keyring, cred, KEY_POS_WRITE) {
            return Err(Errno::EACCES);
        }
        let existing = self
            .search(keyring_id, key_type, description, cred, now_sec)
            .ok();
        keyring.add_member(key.id, key_type.name(), description);
        if let Some(old) = existing {
            if old.id != key.id {
                keyring.remove_member(old.id);
                self.destroy(old.id);
            }
        }
        Ok(key.id)
    }

    /// `keyctl(KEYCTL_LINK)`。
    pub fn link(&self, keyring_id: KeyId, key_id: KeyId, cred: &Credentials) -> Result<(), Errno> {
        let keyring = self.key(keyring_id)?;
        if !keyring.is_keyring() {
            return Err(Errno::ENOTDIR);
        }
        if !permission_ok(&keyring, cred, KEY_POS_WRITE) {
            return Err(Errno::EACCES);
        }
        let key = self.key(key_id)?;
        if !permission_ok(&key, cred, KEY_POS_LINK) {
            return Err(Errno::EACCES);
        }
        let (type_name, desc) = {
            let inner = key.inner.lock();
            (inner.key_type.name(), inner.description.clone())
        };
        keyring.add_member(key_id, type_name, &desc);
        Ok(())
    }

    /// `keyctl(KEYCTL_UNLINK)`。
    pub fn unlink(
        &self,
        keyring_id: KeyId,
        key_id: KeyId,
        cred: &Credentials,
    ) -> Result<(), Errno> {
        let keyring = self.key(keyring_id)?;
        if !keyring.is_keyring() {
            return Err(Errno::ENOTDIR);
        }
        if !permission_ok(&keyring, cred, KEY_POS_WRITE) {
            return Err(Errno::EACCES);
        }
        if keyring.remove_member(key_id) {
            self.destroy(key_id);
            Ok(())
        } else {
            Err(Errno::ENOENT)
        }
    }

    /// `keyctl(KEYCTL_UPDATE)`：仅 user/logon 可更新；keyring 报 `EOPNOTSUPP`。
    pub fn update(&self, key_id: KeyId, payload: Vec<u8>, cred: &Credentials) -> Result<(), Errno> {
        let key = self.key(key_id)?;
        if !permission_ok(&key, cred, KEY_POS_WRITE) {
            return Err(Errno::EACCES);
        }
        let inner = key.inner.lock();
        if inner.key_type == KeyType::Keyring {
            return Err(Errno::EOPNOTSUPP);
        }
        drop(inner);
        // 更新也受配额约束。
        let mut guard = self.state.lock();
        let uid = key.inner.lock().uid;
        let quota_bytes = guard.quota_bytes.get(&uid).copied().unwrap_or(0);
        let bytes = payload.len().saturating_add(key.description().len());
        if quota_bytes.saturating_add(bytes) > KEY_MAXBYTES_PER_UID {
            return Err(Errno::EDQUOT);
        }
        key.set_payload(payload);
        guard.quota_bytes.insert(uid, quota_bytes + bytes);
        Ok(())
    }

    /// `keyctl(KEYCTL_REVOKE)`。
    pub fn revoke(&self, key_id: KeyId, cred: &Credentials) -> Result<(), Errno> {
        let key = self.key(key_id)?;
        if !permission_ok(&key, cred, KEY_POS_WRITE) {
            return Err(Errno::EACCES);
        }
        key.set_state(KeyState::Revoked);
        Ok(())
    }

    /// `keyctl(KEYCTL_CHOWN)`：改 uid/gid 需 `CAP_SYS_ADMIN`（简化：owner 或
    /// 相同 uid）；`KEY_USR_SETATTR` 权限。
    pub fn chown(
        &self,
        key_id: KeyId,
        uid: Option<u32>,
        gid: Option<u32>,
        cred: &Credentials,
    ) -> Result<(), Errno> {
        let key = self.key(key_id)?;
        if !permission_ok(&key, cred, KEY_POS_SETATTR) {
            return Err(Errno::EACCES);
        }
        let current = {
            let inner = key.inner.lock();
            (inner.uid, inner.gid)
        };
        let new_uid = uid.unwrap_or(current.0);
        let new_gid = gid.unwrap_or(current.1);
        if new_uid != current.0 && !cred.has_cap(vfs::cred::Capability::SysAdmin) {
            return Err(Errno::EPERM);
        }
        key.set_uid_gid(new_uid, new_gid);
        Ok(())
    }

    /// `keyctl(KEYCTL_SETPERM)`。
    pub fn setperm(&self, key_id: KeyId, perm: u32, cred: &Credentials) -> Result<(), Errno> {
        let key = self.key(key_id)?;
        if !permission_ok(&key, cred, KEY_POS_SETATTR) {
            return Err(Errno::EACCES);
        }
        key.set_perm(perm);
        Ok(())
    }

    /// `keyctl(KEYCTL_CLEAR)`。
    pub fn clear(&self, keyring_id: KeyId, cred: &Credentials) -> Result<(), Errno> {
        let keyring = self.key(keyring_id)?;
        if !keyring.is_keyring() {
            return Err(Errno::ENOTDIR);
        }
        if !permission_ok(&keyring, cred, KEY_POS_WRITE) {
            return Err(Errno::EACCES);
        }
        for member in keyring.member_ids() {
            keyring.remove_member(member);
            self.destroy(member);
        }
        Ok(())
    }

    /// `keyctl(KEYCTL_SET_TIMEOUT)`：设置到期时间（相对秒）。
    pub fn set_timeout(
        &self,
        key_id: KeyId,
        seconds: u64,
        cred: &Credentials,
        now_sec: u64,
    ) -> Result<(), Errno> {
        let key = self.key(key_id)?;
        if !permission_ok(&key, cred, KEY_POS_SETATTR) {
            return Err(Errno::EACCES);
        }
        key.set_expiry(if seconds == 0 {
            None
        } else {
            Some(now_sec.saturating_add(seconds))
        });
        Ok(())
    }

    /// `keyctl(KEYCTL_INVALIDATE)`。
    pub fn invalidate(&self, key_id: KeyId, cred: &Credentials) -> Result<(), Errno> {
        let key = self.key(key_id)?;
        if !permission_ok(&key, cred, KEY_POS_WRITE) {
            return Err(Errno::EACCES);
        }
        key.set_state(KeyState::Revoked);
        Ok(())
    }

    /// `keyctl(KEYCTL_READ)`：user/logon 返回负载；keyring 返回成员序列号列表
    /// （4 字节每个）。logon 类型不可读（`EACCES`，Linux 语义）。
    pub fn read(&self, key_id: KeyId, cred: &Credentials) -> Result<Vec<u8>, Errno> {
        let key = self.key(key_id)?;
        if !permission_ok(&key, cred, KEY_USR_READ) {
            return Err(Errno::EACCES);
        }
        let inner = key.inner.lock();
        match inner.key_type {
            KeyType::User => Ok(inner.payload.clone()),
            KeyType::Logon => Err(Errno::EACCES),
            KeyType::Keyring => Ok(inner
                .members
                .iter()
                .map(|(_, id)| (id.0 as u32).to_le_bytes())
                .flatten()
                .collect()),
        }
    }

    /// `keyctl(KEYCTL_DESCRIBE)`：`type;uid;gid;perm;desc`。
    pub fn describe(&self, key_id: KeyId, cred: &Credentials) -> Result<String, Errno> {
        let key = self.key(key_id)?;
        if !permission_ok(&key, cred, KEY_USR_VIEW) {
            return Err(Errno::EACCES);
        }
        let inner = key.inner.lock();
        Ok(format!(
            "{};{};{};{:08x};{}",
            inner.key_type.name(),
            inner.uid,
            inner.gid,
            inner.perm,
            inner.description
        ))
    }

    /// 惰性清理：从 keyring 摘除已撤销/到期成员（在遍历入口调用）。
    pub fn reap_expired(&self, now_sec: u64) {
        let mut guard = self.state.lock();
        let keys: Vec<Arc<Key>> = guard.keys.values().cloned().collect();
        for key in keys {
            if !key.is_keyring() {
                continue;
            }
            let expired: Vec<KeyId> = key
                .member_ids()
                .into_iter()
                .filter(|id| {
                    guard
                        .keys
                        .get(id)
                        .is_none_or(|member| !member.is_live(now_sec))
                })
                .collect();
            for id in expired {
                key.remove_member(id);
            }
        }
    }

    /// 全部 key 快照（`/proc/keys`）。
    pub fn snapshot_all(&self) -> Vec<KeySnapshot> {
        let guard = self.state.lock();
        guard.keys.values().map(|key| key.snapshot()).collect()
    }

    /// 每 uid 配额快照（`/proc/key-users`）。
    pub fn quota_snapshot(&self) -> Vec<(u32, usize, usize)> {
        let guard = self.state.lock();
        let mut result = Vec::new();
        for uid in guard.quota_keys.keys() {
            result.push((
                *uid,
                guard.quota_keys.get(uid).copied().unwrap_or(0),
                guard.quota_bytes.get(uid).copied().unwrap_or(0),
            ));
        }
        result
    }
}

impl Default for KeyManager {
    fn default() -> Self {
        Self::new()
    }
}

/// 权限检查（Linux `key_permission` 语义：possessor → user → group → other）。
pub fn permission_ok(key: &Key, cred: &Credentials, mask: u32) -> bool {
    let inner = key.inner.lock();
    let uid = inner.uid;
    let gid = inner.gid;
    let perm = inner.perm;
    let euid = cred.euid.0;
    drop(inner);

    // possessor：拥有者（简化：uid 匹配即 possessor，与 Linux 的"持有引用"
    // 语义在单用户系统等价）。mask 是 KEY_POS_* 位（0x3f 内）；按
    // Linux `key_task_permission` 的语义逐级检查 possessor/USR/GRP/OTH。
    if euid == uid {
        return perm & mask != 0 || perm & (mask << 8) != 0;
    }
    if cred.groups.iter().any(|g| g.0 == gid) || cred.egid.0 == gid {
        return perm & (mask << 16) != 0;
    }
    perm & (mask << 24) != 0
}

/// 供 kernel 层使用的 `KEY_SPEC_*` 解析辅助。
pub fn default_keyring_chain(
    process: &ProcessKeyrings,
    cred: &Credentials,
    manager: &KeyManager,
    now_sec: u64,
) -> KeyId {
    // Linux `KEY_SPEC_REQKEY_AUTH_KEY` 未设置时的默认链：
    // thread → process → session → user-session。
    if let Some(id) = *process.thread.lock() {
        return id;
    }
    if let Some(id) = *process.process.lock() {
        return id;
    }
    if let Some(id) = *process.session.lock() {
        return id;
    }
    manager
        .user_session(cred.euid.0, cred, now_sec)
        .unwrap_or(KeyId(0))
}
