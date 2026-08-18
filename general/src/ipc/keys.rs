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

/// `KEYCTL_SET_REQKEY_KEYRING` 的默认请求 keyring 偏好（Linux `keyctl.h`）。
pub const KEY_REQKEY_DEFL_DEFAULT: i32 = 0;
pub const KEY_REQKEY_DEFL_THREAD_KEYRING: i32 = 1;
pub const KEY_REQKEY_DEFL_PROCESS_KEYRING: i32 = 2;
pub const KEY_REQKEY_DEFL_SESSION_KEYRING: i32 = 3;
pub const KEY_REQKEY_DEFL_USER_KEYRING: i32 = 4;
pub const KEY_REQKEY_DEFL_USER_SESSION_KEYRING: i32 = 5;
pub const KEY_REQKEY_DEFL_REQUESTOR_KEYRING: i32 = 7;
pub const KEY_REQKEY_DEFL_NO_CHANGE: i32 = -1;

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
    /// `KEYCTL_RESTRICT_KEYRING` 施加的 key 类型限制（仅 keyring 有效）。
    /// `None` 表示不限制。
    restriction: Option<KeyType>,
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

    /// 读取当前限制（仅 keyring 有意义）。
    fn restriction(&self) -> Option<KeyType> {
        self.inner.lock().restriction
    }

    fn set_restriction(&self, restriction: Option<KeyType>) {
        self.inner.lock().restriction = restriction;
    }

    fn payload(&self) -> Vec<u8> {
        self.inner.lock().payload.clone()
    }

    /// key 的描述字符串（授权 key 校验、`add_member` 等使用）。
    pub fn description(&self) -> String {
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
#[derive(Debug)]
pub struct ProcessKeyrings {
    pub thread: Mutex<Option<KeyId>>,
    pub process: Mutex<Option<KeyId>>,
    pub session: Mutex<Option<KeyId>>,
    pub reqkey_auth: Mutex<Option<KeyId>>,
    /// `KEYCTL_SET_REQKEY_KEYRING` 的默认请求 keyring 偏好（`jit_keyring`）。
    pub reqkey_default: Mutex<i32>,
}

impl Default for ProcessKeyrings {
    fn default() -> Self {
        Self {
            thread: Mutex::new(None),
            process: Mutex::new(None),
            session: Mutex::new(None),
            reqkey_auth: Mutex::new(None),
            reqkey_default: Mutex::new(KEY_REQKEY_DEFL_DEFAULT),
        }
    }
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
        // 只有未实例化的 key 才能被实例化/否定；已实例化/否定/撤销的 key
        // 再 instantiate 视为越权（Linux `key_reject_and_link` 语义）。
        if key.state() != KeyState::Uninstantiated {
            return Err(Errno::EACCES);
        }
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

    /// 校验授权 key（描述 `_reqkey_auth.<key_id>`）是否指向目标 key。
    ///
    /// `request_key` 创建未实例化 key 时同时创建一个描述为
    /// `_reqkey_auth.<key_id>` 的 User 授权 key；`KEYCTL_INSTANTIATE`/
    /// `NEGATE`/`REJECT` 经 [`ProcessKeyrings::reqkey_auth`] 携带它。
    pub fn auth_key_matches(&self, auth_id: KeyId, key_id: KeyId) -> bool {
        match self.key(auth_id) {
            Ok(auth) => auth.description() == format!("_reqkey_auth.{}", key_id.0),
            Err(_) => false,
        }
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
                restriction: None,
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

    /// 销毁 key。
    ///
    /// 引用计数语义：全局 map（`KeyManagerState::keys`）是 key 的唯一注册
    /// 持有者；keyring 只存 `KeyId`，不持 `Arc<Key>`。若 map 之外仍有强引用
    /// （如调用方经 [`KeyManager::key`]/[`KeyManager::search`] 拿到的 `Arc<Key>`
    /// 仍存活），此时从 map 注销会留下"悬空引用 + 配额提前释放"：key 对象仍
    /// 存活，但已不在注册表、配额也不再记账，反复 create/unlink 可绕过配额。
    ///
    /// 因此只有当 map 是最后一个强引用（移除后 `strong_count == 1`，即仅剩
    /// 本函数局部引用）时才注销并结算配额；否则标记为 Revoked 并保留在 map，
    /// 待引用自然归零后由后续清理路径回收。
    fn destroy(&self, id: KeyId) {
        let mut guard = self.state.lock();
        let Some(key) = guard.keys.remove(&id) else {
            return;
        };
        if Arc::strong_count(&key) > 1 {
            key.set_state(KeyState::Revoked);
            guard.keys.insert(id, key);
            return;
        }
        let inner = key.inner.lock();
        let bytes = inner.payload.len().saturating_add(inner.description.len());
        if let Some(quota_keys) = guard.quota_keys.get_mut(&inner.uid) {
            *quota_keys = quota_keys.saturating_sub(1);
        }
        if let Some(quota_bytes) = guard.quota_bytes.get_mut(&inner.uid) {
            *quota_bytes = quota_bytes.saturating_sub(bytes);
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
                let old_id = old.id;
                keyring.remove_member(old_id);
                // 释放调用方持有的 `Arc<Key>`，让 `destroy` 能看到 map 是否
                // 是最后一个强引用，从而正确结算配额（见 `destroy` 注释）。
                drop(old);
                self.destroy(old_id);
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
        if let Some(restriction) = keyring.restriction() {
            if key.key_type() != restriction {
                return Err(Errno::EACCES);
            }
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
        let (uid, old_bytes) = {
            let inner = key.inner.lock();
            if inner.key_type == KeyType::Keyring {
                return Err(Errno::EOPNOTSUPP);
            }
            (inner.uid, inner.payload.len())
        };
        let new_bytes = payload.len();
        // 更新只按负载差量结算：描述长度不变，不计入增量。避免反复 update 让
        // 每 uid 字节配额单调增长。
        let mut guard = self.state.lock();
        let quota_bytes = guard.quota_bytes.get(&uid).copied().unwrap_or(0);
        let updated = quota_bytes
            .saturating_sub(old_bytes)
            .saturating_add(new_bytes);
        if updated > KEY_MAXBYTES_PER_UID {
            return Err(Errno::EDQUOT);
        }
        key.set_payload(payload);
        guard.quota_bytes.insert(uid, updated);
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

    /// `keyctl(KEYCTL_CHOWN)`：改 uid 需 `CAP_SYS_ADMIN`；改 gid 需
    /// `CAP_SYS_ADMIN` 或调用者属于目标组（Linux `keyctl_chown` 语义）；
    /// 操作本身还需 `KEY_POS_SETATTR` 权限。
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
        if new_gid != current.1
            && !cred.has_cap(vfs::cred::Capability::SysAdmin)
            && cred.egid.0 != new_gid
            && !cred.groups.iter().any(|g| g.0 == new_gid)
        {
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

    /// `keyctl(KEYCTL_RESTRICT_KEYRING)`：限制 keyring 只接受指定类型的 key。
    ///
    /// `restriction == None` 表示不限制类型（Linux 允许只给 restriction 字符串
    /// 而不给 type）。本内核的 key 类型固定为 user/keyring/logon，因此只实现
    /// type 级限制；restriction 字符串（LSM 风格）在 syscall 层校验后可安全忽略。
    pub fn restrict_keyring(
        &self,
        keyring_id: KeyId,
        restriction: Option<KeyType>,
        cred: &Credentials,
    ) -> Result<(), Errno> {
        let keyring = self.key(keyring_id)?;
        if !keyring.is_keyring() {
            return Err(Errno::ENOTDIR);
        }
        if !permission_ok(&keyring, cred, KEY_POS_SETATTR) {
            return Err(Errno::EACCES);
        }
        keyring.set_restriction(restriction);
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
        // Linux `key_validate` 用 KEY_NEED_SEARCH 校验 KEYCTL_INVALIDATE。
        if !permission_ok(&key, cred, KEY_POS_SEARCH) {
            return Err(Errno::EACCES);
        }
        key.set_state(KeyState::Revoked);
        Ok(())
    }

    /// `keyctl(KEYCTL_READ)`：user/logon 返回负载；keyring 返回成员序列号列表
    /// （4 字节每个）。logon 类型不可读（`EACCES`，Linux 语义）。
    pub fn read(&self, key_id: KeyId, cred: &Credentials) -> Result<Vec<u8>, Errno> {
        let key = self.key(key_id)?;
        // `permission_ok` 的 mask 约定是 possessor 位（`KEY_POS_*`），内部按
        // possessor/user/group/other 逐级左移 8 位检查；传 `KEY_USR_READ` 会让
        // group/other 档位失效。
        if !permission_ok(&key, cred, KEY_POS_READ) {
            return Err(Errno::EACCES);
        }
        let inner = key.inner.lock();
        match inner.key_type {
            KeyType::User => Ok(inner.payload.clone()),
            // Linux 读取 logon key 返回 EOPNOTSUPP。
            KeyType::Logon => Err(Errno::EOPNOTSUPP),
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
        if !permission_ok(&key, cred, KEY_POS_VIEW) {
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

/// 在进程 keyring 链上搜索已实例化的 key（`request_key` 的搜索阶段）。
///
/// 链序为 thread → process → session → user-session → user；若调用方经
/// `KEYCTL_SET_REQKEY_KEYRING` 指定了偏好环，则优先搜索该环。逐环调用
/// [`KeyManager::search`]，命中即返回 key 序列号。
pub fn search_process_keyrings(
    process: &ProcessKeyrings,
    cred: &Credentials,
    manager: &KeyManager,
    key_type: KeyType,
    description: &str,
    now_sec: u64,
) -> Option<KeyId> {
    let mut candidates: Vec<KeyId> = Vec::new();

    // 偏好环优先（KEY_REQKEY_DEFL_*）。
    match *process.reqkey_default.lock() {
        KEY_REQKEY_DEFL_THREAD_KEYRING => {
            if let Some(id) = *process.thread.lock() {
                push_unique_keyring(&mut candidates, id);
            }
        }
        KEY_REQKEY_DEFL_PROCESS_KEYRING => {
            if let Some(id) = *process.process.lock() {
                push_unique_keyring(&mut candidates, id);
            }
        }
        KEY_REQKEY_DEFL_SESSION_KEYRING => {
            if let Some(id) = *process.session.lock() {
                push_unique_keyring(&mut candidates, id);
            }
        }
        KEY_REQKEY_DEFL_USER_KEYRING => {
            if let Ok(id) = manager.user_keyring(cred.euid.0, cred) {
                push_unique_keyring(&mut candidates, id);
            }
        }
        KEY_REQKEY_DEFL_USER_SESSION_KEYRING => {
            if let Ok(id) = manager.user_session(cred.euid.0, cred, now_sec) {
                push_unique_keyring(&mut candidates, id);
            }
        }
        _ => {}
    }

    // 默认链：thread → process → session → user-session → user。
    if let Some(id) = *process.thread.lock() {
        push_unique_keyring(&mut candidates, id);
    }
    if let Some(id) = *process.process.lock() {
        push_unique_keyring(&mut candidates, id);
    }
    if let Some(id) = *process.session.lock() {
        push_unique_keyring(&mut candidates, id);
    }
    if let Ok(id) = manager.user_session(cred.euid.0, cred, now_sec) {
        push_unique_keyring(&mut candidates, id);
    }
    if let Ok(id) = manager.user_keyring(cred.euid.0, cred) {
        push_unique_keyring(&mut candidates, id);
    }

    for id in candidates {
        if let Ok(key) = manager.search(id, key_type, description, cred, now_sec) {
            return Some(key.id);
        }
    }
    None
}

/// 把非零 keyring id 去重后加入候选列表（`KeyId(0)` 是"无 keyring"哨兵）。
fn push_unique_keyring(list: &mut Vec<KeyId>, id: KeyId) {
    if id.0 > 0 && !list.contains(&id) {
        list.push(id);
    }
}

/// 供 kernel 层使用的 `KEY_SPEC_*` 解析辅助。
pub fn default_keyring_chain(
    process: &ProcessKeyrings,
    cred: &Credentials,
    manager: &KeyManager,
    now_sec: u64,
) -> KeyId {
    // `KEYCTL_SET_REQKEY_KEYRING` 设置的具体 keyring 优先；否则走
    // Linux `KEY_SPEC_REQKEY_AUTH_KEY` 未设置时的默认链：
    // thread → process → session → user-session。
    match *process.reqkey_default.lock() {
        KEY_REQKEY_DEFL_THREAD_KEYRING => {
            if let Some(id) = *process.thread.lock() {
                return id;
            }
        }
        KEY_REQKEY_DEFL_PROCESS_KEYRING => {
            if let Some(id) = *process.process.lock() {
                return id;
            }
        }
        KEY_REQKEY_DEFL_SESSION_KEYRING => {
            if let Some(id) = *process.session.lock() {
                return id;
            }
        }
        KEY_REQKEY_DEFL_USER_KEYRING => {
            return manager.user_keyring(cred.euid.0, cred).unwrap_or(KeyId(0));
        }
        KEY_REQKEY_DEFL_USER_SESSION_KEYRING => {
            return manager
                .user_session(cred.euid.0, cred, now_sec)
                .unwrap_or(KeyId(0));
        }
        _ => {}
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;
    use alloc::vec;
    use vfs::cred::{CapSet, Credentials, Gid, Uid};

    fn cred(uid: u32, gid: u32) -> Credentials {
        Credentials {
            uid: Uid(uid),
            euid: Uid(uid),
            suid: Uid(uid),
            fsuid: Uid(uid),
            gid: Gid(gid),
            egid: Gid(gid),
            sgid: Gid(gid),
            fsgid: Gid(gid),
            groups: Vec::new(),
            caps: CapSet::EMPTY,
        }
    }

    fn make_keyring(manager: &KeyManager, uid: u32) -> KeyId {
        manager.user_keyring(uid, &cred(uid, 0)).unwrap()
    }

    fn quota_bytes(manager: &KeyManager, uid: u32) -> usize {
        manager
            .quota_snapshot()
            .into_iter()
            .find(|(u, _, _)| *u == uid)
            .map(|(_, _, bytes)| bytes)
            .unwrap_or(0)
    }

    #[test]
    fn instantiate_requires_uninstantiated_state() {
        let manager = KeyManager::new();
        let key = manager
            .create_uninstantiated(KeyType::User, "k", 1000, 1000, KEY_DEFAULT_PERM)
            .unwrap();
        assert_eq!(key.state(), KeyState::Uninstantiated);
        manager
            .instantiate(key.id, vec![1, 2, 3], true, KeyId(0), None, 0)
            .unwrap();
        assert_eq!(key.state(), KeyState::Instantiated);
        // 已实例化的 key 再 instantiate 应 EACCES。
        assert_eq!(
            manager.instantiate(key.id, vec![4], true, KeyId(0), None, 0),
            Err(Errno::EACCES)
        );
    }

    #[test]
    fn auth_key_matches_target_description() {
        let manager = KeyManager::new();
        let key = manager
            .create_uninstantiated(KeyType::User, "k", 1000, 1000, KEY_DEFAULT_PERM)
            .unwrap();
        let auth = manager
            .create_uninstantiated(
                KeyType::User,
                &format!("_reqkey_auth.{}", key.id.0),
                1000,
                1000,
                KEY_DEFAULT_PERM,
            )
            .unwrap();
        assert!(manager.auth_key_matches(auth.id, key.id));
        assert!(!manager.auth_key_matches(KeyId(9999), key.id));
        let other = manager
            .create_uninstantiated(KeyType::User, "k2", 1000, 1000, KEY_DEFAULT_PERM)
            .unwrap();
        assert!(!manager.auth_key_matches(auth.id, other.id));
    }

    #[test]
    fn search_process_keyrings_walks_default_chain() {
        let manager = KeyManager::new();
        let process = ProcessKeyrings::new();
        let creds = cred(1000, 1000);
        // 命中链上最后一环（user keyring）。
        let ring = make_keyring(&manager, 1000);
        manager
            .add_key(KeyType::User, "needle", b"v".to_vec(), ring, &creds, 0)
            .unwrap();
        let found = search_process_keyrings(&process, &creds, &manager, KeyType::User, "needle", 0);
        assert!(found.is_some());
        // 未命中返回 None。
        assert!(
            search_process_keyrings(&process, &creds, &manager, KeyType::User, "absent", 0)
                .is_none()
        );
    }

    #[test]
    fn invalidate_requires_search_not_write() {
        let manager = KeyManager::new();
        // 拥有者只有 SEARCH、没有 WRITE，仍可 invalidate（Linux KEY_NEED_SEARCH）。
        let key = manager
            .create_uninstantiated(KeyType::User, "k", 1000, 1000, KEY_POS_SEARCH)
            .unwrap();
        assert_eq!(manager.invalidate(key.id, &cred(1000, 0)), Ok(()));
        assert_eq!(key.state(), KeyState::Revoked);

        // 没有任何权限的 key 不可 invalidate。
        let no_perm = manager
            .create_uninstantiated(KeyType::User, "k2", 1000, 1000, 0)
            .unwrap();
        assert_eq!(
            manager.invalidate(no_perm.id, &cred(1000, 0)),
            Err(Errno::EACCES)
        );
    }

    #[test]
    fn chown_gid_requires_cap_or_membership() {
        let manager = KeyManager::new();
        let key = manager
            .create_uninstantiated(KeyType::User, "k", 1000, 1000, KEY_DEFAULT_PERM)
            .unwrap();
        // 无 CAP_SYS_ADMIN、目标 gid 不属于调用者 → EPERM。
        assert_eq!(
            manager.chown(key.id, None, Some(2000), &cred(1000, 1000)),
            Err(Errno::EPERM)
        );
        // 目标 gid 是调用者 egid → 允许。
        let mut by_egid = cred(1000, 1000);
        by_egid.egid = Gid(2000);
        assert_eq!(manager.chown(key.id, None, Some(2000), &by_egid), Ok(()));
        // 目标 gid 在附加组列表 → 允许。
        let mut by_groups = cred(1000, 1000);
        by_groups.groups = vec![Gid(3000)];
        assert_eq!(manager.chown(key.id, None, Some(3000), &by_groups), Ok(()));
    }

    #[test]
    fn update_quota_accounts_payload_delta() {
        let manager = KeyManager::new();
        let key = manager
            .create_uninstantiated(KeyType::User, "k", 1000, 1000, KEY_DEFAULT_PERM)
            .unwrap();
        // create_key 记账：空负载 + 描述 "k"（1 字节）= 1 字节。
        let base = quota_bytes(&manager, 1000);
        assert_eq!(base, 1);
        // 更新到 100 字节：delta = +100。
        manager
            .update(key.id, vec![0u8; 100], &cred(1000, 0))
            .unwrap();
        assert_eq!(quota_bytes(&manager, 1000), base + 100);
        // 再更新到 10 字节：delta = -90，配额应回落而非单调增长。
        manager
            .update(key.id, vec![0u8; 10], &cred(1000, 0))
            .unwrap();
        assert_eq!(quota_bytes(&manager, 1000), base + 10);
    }

    #[test]
    fn destroy_defers_while_externally_referenced() {
        let manager = KeyManager::new();
        let ring = make_keyring(&manager, 1000);
        let creds = cred(1000, 0);
        let id = manager
            .add_key(KeyType::User, "k", b"payload".to_vec(), ring, &creds, 0)
            .unwrap();
        let quota_before = quota_bytes(&manager, 1000);
        assert!(quota_before > 0);

        // 持有外部 Arc 时 unlink：不注销、不释放配额，只标记 Revoked。
        let held = manager.key(id).unwrap();
        manager.unlink(ring, id, &creds).unwrap();
        assert_eq!(manager.key(id).unwrap().state(), KeyState::Revoked);
        assert_eq!(quota_bytes(&manager, 1000), quota_before);
        drop(held);

        // 无外部引用时 unlink：正常注销并释放配额。
        let id2 = manager
            .add_key(KeyType::User, "k2", b"payload".to_vec(), ring, &creds, 0)
            .unwrap();
        let quota_mid = quota_bytes(&manager, 1000);
        manager.unlink(ring, id2, &creds).unwrap();
        assert!(manager.key(id2).is_err());
        assert!(quota_bytes(&manager, 1000) < quota_mid);
    }

    #[test]
    fn read_logon_key_returns_eopnotsupp() {
        let manager = KeyManager::new();
        let key = manager
            .create_uninstantiated(KeyType::Logon, "l", 1000, 1000, KEY_DEFAULT_PERM)
            .unwrap();
        assert_eq!(manager.read(key.id, &cred(1000, 0)), Err(Errno::EOPNOTSUPP));
    }
}
