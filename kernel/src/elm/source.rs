//! ELM EBI Source 注册表。
//!
//! ELM Core 只消费 EBI 协议对象。Projection Source 在这里登记投影函数；注册表
//! 负责所有者、代际、影子切换和调用引用计数，不把任何具体容器格式写入核心。

use alloc::vec::Vec;

use elm_model::{
    ELM_EKI_BUILTIN_ID, ELM_EKI_PROJECTION_SOURCE_ID, ELM_IMAGE_SESSION_DEFAULT_TTL_MS,
    ELM_IMAGE_SESSION_DIGEST_LEN, ELM_IMAGE_SESSION_HASH_SHA256, ELM_IMAGE_SESSION_MAX_ACTIVE,
    ELM_IMAGE_SESSION_MAX_CHUNK, ELM_IMAGE_SESSION_MAX_LENGTH, ELM_IMAGE_SESSION_MAX_PER_OWNER,
    ELM_IMAGE_SESSION_MAX_RESERVED_BYTES, ELM_IMAGE_SESSION_MAX_TTL_MS, ELM_MGR_BUILTIN_ID,
    ELM_MGR_STATUS_BUSY, ELM_MGR_STATUS_EXPIRED, ELM_MGR_STATUS_INTEGRITY, ELM_MGR_STATUS_INVALID,
    ELM_MGR_STATUS_NO_MEMORY, ELM_MGR_STATUS_NOT_FOUND, ELM_MGR_STATUS_PERMISSION, ElmEbiArch,
    ElmEbiImage, ElmEbiLoadStatus, ElmEkiSelector, ElmId, ElmImageReader,
    ElmImageSessionBeginRequestV1, ElmImageSessionInfoV1, ElmImageSessionState, ElmPrincipal,
    Generation, Sha256, parse_eki_image_for,
};
use sched::sync::Spinlock;

const PROJECTION_SOURCE_LIMIT: usize = 256;

pub(crate) type ElmProjectionSourceProvider =
    fn(reader: &dyn ElmImageReader, arch: ElmEbiArch) -> Result<ElmEbiImage, ElmEbiLoadStatus>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProjectionSourceRegistryError {
    Invalid,
    Duplicate,
    Conflict,
    NotFound,
    StaleGeneration,
    Busy,
    Capacity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProjectionSourceSnapshot {
    pub id: u64,
    pub owner: ElmId,
    pub generation: Generation,
    pub active_refs: u32,
    pub active: bool,
    pub suspended: bool,
    pub retiring: bool,
}

#[derive(Clone, Copy)]
struct ProjectionSourceProviderRuntime {
    id: u64,
    owner: ElmId,
    generation: Generation,
    provider: ElmProjectionSourceProvider,
    active_refs: u32,
    active: bool,
    suspended: bool,
    retiring: bool,
}

struct ProjectionSourceLease {
    id: u64,
    owner: ElmId,
    generation: Generation,
    provider: ElmProjectionSourceProvider,
}

pub(crate) struct ProjectionSourceSuspension {
    owner: ElmId,
    generation: Generation,
    count: usize,
    armed: bool,
}

impl ProjectionSourceSuspension {
    pub(crate) fn keep_suspended(mut self) -> usize {
        self.armed = false;
        self.count
    }

    pub(crate) fn retire(mut self) -> Result<usize, ProjectionSourceRegistryError> {
        let result = retire_projection_sources_owned_by(self.owner, self.generation);
        // 退役失败时也必须保持隔离，不能让已进入卸载流程的 source 被 Drop 自动恢复。
        self.armed = false;
        result
    }
}

impl Drop for ProjectionSourceSuspension {
    fn drop(&mut self) {
        if self.armed {
            if let Err(err) = resume_projection_sources(self.owner, self.generation) {
                log::error!(
                    "[elm] Projection Source 自动恢复失败 owner={} generation={}: {:?}",
                    self.owner.0,
                    self.generation.0,
                    err
                );
            }
        }
    }
}

impl Drop for ProjectionSourceLease {
    fn drop(&mut self) {
        release_projection_source(self.id, self.owner, self.generation);
    }
}

static PROJECTION_SOURCES: Spinlock<Vec<ProjectionSourceProviderRuntime>> =
    Spinlock::new(Vec::new());

struct ImageSessionRecord {
    id: u64,
    owner: ElmPrincipal,
    state: ElmImageSessionState,
    total_len: usize,
    created_at_ns: u64,
    expires_at_ns: u64,
    expected_digest: [u8; ELM_IMAGE_SESSION_DIGEST_LEN],
    actual_digest: [u8; ELM_IMAGE_SESSION_DIGEST_LEN],
    hasher: Sha256,
    bytes: Vec<u8>,
}

struct ImageSessionRegistry {
    next_id: u64,
    sessions: Vec<ImageSessionRecord>,
}

impl ImageSessionRegistry {
    const fn new() -> Self {
        Self {
            next_id: 1,
            sessions: Vec::new(),
        }
    }

    fn cleanup_expired(&mut self, now_ns: u64) {
        self.sessions
            .retain(|session| now_ns < session.expires_at_ns);
    }

    fn owner_index(&self, owner: ElmPrincipal, session_id: u64) -> Result<usize, i32> {
        let Some(index) = self
            .sessions
            .iter()
            .position(|session| session.id == session_id)
        else {
            return Err(ELM_MGR_STATUS_NOT_FOUND);
        };
        if self.sessions[index].owner != owner {
            return Err(ELM_MGR_STATUS_PERMISSION);
        }
        Ok(index)
    }

    fn info(session: &ImageSessionRecord) -> ElmImageSessionInfoV1 {
        ElmImageSessionInfoV1 {
            abi_version: elm_model::ELM_IMAGE_SESSION_ABI_VERSION,
            struct_size: core::mem::size_of::<ElmImageSessionInfoV1>() as u16,
            state: session.state as u32,
            session_id: session.id,
            total_len: session.total_len as u64,
            written_len: session.bytes.len() as u64,
            created_at_ns: session.created_at_ns,
            expires_at_ns: session.expires_at_ns,
            hash_alg: ELM_IMAGE_SESSION_HASH_SHA256,
            digest_len: ELM_IMAGE_SESSION_DIGEST_LEN as u16,
            flags: 0,
            expected_digest: session.expected_digest,
            actual_digest: session.actual_digest,
        }
    }
}

static IMAGE_SESSIONS: Spinlock<ImageSessionRegistry> = Spinlock::new(ImageSessionRegistry::new());

pub(crate) struct SealedImageSession {
    bytes: Vec<u8>,
}

impl ElmImageReader for SealedImageSession {
    fn len(&self) -> u64 {
        self.bytes.len() as u64
    }

    fn read_at(&self, offset: u64, output: &mut [u8]) -> Result<(), ElmEbiLoadStatus> {
        let start = usize::try_from(offset).map_err(|_| ElmEbiLoadStatus::InvalidUnit)?;
        let end = start
            .checked_add(output.len())
            .ok_or(ElmEbiLoadStatus::InvalidUnit)?;
        let source = self
            .bytes
            .get(start..end)
            .ok_or(ElmEbiLoadStatus::InvalidUnit)?;
        output.copy_from_slice(source);
        Ok(())
    }
}

pub(crate) fn begin_image_session(
    owner: ElmPrincipal,
    request: ElmImageSessionBeginRequestV1,
    now_ns: u64,
) -> Result<ElmImageSessionInfoV1, i32> {
    let total_len = usize::try_from(request.total_len).map_err(|_| ELM_MGR_STATUS_INVALID)?;
    let ttl_ms = if request.ttl_ms == 0 {
        ELM_IMAGE_SESSION_DEFAULT_TTL_MS
    } else {
        request.ttl_ms
    };
    if request.abi_version != elm_model::ELM_IMAGE_SESSION_ABI_VERSION
        || request.hash_alg != ELM_IMAGE_SESSION_HASH_SHA256
        || request.flags != 0
        || request.digest_len as usize != ELM_IMAGE_SESSION_DIGEST_LEN
        || request.reserved0 != 0
        || request.reserved1 != 0
        || request.expected_digest == [0; ELM_IMAGE_SESSION_DIGEST_LEN]
        || total_len == 0
        || total_len > ELM_IMAGE_SESSION_MAX_LENGTH
        || ttl_ms > ELM_IMAGE_SESSION_MAX_TTL_MS
    {
        return Err(ELM_MGR_STATUS_INVALID);
    }
    let ttl_ns = u64::from(ttl_ms)
        .checked_mul(1_000_000)
        .ok_or(ELM_MGR_STATUS_INVALID)?;
    let expires_at_ns = now_ns.checked_add(ttl_ns).ok_or(ELM_MGR_STATUS_INVALID)?;

    let mut sessions = IMAGE_SESSIONS.lock();
    sessions.cleanup_expired(now_ns);
    if sessions.sessions.len() >= ELM_IMAGE_SESSION_MAX_ACTIVE
        || sessions
            .sessions
            .iter()
            .filter(|session| session.owner == owner)
            .count()
            >= ELM_IMAGE_SESSION_MAX_PER_OWNER
    {
        return Err(ELM_MGR_STATUS_BUSY);
    }
    let reserved_bytes = sessions
        .sessions
        .iter()
        .try_fold(0usize, |total, session| {
            total.checked_add(session.total_len)
        })
        .ok_or(ELM_MGR_STATUS_NO_MEMORY)?;
    if reserved_bytes
        .checked_add(total_len)
        .is_none_or(|total| total > ELM_IMAGE_SESSION_MAX_RESERVED_BYTES)
    {
        return Err(ELM_MGR_STATUS_NO_MEMORY);
    }
    let session_id = sessions.next_id;
    sessions.next_id = sessions.next_id.checked_add(1).ok_or(ELM_MGR_STATUS_BUSY)?;
    sessions
        .sessions
        .try_reserve(1)
        .map_err(|_| ELM_MGR_STATUS_NO_MEMORY)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(total_len)
        .map_err(|_| ELM_MGR_STATUS_NO_MEMORY)?;
    sessions.sessions.push(ImageSessionRecord {
        id: session_id,
        owner,
        state: ElmImageSessionState::Uploading,
        total_len,
        created_at_ns: now_ns,
        expires_at_ns,
        expected_digest: request.expected_digest,
        actual_digest: [0; ELM_IMAGE_SESSION_DIGEST_LEN],
        hasher: Sha256::new(),
        bytes,
    });
    Ok(ImageSessionRegistry::info(
        sessions.sessions.last().expect("刚插入的镜像会话必须存在"),
    ))
}

pub(crate) fn write_image_session(
    owner: ElmPrincipal,
    session_id: u64,
    offset: u64,
    chunk: &[u8],
    now_ns: u64,
) -> Result<ElmImageSessionInfoV1, i32> {
    if session_id == 0 || chunk.is_empty() || chunk.len() > ELM_IMAGE_SESSION_MAX_CHUNK {
        return Err(ELM_MGR_STATUS_INVALID);
    }
    let offset = usize::try_from(offset).map_err(|_| ELM_MGR_STATUS_INVALID)?;
    let mut sessions = IMAGE_SESSIONS.lock();
    let expired = sessions
        .sessions
        .iter()
        .any(|session| session.id == session_id && now_ns >= session.expires_at_ns);
    sessions.cleanup_expired(now_ns);
    if expired {
        return Err(ELM_MGR_STATUS_EXPIRED);
    }
    let index = sessions.owner_index(owner, session_id)?;
    let session = &mut sessions.sessions[index];
    if session.state != ElmImageSessionState::Uploading {
        return Err(ELM_MGR_STATUS_BUSY);
    }
    if offset != session.bytes.len()
        || offset
            .checked_add(chunk.len())
            .is_none_or(|end| end > session.total_len)
    {
        return Err(ELM_MGR_STATUS_INVALID);
    }
    session.bytes.extend_from_slice(chunk);
    session.hasher.update(chunk);
    Ok(ImageSessionRegistry::info(session))
}

pub(crate) fn seal_image_session(
    owner: ElmPrincipal,
    session_id: u64,
    now_ns: u64,
) -> Result<ElmImageSessionInfoV1, i32> {
    let mut sessions = IMAGE_SESSIONS.lock();
    let expired = sessions
        .sessions
        .iter()
        .any(|session| session.id == session_id && now_ns >= session.expires_at_ns);
    sessions.cleanup_expired(now_ns);
    if expired {
        return Err(ELM_MGR_STATUS_EXPIRED);
    }
    let index = sessions.owner_index(owner, session_id)?;
    let session = &mut sessions.sessions[index];
    if session.state != ElmImageSessionState::Uploading || session.bytes.len() != session.total_len
    {
        return Err(ELM_MGR_STATUS_BUSY);
    }
    session.actual_digest = session.hasher.clone().finish();
    if session.actual_digest != session.expected_digest {
        return Err(ELM_MGR_STATUS_INTEGRITY);
    }
    session.state = ElmImageSessionState::Sealed;
    Ok(ImageSessionRegistry::info(session))
}

pub(crate) fn abort_image_session(
    owner: ElmPrincipal,
    session_id: u64,
    now_ns: u64,
) -> Result<ElmImageSessionInfoV1, i32> {
    let mut sessions = IMAGE_SESSIONS.lock();
    let expired = sessions
        .sessions
        .iter()
        .any(|session| session.id == session_id && now_ns >= session.expires_at_ns);
    sessions.cleanup_expired(now_ns);
    if expired {
        return Err(ELM_MGR_STATUS_EXPIRED);
    }
    let index = sessions.owner_index(owner, session_id)?;
    let session = sessions.sessions.remove(index);
    Ok(ImageSessionRegistry::info(&session))
}

pub(crate) fn query_image_session(
    owner: ElmPrincipal,
    session_id: u64,
    now_ns: u64,
) -> Result<ElmImageSessionInfoV1, i32> {
    let mut sessions = IMAGE_SESSIONS.lock();
    let expired = sessions
        .sessions
        .iter()
        .any(|session| session.id == session_id && now_ns >= session.expires_at_ns);
    sessions.cleanup_expired(now_ns);
    if expired {
        return Err(ELM_MGR_STATUS_EXPIRED);
    }
    let index = sessions.owner_index(owner, session_id)?;
    Ok(ImageSessionRegistry::info(&sessions.sessions[index]))
}

pub(crate) fn consume_image_session(
    owner: ElmPrincipal,
    session_id: u64,
    now_ns: u64,
) -> Result<SealedImageSession, i32> {
    let mut sessions = IMAGE_SESSIONS.lock();
    let expired = sessions
        .sessions
        .iter()
        .any(|session| session.id == session_id && now_ns >= session.expires_at_ns);
    sessions.cleanup_expired(now_ns);
    if expired {
        return Err(ELM_MGR_STATUS_EXPIRED);
    }
    let index = sessions.owner_index(owner, session_id)?;
    if sessions.sessions[index].state != ElmImageSessionState::Sealed {
        return Err(ELM_MGR_STATUS_BUSY);
    }
    let session = sessions.sessions.remove(index);
    Ok(SealedImageSession {
        bytes: session.bytes,
    })
}

/// 登记由指定 ELM 代际实现的 Projection Source。
///
/// 同一所有者可以先登记下一代同名 source；它会保持影子状态，直到显式执行代际
/// 切换。不同所有者不得复用同一 source id。
pub(crate) fn register_projection_source_owned(
    id: u64,
    owner: ElmId,
    generation: Generation,
    provider: ElmProjectionSourceProvider,
) -> Result<(), ProjectionSourceRegistryError> {
    if id == 0 || owner.0 == 0 || generation.0 == 0 {
        return Err(ProjectionSourceRegistryError::Invalid);
    }
    let mut sources = PROJECTION_SOURCES.lock();
    if let Some(source) = sources
        .iter()
        .find(|source| source.id == id && source.owner == owner && source.generation == generation)
    {
        return if core::ptr::fn_addr_eq(source.provider, provider) && !source.retiring {
            Ok(())
        } else {
            Err(ProjectionSourceRegistryError::Duplicate)
        };
    }
    if sources
        .iter()
        .any(|source| source.id == id && source.owner != owner && !source.retiring)
    {
        return Err(ProjectionSourceRegistryError::Conflict);
    }
    if sources.len() >= PROJECTION_SOURCE_LIMIT {
        return Err(ProjectionSourceRegistryError::Capacity);
    }
    if sources.len() == sources.capacity() && sources.try_reserve(1).is_err() {
        return Err(ProjectionSourceRegistryError::Capacity);
    }
    let active = !sources
        .iter()
        .any(|source| source.id == id && source.owner == owner && !source.retiring);
    sources.push(ProjectionSourceProviderRuntime {
        id,
        owner,
        generation,
        provider,
        active_refs: 0,
        active,
        suspended: false,
        retiring: false,
    });
    Ok(())
}

/// 兼容内核内建与测试投影器的快捷登记入口。
pub(crate) fn register_projection_source(id: u64, provider: ElmProjectionSourceProvider) -> bool {
    register_projection_source_owned(id, ELM_MGR_BUILTIN_ID, Generation::FIRST, provider).is_ok()
}

/// 登记内建 `eki` 子单元提供的原生投影源。
pub(crate) fn register_builtin_eki_projection_source() -> Result<(), ProjectionSourceRegistryError>
{
    register_projection_source_owned(
        ELM_EKI_PROJECTION_SOURCE_ID,
        ELM_EKI_BUILTIN_ID,
        Generation::FIRST,
        project_builtin_eki_image,
    )
}

pub(crate) fn commit_projection_source_generation(
    owner: ElmId,
    old_generation: Generation,
    new_generation: Generation,
    commit: impl FnOnce() -> bool,
) -> Result<(usize, bool), ProjectionSourceRegistryError> {
    if owner.0 == 0
        || old_generation.0 == 0
        || new_generation.0 == 0
        || old_generation == new_generation
    {
        return Err(ProjectionSourceRegistryError::Invalid);
    }
    let mut sources = PROJECTION_SOURCES.lock();
    let old_count = sources
        .iter()
        .filter(|source| {
            source.owner == owner
                && source.generation == old_generation
                && source.suspended
                && !source.retiring
        })
        .count();
    if sources.iter().any(|old| {
        old.owner == owner
            && old.generation == old_generation
            && old.suspended
            && !old.retiring
            && !sources.iter().any(|source| {
                source.id == old.id
                    && source.owner == owner
                    && source.generation == new_generation
                    && !source.retiring
            })
    }) {
        return Err(ProjectionSourceRegistryError::NotFound);
    }

    if !commit() {
        return Ok((0, false));
    }

    // 先激活全部新代际，再在同一把锁内退役旧代际，外部观察不到中间状态。
    for index in 0..sources.len() {
        if sources[index].owner != owner
            || sources[index].generation != new_generation
            || sources[index].retiring
        {
            continue;
        }
        let id = sources[index].id;
        if sources.iter().any(|old| {
            old.id == id
                && old.owner == owner
                && old.generation == old_generation
                && old.suspended
                && !old.retiring
        }) {
            sources[index].active = true;
            sources[index].suspended = false;
        }
    }
    for source in sources.iter_mut().filter(|source| {
        source.owner == owner
            && source.generation == old_generation
            && source.suspended
            && !source.retiring
    }) {
        source.active = false;
        source.suspended = false;
        source.retiring = true;
    }
    remove_retired_sources(&mut sources);
    Ok((old_count, true))
}

pub(crate) fn projection_source_generation_ready(
    owner: ElmId,
    old_generation: Generation,
    new_generation: Generation,
) -> bool {
    let sources = PROJECTION_SOURCES.lock();
    sources
        .iter()
        .filter(|source| {
            source.owner == owner
                && source.generation == old_generation
                && source.suspended
                && !source.retiring
        })
        .all(|old| {
            sources.iter().any(|new| {
                new.id == old.id
                    && new.owner == owner
                    && new.generation == new_generation
                    && !new.retiring
            })
        })
}

pub(crate) fn suspend_projection_sources(
    owner: ElmId,
    generation: Generation,
) -> Result<usize, ProjectionSourceRegistryError> {
    let mut sources = PROJECTION_SOURCES.lock();
    if sources.iter().any(|source| {
        source.owner == owner && source.generation == generation && source.active_refs != 0
    }) {
        return Err(ProjectionSourceRegistryError::Busy);
    }
    let mut suspended = 0usize;
    for source in sources.iter_mut().filter(|source| {
        source.owner == owner
            && source.generation == generation
            && source.active
            && !source.retiring
    }) {
        source.active = false;
        source.suspended = true;
        suspended += 1;
    }
    Ok(suspended)
}

pub(crate) fn suspend_projection_sources_guard(
    owner: ElmId,
    generation: Generation,
) -> Result<ProjectionSourceSuspension, ProjectionSourceRegistryError> {
    let count = suspend_projection_sources(owner, generation)?;
    Ok(ProjectionSourceSuspension {
        owner,
        generation,
        count,
        armed: true,
    })
}

pub(crate) fn resume_projection_sources(
    owner: ElmId,
    generation: Generation,
) -> Result<usize, ProjectionSourceRegistryError> {
    let mut sources = PROJECTION_SOURCES.lock();
    if owner.0 == 0 || generation.0 == 0 {
        return Err(ProjectionSourceRegistryError::Invalid);
    }
    if sources.iter().any(|suspended| {
        suspended.owner == owner
            && suspended.generation == generation
            && suspended.suspended
            && !suspended.retiring
            && sources.iter().any(|source| {
                source.id == suspended.id
                    && source.owner == owner
                    && source.generation != generation
                    && source.active
                    && !source.retiring
            })
    }) {
        return Err(ProjectionSourceRegistryError::Conflict);
    }
    let mut resumed = 0usize;
    for source in sources.iter_mut().filter(|source| {
        source.owner == owner
            && source.generation == generation
            && source.suspended
            && !source.retiring
    }) {
        source.active = true;
        source.suspended = false;
        resumed += 1;
    }
    Ok(resumed)
}

/// 注销一个精确代际的 Projection Source。
pub(crate) fn unregister_projection_source(
    id: u64,
    owner: ElmId,
    generation: Generation,
) -> Result<(), ProjectionSourceRegistryError> {
    let mut sources = PROJECTION_SOURCES.lock();
    let Some(index) = sources.iter().position(|source| {
        source.id == id && source.owner == owner && source.generation == generation
    }) else {
        if sources
            .iter()
            .any(|source| source.id == id && source.owner == owner)
        {
            return Err(ProjectionSourceRegistryError::StaleGeneration);
        }
        return Err(ProjectionSourceRegistryError::NotFound);
    };
    if sources[index].active_refs != 0 {
        return Err(ProjectionSourceRegistryError::Busy);
    }
    sources[index].active = false;
    sources[index].suspended = false;
    sources[index].retiring = true;
    sources.remove(index);
    Ok(())
}

/// 将一个所有者代际的全部 source 置为退役状态。
pub(crate) fn retire_projection_sources_owned_by(
    owner: ElmId,
    generation: Generation,
) -> Result<usize, ProjectionSourceRegistryError> {
    let mut sources = PROJECTION_SOURCES.lock();
    if owner.0 == 0 || generation.0 == 0 {
        return Err(ProjectionSourceRegistryError::Invalid);
    }
    if sources.iter().any(|source| {
        source.owner == owner && source.generation == generation && source.active_refs != 0
    }) {
        return Err(ProjectionSourceRegistryError::Busy);
    }
    let found = sources
        .iter()
        .filter(|source| source.owner == owner && source.generation == generation)
        .count();
    for source in sources
        .iter_mut()
        .filter(|source| source.owner == owner && source.generation == generation)
    {
        source.active = false;
        source.suspended = false;
        source.retiring = true;
    }
    remove_retired_sources(&mut sources);
    Ok(found)
}

pub(crate) fn owner_generation_busy(owner: ElmId, generation: Generation) -> bool {
    PROJECTION_SOURCES.lock().iter().any(|source| {
        source.owner == owner && source.generation == generation && source.active_refs != 0
    })
}

pub(crate) fn projection_source_snapshots()
-> Result<Vec<ProjectionSourceSnapshot>, ProjectionSourceRegistryError> {
    let sources = PROJECTION_SOURCES.lock();
    let mut snapshots = Vec::new();
    snapshots
        .try_reserve_exact(sources.len())
        .map_err(|_| ProjectionSourceRegistryError::Capacity)?;
    snapshots.extend(sources.iter().map(|source| ProjectionSourceSnapshot {
        id: source.id,
        owner: source.owner,
        generation: source.generation,
        active_refs: source.active_refs,
        active: source.active,
        suspended: source.suspended,
        retiring: source.retiring,
    }));
    Ok(snapshots)
}

pub(crate) fn project_ebi_image(
    id: u64,
    reader: &dyn ElmImageReader,
    arch: ElmEbiArch,
) -> Result<ElmEbiImage, ElmEbiLoadStatus> {
    let lease = acquire_projection_source(id)?;
    (lease.provider)(reader, arch)
}

fn acquire_projection_source(id: u64) -> Result<ProjectionSourceLease, ElmEbiLoadStatus> {
    let mut sources = PROJECTION_SOURCES.lock();
    let Some(index) = sources
        .iter()
        .enumerate()
        .filter(|(_, source)| source.id == id && source.active && !source.retiring)
        .max_by_key(|(_, source)| source.generation)
        .map(|(index, _)| index)
    else {
        return Err(ElmEbiLoadStatus::RuntimeRejected);
    };
    let Some(active_refs) = sources[index].active_refs.checked_add(1) else {
        return Err(ElmEbiLoadStatus::RuntimeRejected);
    };
    sources[index].active_refs = active_refs;
    Ok(ProjectionSourceLease {
        id: sources[index].id,
        owner: sources[index].owner,
        generation: sources[index].generation,
        provider: sources[index].provider,
    })
}

fn release_projection_source(id: u64, owner: ElmId, generation: Generation) {
    let mut sources = PROJECTION_SOURCES.lock();
    let Some(index) = sources.iter().position(|source| {
        source.id == id && source.owner == owner && source.generation == generation
    }) else {
        return;
    };
    if sources[index].active_refs == 0 {
        log::error!(
            "[elm] Projection Source 引用计数下溢 id={} owner={} generation={}",
            id,
            owner.0,
            generation.0
        );
        return;
    }
    sources[index].active_refs -= 1;
    if sources[index].retiring && sources[index].active_refs == 0 {
        sources.remove(index);
    }
}

fn remove_retired_sources(sources: &mut Vec<ProjectionSourceProviderRuntime>) {
    sources.retain(|source| !source.retiring || source.active_refs != 0);
}

pub(crate) fn project_builtin_eki_image(
    reader: &dyn ElmImageReader,
    arch: ElmEbiArch,
) -> Result<ElmEbiImage, ElmEbiLoadStatus> {
    // 这是内建 `eki` 子单元的投影入口；管理通道只选择 Source，不直接拥有格式解析。
    let payload = reader.read_all(ELM_IMAGE_SESSION_MAX_LENGTH)?;
    let profile_hash = super::kernel_symbols::catalog_profile_hash()
        .map_err(|_| ElmEbiLoadStatus::RuntimeRejected)?;
    let image = parse_eki_image_for(
        &payload,
        ElmEkiSelector {
            arch,
            profile_hash,
            bridge_abi_version: super::kernel_symbols::KERNEL_API_BRIDGE_ABI_VERSION,
        },
    )?;
    image.validate(arch)?;
    Ok(image)
}
