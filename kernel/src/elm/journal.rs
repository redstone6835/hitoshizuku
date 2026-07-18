//! ELM 运行时持久日志。
//!
//! 本模块只定义顺序追加、回放和哈希链语义，不依赖 VFS、块设备或网络。平台或
//! 子系统可以在 ELM 初始化前登记一个静态后端；未登记后端时运行在可观测的易失
//! 模式，登记为强制后端后任何完整性或写入故障都会封闭后续变更操作。

use alloc::vec::Vec;

use elm_model::sha256;
use sched::sync::Spinlock;

pub(crate) const ELM_JOURNAL_RECORD_SIZE: usize = 240;
const ELM_JOURNAL_MAGIC: u32 = u32::from_le_bytes(*b"ELMJ");
const ELM_JOURNAL_ABI_VERSION: u16 = 1;
const ELM_JOURNAL_RING_LIMIT: usize = 256;
const ELM_JOURNAL_MAX_BACKEND_BYTES: u64 = 16 * 1024 * 1024;

const OFFSET_MAGIC: usize = 0;
const OFFSET_VERSION: usize = 4;
const OFFSET_SIZE: usize = 6;
const OFFSET_SEQUENCE: usize = 8;
const OFFSET_TIMESTAMP: usize = 16;
const OFFSET_ACTION: usize = 24;
const OFFSET_STATUS: usize = 28;
const OFFSET_CELL: usize = 32;
const OFFSET_SUBJECT: usize = 40;
const OFFSET_AUX: usize = 48;
const OFFSET_VALUE: usize = 56;
const OFFSET_BLOCKERS: usize = 64;
const OFFSET_FLAGS: usize = 72;
const OFFSET_ROLLBACK_AUTHORITY_ID: usize = 80;
const OFFSET_MODULE_DIGEST: usize = 112;
const OFFSET_SIGNER_KEY_ID: usize = 144;
const OFFSET_PREVIOUS_HASH: usize = 176;
const OFFSET_RECORD_HASH: usize = 208;

pub(crate) const ELM_JOURNAL_FLAG_AUTHORIZATION: u32 = 1 << 0;
pub(crate) const ELM_JOURNAL_FLAG_TRUST_ACCEPTANCE: u32 = 1 << 1;
const ELM_JOURNAL_FLAGS_MASK: u32 =
    ELM_JOURNAL_FLAG_AUTHORIZATION | ELM_JOURNAL_FLAG_TRUST_ACCEPTANCE;
const ELM_JOURNAL_ACTION_TRUST_ACCEPTANCE: u32 = 0x8000_0001;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JournalError {
    AlreadyRegistered,
    AlreadyInitialized,
    InvalidBackend,
    Capacity,
    Io(i32),
    Malformed,
    Rollback,
    SequenceExhausted,
    Sealed,
}

pub(crate) struct ElmJournalBackendOps {
    /// 返回后端可读取的最大字节数。
    pub capacity: fn() -> u64,
    /// 从固定偏移读取一个完整记录；返回 0 表示到达日志尾。
    pub read: fn(offset: u64, out: &mut [u8]) -> Result<usize, i32>,
    /// 原子追加一个完整记录。
    pub append: fn(record: &[u8]) -> Result<(), i32>,
    /// 将此前追加的数据推进到后端承诺的持久边界。
    pub sync: fn() -> Result<(), i32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct JournalRecord {
    pub sequence: u64,
    pub timestamp_ns: u64,
    pub action: u32,
    pub status: i32,
    pub cell: u64,
    pub subject: u64,
    pub aux: u64,
    pub value: u64,
    pub blockers: u64,
    pub flags: u32,
    pub rollback_authority_id: [u8; 32],
    pub module_digest: [u8; 32],
    pub signer_key_id: [u8; 32],
    pub previous_hash: [u8; 32],
    pub record_hash: [u8; 32],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct JournalTrustEpoch {
    pub rollback_authority_id: [u8; 32],
    pub module_digest: [u8; 32],
    pub signer_key_id: [u8; 32],
    pub release_epoch: u64,
}

impl JournalRecord {
    #[allow(clippy::too_many_arguments)]
    fn new(
        sequence: u64,
        timestamp_ns: u64,
        action: u32,
        status: i32,
        cell: u64,
        subject: u64,
        aux: u64,
        value: u64,
        blockers: u64,
        flags: u32,
        previous_hash: [u8; 32],
    ) -> Self {
        Self::new_with_identity(
            sequence,
            timestamp_ns,
            action,
            status,
            cell,
            subject,
            aux,
            value,
            blockers,
            flags,
            [0; 32],
            [0; 32],
            [0; 32],
            previous_hash,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn new_with_identity(
        sequence: u64,
        timestamp_ns: u64,
        action: u32,
        status: i32,
        cell: u64,
        subject: u64,
        aux: u64,
        value: u64,
        blockers: u64,
        flags: u32,
        rollback_authority_id: [u8; 32],
        module_digest: [u8; 32],
        signer_key_id: [u8; 32],
        previous_hash: [u8; 32],
    ) -> Self {
        let mut record = Self {
            sequence,
            timestamp_ns,
            action,
            status,
            cell,
            subject,
            aux,
            value,
            blockers,
            flags,
            rollback_authority_id,
            module_digest,
            signer_key_id,
            previous_hash,
            record_hash: [0; 32],
        };
        record.record_hash = sha256(&record.encode_with_hash([0; 32]));
        record
    }

    fn encode(self) -> [u8; ELM_JOURNAL_RECORD_SIZE] {
        self.encode_with_hash(self.record_hash)
    }

    fn encode_with_hash(self, hash: [u8; 32]) -> [u8; ELM_JOURNAL_RECORD_SIZE] {
        let mut out = [0u8; ELM_JOURNAL_RECORD_SIZE];
        write_u32(&mut out, OFFSET_MAGIC, ELM_JOURNAL_MAGIC);
        write_u16(&mut out, OFFSET_VERSION, ELM_JOURNAL_ABI_VERSION);
        write_u16(&mut out, OFFSET_SIZE, ELM_JOURNAL_RECORD_SIZE as u16);
        write_u64(&mut out, OFFSET_SEQUENCE, self.sequence);
        write_u64(&mut out, OFFSET_TIMESTAMP, self.timestamp_ns);
        write_u32(&mut out, OFFSET_ACTION, self.action);
        write_i32(&mut out, OFFSET_STATUS, self.status);
        write_u64(&mut out, OFFSET_CELL, self.cell);
        write_u64(&mut out, OFFSET_SUBJECT, self.subject);
        write_u64(&mut out, OFFSET_AUX, self.aux);
        write_u64(&mut out, OFFSET_VALUE, self.value);
        write_u64(&mut out, OFFSET_BLOCKERS, self.blockers);
        write_u32(&mut out, OFFSET_FLAGS, self.flags);
        out[OFFSET_ROLLBACK_AUTHORITY_ID..OFFSET_MODULE_DIGEST]
            .copy_from_slice(&self.rollback_authority_id);
        out[OFFSET_MODULE_DIGEST..OFFSET_SIGNER_KEY_ID].copy_from_slice(&self.module_digest);
        out[OFFSET_SIGNER_KEY_ID..OFFSET_PREVIOUS_HASH].copy_from_slice(&self.signer_key_id);
        out[OFFSET_PREVIOUS_HASH..OFFSET_RECORD_HASH].copy_from_slice(&self.previous_hash);
        out[OFFSET_RECORD_HASH..ELM_JOURNAL_RECORD_SIZE].copy_from_slice(&hash);
        out
    }

    fn decode(bytes: &[u8; ELM_JOURNAL_RECORD_SIZE]) -> Result<Self, JournalError> {
        if read_u32(bytes, OFFSET_MAGIC) != ELM_JOURNAL_MAGIC
            || read_u16(bytes, OFFSET_VERSION) != ELM_JOURNAL_ABI_VERSION
            || read_u16(bytes, OFFSET_SIZE) as usize != ELM_JOURNAL_RECORD_SIZE
            || bytes[76..80].iter().any(|byte| *byte != 0)
            || read_u32(bytes, OFFSET_FLAGS) & !ELM_JOURNAL_FLAGS_MASK != 0
        {
            return Err(JournalError::Malformed);
        }
        let mut rollback_authority_id = [0u8; 32];
        rollback_authority_id
            .copy_from_slice(&bytes[OFFSET_ROLLBACK_AUTHORITY_ID..OFFSET_MODULE_DIGEST]);
        let mut module_digest = [0u8; 32];
        module_digest.copy_from_slice(&bytes[OFFSET_MODULE_DIGEST..OFFSET_SIGNER_KEY_ID]);
        let mut signer_key_id = [0u8; 32];
        signer_key_id.copy_from_slice(&bytes[OFFSET_SIGNER_KEY_ID..OFFSET_PREVIOUS_HASH]);
        let mut previous_hash = [0u8; 32];
        previous_hash.copy_from_slice(&bytes[OFFSET_PREVIOUS_HASH..OFFSET_RECORD_HASH]);
        let mut record_hash = [0u8; 32];
        record_hash.copy_from_slice(&bytes[OFFSET_RECORD_HASH..ELM_JOURNAL_RECORD_SIZE]);
        let record = Self {
            sequence: read_u64(bytes, OFFSET_SEQUENCE),
            timestamp_ns: read_u64(bytes, OFFSET_TIMESTAMP),
            action: read_u32(bytes, OFFSET_ACTION),
            status: read_i32(bytes, OFFSET_STATUS),
            cell: read_u64(bytes, OFFSET_CELL),
            subject: read_u64(bytes, OFFSET_SUBJECT),
            aux: read_u64(bytes, OFFSET_AUX),
            value: read_u64(bytes, OFFSET_VALUE),
            blockers: read_u64(bytes, OFFSET_BLOCKERS),
            flags: read_u32(bytes, OFFSET_FLAGS),
            rollback_authority_id,
            module_digest,
            signer_key_id,
            previous_hash,
            record_hash,
        };
        if record.sequence == 0
            || !record.identity_shape_is_valid()
            || sha256(&record.encode_with_hash([0; 32])) != record.record_hash
        {
            return Err(JournalError::Malformed);
        }
        Ok(record)
    }

    fn identity_shape_is_valid(&self) -> bool {
        if self.flags & ELM_JOURNAL_FLAG_TRUST_ACCEPTANCE != 0 {
            self.flags == ELM_JOURNAL_FLAG_TRUST_ACCEPTANCE
                && self.action == ELM_JOURNAL_ACTION_TRUST_ACCEPTANCE
                && self.status == 0
                && self.blockers == 0
                && self.value != 0
                && self.rollback_authority_id != [0; 32]
                && self.module_digest != [0; 32]
                && self.signer_key_id != [0; 32]
        } else {
            self.rollback_authority_id == [0; 32]
                && self.module_digest == [0; 32]
                && self.signer_key_id == [0; 32]
        }
    }

    fn trust_epoch(&self) -> Option<JournalTrustEpoch> {
        (self.flags == ELM_JOURNAL_FLAG_TRUST_ACCEPTANCE).then_some(JournalTrustEpoch {
            rollback_authority_id: self.rollback_authority_id,
            module_digest: self.module_digest,
            signer_key_id: self.signer_key_id,
            release_epoch: self.value,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct JournalRuntimeInfo {
    pub initialized: bool,
    pub configured: bool,
    pub persistent: bool,
    pub required: bool,
    pub failed: bool,
    pub sequence_exhausted: bool,
    pub last_error: i32,
    pub replayed_records: u64,
    pub trust_epoch_count: u32,
    pub retained_records: u32,
    pub dropped_records: u32,
    pub last_sequence: u64,
    pub backend_bytes_used: u64,
    pub last_hash: [u8; 32],
}

struct JournalRuntime {
    backend: Option<&'static ElmJournalBackendOps>,
    configured: bool,
    required: bool,
    initialized: bool,
    failed: bool,
    sequence_exhausted: bool,
    last_error: i32,
    records: Vec<JournalRecord>,
    trust_epochs: Vec<JournalTrustEpoch>,
    replay_epochs: Vec<JournalTrustEpoch>,
    replayed_records: u64,
    dropped_records: u32,
    next_sequence: u64,
    last_sequence: u64,
    backend_bytes_used: u64,
    last_hash: [u8; 32],
}

impl JournalRuntime {
    const fn new() -> Self {
        Self {
            backend: None,
            configured: false,
            required: false,
            initialized: false,
            failed: false,
            sequence_exhausted: false,
            last_error: 0,
            records: Vec::new(),
            trust_epochs: Vec::new(),
            replay_epochs: Vec::new(),
            replayed_records: 0,
            dropped_records: 0,
            next_sequence: 1,
            last_sequence: 0,
            backend_bytes_used: 0,
            last_hash: [0; 32],
        }
    }

    fn push_record(&mut self, record: JournalRecord) {
        if self.records.len() >= ELM_JOURNAL_RING_LIMIT {
            self.records.remove(0);
            self.dropped_records = self.dropped_records.saturating_add(1);
        }
        self.records.push(record);
    }

    fn reserve_trust_epoch(&mut self, epoch: JournalTrustEpoch) -> Result<(), JournalError> {
        if let Some(current) = self.trust_epochs.iter().find(|current| {
            current.rollback_authority_id == epoch.rollback_authority_id
                && current.module_digest == epoch.module_digest
        }) {
            if epoch.release_epoch < current.release_epoch {
                return Err(JournalError::Rollback);
            }
            return Ok(());
        }
        self.trust_epochs
            .try_reserve(1)
            .map_err(|_| JournalError::Capacity)
    }

    fn merge_trust_epoch(&mut self, epoch: JournalTrustEpoch) {
        merge_trust_epoch(&mut self.trust_epochs, epoch);
    }

    fn reserve_replay_epoch(&mut self, epoch: JournalTrustEpoch) -> Result<(), JournalError> {
        if self.replay_epochs.iter().any(|current| {
            current.rollback_authority_id == epoch.rollback_authority_id
                && current.module_digest == epoch.module_digest
        }) {
            return Ok(());
        }
        self.replay_epochs
            .try_reserve(1)
            .map_err(|_| JournalError::Capacity)
    }

    fn merge_replay_epoch(&mut self, epoch: JournalTrustEpoch) {
        merge_trust_epoch(&mut self.replay_epochs, epoch);
    }

    fn fail(&mut self, error: JournalError) -> JournalError {
        self.failed = true;
        self.last_error = journal_error_code(error);
        self.sequence_exhausted |= error == JournalError::SequenceExhausted;
        error
    }

    fn handle_backend_failure(&mut self, error: JournalError) -> Result<(), JournalError> {
        let error = self.fail(error);
        if self.required {
            Err(error)
        } else {
            // 可选持久后端失败后只保留内存日志，禁止继续向不确定尾部追加。
            self.backend = None;
            Ok(())
        }
    }

    fn set_replay_cursor(
        &mut self,
        next_sequence: u64,
        last_sequence: u64,
        bytes_used: u64,
        last_hash: [u8; 32],
    ) {
        self.next_sequence = next_sequence;
        self.last_sequence = last_sequence;
        self.backend_bytes_used = bytes_used;
        self.last_hash = last_hash;
    }

    fn finish_append(&mut self, record: JournalRecord, next_sequence: u64) {
        if let Some(epoch) = record.trust_epoch() {
            self.merge_trust_epoch(epoch);
        }
        self.next_sequence = next_sequence;
        self.last_sequence = record.sequence;
        self.last_hash = record.record_hash;
        self.push_record(record);
        if next_sequence == 0 {
            let _ = self.fail(JournalError::SequenceExhausted);
        }
    }

    fn register_backend(
        &mut self,
        backend: &'static ElmJournalBackendOps,
        required: bool,
    ) -> Result<(), JournalError> {
        if self.initialized {
            return Err(JournalError::AlreadyInitialized);
        }
        if self.backend.is_some() {
            return Err(JournalError::AlreadyRegistered);
        }
        let capacity = (backend.capacity)();
        if capacity < ELM_JOURNAL_RECORD_SIZE as u64
            || capacity > ELM_JOURNAL_MAX_BACKEND_BYTES
            || capacity % ELM_JOURNAL_RECORD_SIZE as u64 != 0
        {
            return Err(JournalError::InvalidBackend);
        }
        self.backend = Some(backend);
        self.configured = true;
        self.required = required;
        Ok(())
    }

    fn initialize(&mut self) -> Result<(), JournalError> {
        if self.initialized {
            return if self.required && self.failed {
                Err(JournalError::Sealed)
            } else {
                Ok(())
            };
        }
        self.records
            .try_reserve_exact(ELM_JOURNAL_RING_LIMIT)
            .map_err(|_| JournalError::Capacity)?;
        let Some(backend) = self.backend else {
            self.initialized = true;
            return Ok(());
        };
        let capacity = (backend.capacity)();
        let mut offset = 0u64;
        let mut expected_sequence = 1u64;
        let mut expected_previous_hash = [0u8; 32];
        while offset < capacity {
            let mut bytes = [0u8; ELM_JOURNAL_RECORD_SIZE];
            let read = match (backend.read)(offset, &mut bytes) {
                Ok(read) => read,
                Err(status) => {
                    self.set_replay_cursor(
                        expected_sequence,
                        expected_sequence.saturating_sub(1),
                        offset,
                        expected_previous_hash,
                    );
                    self.initialized = true;
                    self.handle_backend_failure(JournalError::Io(status))?;
                    return Ok(());
                }
            };
            if read == 0 {
                break;
            }
            if read != ELM_JOURNAL_RECORD_SIZE {
                self.set_replay_cursor(
                    expected_sequence,
                    expected_sequence.saturating_sub(1),
                    offset,
                    expected_previous_hash,
                );
                self.initialized = true;
                self.handle_backend_failure(JournalError::Malformed)?;
                return Ok(());
            }
            let record = match JournalRecord::decode(&bytes) {
                Ok(record) => record,
                Err(error) => {
                    self.set_replay_cursor(
                        expected_sequence,
                        expected_sequence.saturating_sub(1),
                        offset,
                        expected_previous_hash,
                    );
                    self.initialized = true;
                    self.handle_backend_failure(error)?;
                    return Ok(());
                }
            };
            if record.sequence != expected_sequence
                || record.previous_hash != expected_previous_hash
            {
                self.set_replay_cursor(
                    expected_sequence,
                    expected_sequence.saturating_sub(1),
                    offset,
                    expected_previous_hash,
                );
                self.initialized = true;
                self.handle_backend_failure(JournalError::Malformed)?;
                return Ok(());
            }
            if let Some(epoch) = record.trust_epoch() {
                if let Err(error) = self
                    .reserve_trust_epoch(epoch)
                    .and_then(|_| self.reserve_replay_epoch(epoch))
                {
                    self.set_replay_cursor(
                        expected_sequence,
                        expected_sequence.saturating_sub(1),
                        offset,
                        expected_previous_hash,
                    );
                    self.initialized = true;
                    self.handle_backend_failure(error)?;
                    return Ok(());
                }
                self.merge_trust_epoch(epoch);
                self.merge_replay_epoch(epoch);
            }
            let Some(next_offset) = offset.checked_add(ELM_JOURNAL_RECORD_SIZE as u64) else {
                self.set_replay_cursor(
                    expected_sequence,
                    expected_sequence.saturating_sub(1),
                    offset,
                    expected_previous_hash,
                );
                self.initialized = true;
                self.handle_backend_failure(JournalError::Capacity)?;
                return Ok(());
            };
            expected_previous_hash = record.record_hash;
            let last_sequence = record.sequence;
            self.push_record(record);
            self.replayed_records += 1;
            offset = next_offset;
            let Some(next_sequence) = expected_sequence.checked_add(1) else {
                self.set_replay_cursor(0, last_sequence, offset, expected_previous_hash);
                self.initialized = true;
                self.handle_backend_failure(JournalError::SequenceExhausted)?;
                return Ok(());
            };
            expected_sequence = next_sequence;
        }
        self.set_replay_cursor(
            expected_sequence,
            expected_sequence.saturating_sub(1),
            offset,
            expected_previous_hash,
        );
        self.initialized = true;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn append_at(
        &mut self,
        timestamp_ns: u64,
        action: u32,
        status: i32,
        cell: u64,
        subject: u64,
        aux: u64,
        value: u64,
        blockers: u64,
        flags: u32,
    ) -> Result<u64, JournalError> {
        if flags & !ELM_JOURNAL_FLAG_AUTHORIZATION != 0 {
            return Err(JournalError::Malformed);
        }
        let sequence = self.begin_append()?;
        let next_sequence = sequence.checked_add(1).unwrap_or(0);
        let record = JournalRecord::new(
            sequence,
            timestamp_ns,
            action,
            status,
            cell,
            subject,
            aux,
            value,
            blockers,
            flags,
            self.last_hash,
        );
        self.append_record(record, next_sequence)
    }

    fn append_trust_acceptance_at(
        &mut self,
        timestamp_ns: u64,
        epoch: JournalTrustEpoch,
    ) -> Result<u64, JournalError> {
        self.reserve_trust_epoch(epoch)?;
        let sequence = self.begin_append()?;
        let next_sequence = sequence.checked_add(1).unwrap_or(0);
        let record = JournalRecord::new_with_identity(
            sequence,
            timestamp_ns,
            ELM_JOURNAL_ACTION_TRUST_ACCEPTANCE,
            0,
            0,
            0,
            0,
            epoch.release_epoch,
            0,
            ELM_JOURNAL_FLAG_TRUST_ACCEPTANCE,
            epoch.rollback_authority_id,
            epoch.module_digest,
            epoch.signer_key_id,
            self.last_hash,
        );
        self.append_record(record, next_sequence)
    }

    fn begin_append(&mut self) -> Result<u64, JournalError> {
        if !self.initialized {
            return Err(JournalError::AlreadyInitialized);
        }
        if self.required && self.failed {
            return Err(JournalError::Sealed);
        }
        let sequence = self.next_sequence;
        if sequence == 0 {
            return Err(self.fail(JournalError::SequenceExhausted));
        }
        Ok(sequence)
    }

    fn append_record(
        &mut self,
        record: JournalRecord,
        next_sequence: u64,
    ) -> Result<u64, JournalError> {
        let sequence = record.sequence;
        if let Some(backend) = self.backend {
            let capacity = (backend.capacity)();
            let Some(next_bytes_used) = self
                .backend_bytes_used
                .checked_add(ELM_JOURNAL_RECORD_SIZE as u64)
            else {
                self.handle_backend_failure(JournalError::Capacity)?;
                self.finish_append(record, next_sequence);
                return Ok(sequence);
            };
            if next_bytes_used > capacity {
                self.handle_backend_failure(JournalError::Capacity)?;
                self.finish_append(record, next_sequence);
                return Ok(sequence);
            }
            let bytes = record.encode();
            if let Err(status) = (backend.append)(&bytes) {
                self.handle_backend_failure(JournalError::Io(status))?;
            } else if let Err(status) = (backend.sync)() {
                self.handle_backend_failure(JournalError::Io(status))?;
            } else {
                self.backend_bytes_used = next_bytes_used;
            }
        }
        self.finish_append(record, next_sequence);
        Ok(sequence)
    }

    fn mutation_allowed(&self) -> bool {
        self.initialized && !self.sequence_exhausted && (!self.required || !self.failed)
    }

    fn runtime_info(&self) -> JournalRuntimeInfo {
        JournalRuntimeInfo {
            initialized: self.initialized,
            configured: self.configured,
            persistent: self.backend.is_some(),
            required: self.required,
            failed: self.failed,
            sequence_exhausted: self.sequence_exhausted,
            last_error: self.last_error,
            replayed_records: self.replayed_records,
            trust_epoch_count: self.trust_epochs.len() as u32,
            retained_records: self.records.len() as u32,
            dropped_records: self.dropped_records,
            last_sequence: self.last_sequence,
            backend_bytes_used: self.backend_bytes_used,
            last_hash: self.last_hash,
        }
    }
}

fn merge_trust_epoch(epochs: &mut Vec<JournalTrustEpoch>, epoch: JournalTrustEpoch) {
    if let Some(current) = epochs.iter_mut().find(|current| {
        current.rollback_authority_id == epoch.rollback_authority_id
            && current.module_digest == epoch.module_digest
    }) {
        if epoch.release_epoch >= current.release_epoch {
            *current = epoch;
        }
    } else {
        epochs.push(epoch);
    }
}

static JOURNAL: Spinlock<JournalRuntime> = Spinlock::new(JournalRuntime::new());

pub(crate) fn register_backend(
    backend: &'static ElmJournalBackendOps,
    required: bool,
) -> Result<(), JournalError> {
    JOURNAL.lock().register_backend(backend, required)
}
pub(crate) fn init() -> Result<(), JournalError> {
    JOURNAL.lock().initialize()
}
#[allow(clippy::too_many_arguments)]
pub(crate) fn append(
    action: u32,
    status: i32,
    cell: u64,
    subject: u64,
    aux: u64,
    value: u64,
    blockers: u64,
    flags: u32,
) -> Result<u64, JournalError> {
    JOURNAL.lock().append_at(
        sched::now_ns_public(),
        action,
        status,
        cell,
        subject,
        aux,
        value,
        blockers,
        flags,
    )
}

pub(crate) fn append_trust_acceptance(
    rollback_authority_id: [u8; 32],
    module_digest: [u8; 32],
    signer_key_id: [u8; 32],
    release_epoch: u64,
) -> Result<u64, JournalError> {
    JOURNAL.lock().append_trust_acceptance_at(
        sched::now_ns_public(),
        JournalTrustEpoch {
            rollback_authority_id,
            module_digest,
            signer_key_id,
            release_epoch,
        },
    )
}

pub(crate) fn mutation_allowed() -> bool {
    JOURNAL.lock().mutation_allowed()
}

pub(crate) fn runtime_info() -> JournalRuntimeInfo {
    JOURNAL.lock().runtime_info()
}

pub(crate) fn records() -> Vec<JournalRecord> {
    JOURNAL.lock().records.clone()
}

pub(crate) fn take_replayed_trust_epochs() -> Vec<JournalTrustEpoch> {
    let mut journal = JOURNAL.lock();
    core::mem::take(&mut journal.replay_epochs)
}

pub(crate) fn restore_replayed_trust_epochs(
    epochs: Vec<JournalTrustEpoch>,
) -> Result<(), JournalError> {
    let mut journal = JOURNAL.lock();
    if !journal.replay_epochs.is_empty() {
        return Err(JournalError::AlreadyInitialized);
    }
    journal.replay_epochs = epochs;
    Ok(())
}

#[cfg(feature = "kernel-tests")]
pub(crate) fn test_codec_and_hash_chain() -> bool {
    let first = JournalRecord::new(1, 10, 20, 0, 1, 2, 3, 4, 0, 0, [0; 32]);
    let second = JournalRecord::new(
        2,
        11,
        21,
        -1,
        2,
        3,
        4,
        5,
        6,
        ELM_JOURNAL_FLAG_AUTHORIZATION,
        first.record_hash,
    );
    let trust = JournalRecord::new_with_identity(
        3,
        12,
        ELM_JOURNAL_ACTION_TRUST_ACCEPTANCE,
        0,
        0,
        0,
        0,
        9,
        0,
        ELM_JOURNAL_FLAG_TRUST_ACCEPTANCE,
        [1; 32],
        [2; 32],
        [3; 32],
        second.record_hash,
    );
    let first_bytes = first.encode();
    let second_bytes = second.encode();
    let trust_bytes = trust.encode();
    JournalRecord::decode(&first_bytes) == Ok(first)
        && JournalRecord::decode(&second_bytes) == Ok(second)
        && JournalRecord::decode(&trust_bytes) == Ok(trust)
        && second.previous_hash == first.record_hash
        && trust.previous_hash == second.record_hash
        && trust.trust_epoch()
            == Some(JournalTrustEpoch {
                rollback_authority_id: [1; 32],
                module_digest: [2; 32],
                signer_key_id: [3; 32],
                release_epoch: 9,
            })
}

#[cfg(feature = "kernel-tests")]
pub(crate) fn test_tamper_is_rejected() -> bool {
    let record = JournalRecord::new(1, 10, 20, 0, 1, 2, 3, 4, 0, 0, [0; 32]);
    let mut bytes = record.encode();
    bytes[OFFSET_VALUE] ^= 0x80;
    JournalRecord::decode(&bytes) == Err(JournalError::Malformed)
}

#[cfg(feature = "kernel-tests")]
struct TestBackendState {
    capacity: u64,
    bytes: Vec<u8>,
    read_error: i32,
    append_error: i32,
    sync_error: i32,
}

#[cfg(feature = "kernel-tests")]
impl TestBackendState {
    const fn new() -> Self {
        Self {
            capacity: ELM_JOURNAL_RECORD_SIZE as u64,
            bytes: Vec::new(),
            read_error: 0,
            append_error: 0,
            sync_error: 0,
        }
    }
}

#[cfg(feature = "kernel-tests")]
static TEST_BACKEND_STATE: Spinlock<TestBackendState> = Spinlock::new(TestBackendState::new());

#[cfg(feature = "kernel-tests")]
static TEST_BACKEND: ElmJournalBackendOps = ElmJournalBackendOps {
    capacity: test_backend_capacity,
    read: test_backend_read,
    append: test_backend_append,
    sync: test_backend_sync,
};

#[cfg(feature = "kernel-tests")]
fn test_backend_capacity() -> u64 {
    TEST_BACKEND_STATE.lock().capacity
}

#[cfg(feature = "kernel-tests")]
fn test_backend_read(offset: u64, out: &mut [u8]) -> Result<usize, i32> {
    let state = TEST_BACKEND_STATE.lock();
    if state.read_error != 0 {
        return Err(state.read_error);
    }
    let Ok(offset) = usize::try_from(offset) else {
        return Err(-22);
    };
    if offset >= state.bytes.len() {
        return Ok(0);
    }
    let len = out.len().min(state.bytes.len() - offset);
    out[..len].copy_from_slice(&state.bytes[offset..offset + len]);
    Ok(len)
}

#[cfg(feature = "kernel-tests")]
fn test_backend_append(record: &[u8]) -> Result<(), i32> {
    let mut state = TEST_BACKEND_STATE.lock();
    if state.append_error != 0 {
        return Err(state.append_error);
    }
    let Some(next_len) = state.bytes.len().checked_add(record.len()) else {
        return Err(-12);
    };
    if next_len as u64 > state.capacity || state.bytes.try_reserve(record.len()).is_err() {
        return Err(-12);
    }
    state.bytes.extend_from_slice(record);
    Ok(())
}

#[cfg(feature = "kernel-tests")]
fn test_backend_sync() -> Result<(), i32> {
    let state = TEST_BACKEND_STATE.lock();
    if state.sync_error == 0 {
        Ok(())
    } else {
        Err(state.sync_error)
    }
}

#[cfg(feature = "kernel-tests")]
fn reset_test_backend(capacity_records: u64) {
    let mut state = TEST_BACKEND_STATE.lock();
    state.capacity = capacity_records * ELM_JOURNAL_RECORD_SIZE as u64;
    state.bytes.clear();
    state.read_error = 0;
    state.append_error = 0;
    state.sync_error = 0;
}

#[cfg(feature = "kernel-tests")]
pub(crate) fn test_optional_and_required_backend_failures() -> bool {
    reset_test_backend(2);
    TEST_BACKEND_STATE.lock().append_error = -5;
    let mut optional = JournalRuntime::new();
    if optional.register_backend(&TEST_BACKEND, false).is_err()
        || optional.initialize().is_err()
        || optional.append_at(1, 1, 0, 1, 1, 0, 0, 0, 0) != Ok(1)
    {
        return false;
    }
    let optional_info = optional.runtime_info();
    if !optional_info.failed
        || optional_info.persistent
        || optional_info.last_error != -5
        || optional_info.last_sequence != 1
        || !optional.mutation_allowed()
    {
        return false;
    }

    reset_test_backend(2);
    TEST_BACKEND_STATE.lock().append_error = -5;
    let mut required = JournalRuntime::new();
    required.register_backend(&TEST_BACKEND, true).is_ok()
        && required.initialize().is_ok()
        && required.append_at(1, 1, 0, 1, 1, 0, 0, 0, 0) == Err(JournalError::Io(-5))
        && required.runtime_info().failed
        && required.runtime_info().last_sequence == 0
        && !required.mutation_allowed()
}

#[cfg(feature = "kernel-tests")]
pub(crate) fn test_backend_capacity_and_replay_failures() -> bool {
    reset_test_backend(1);
    let mut capacity = JournalRuntime::new();
    if capacity.register_backend(&TEST_BACKEND, false).is_err()
        || capacity.initialize().is_err()
        || capacity.append_at(1, 1, 0, 1, 1, 0, 0, 0, 0) != Ok(1)
        || capacity.append_at(2, 2, 0, 1, 1, 0, 0, 0, 0) != Ok(2)
    {
        return false;
    }
    let info = capacity.runtime_info();
    if !info.failed
        || info.persistent
        || info.last_error != journal_error_code(JournalError::Capacity)
        || info.backend_bytes_used != ELM_JOURNAL_RECORD_SIZE as u64
        || info.last_sequence != 2
    {
        return false;
    }

    reset_test_backend(2);
    let first = JournalRecord::new(1, 1, 1, 0, 1, 1, 0, 0, 0, 0, [0; 32]);
    let broken = JournalRecord::new(2, 2, 2, 0, 1, 1, 0, 0, 0, 0, [0x5a; 32]);
    {
        let mut state = TEST_BACKEND_STATE.lock();
        state.bytes.extend_from_slice(&first.encode());
        state.bytes.extend_from_slice(&broken.encode());
    }
    let mut replay = JournalRuntime::new();
    replay.register_backend(&TEST_BACKEND, true).is_ok()
        && replay.initialize() == Err(JournalError::Malformed)
        && replay.runtime_info().replayed_records == 1
        && replay.runtime_info().last_sequence == 1
        && replay.runtime_info().backend_bytes_used == ELM_JOURNAL_RECORD_SIZE as u64
        && !replay.mutation_allowed()
}

#[cfg(feature = "kernel-tests")]
pub(crate) fn test_backend_read_and_sequence_exhaustion() -> bool {
    reset_test_backend(1);
    TEST_BACKEND_STATE.lock().read_error = -6;
    let mut optional = JournalRuntime::new();
    if optional.register_backend(&TEST_BACKEND, false).is_err()
        || optional.initialize().is_err()
        || !optional.runtime_info().failed
        || optional.runtime_info().persistent
        || !optional.mutation_allowed()
    {
        return false;
    }

    let mut sequence = JournalRuntime::new();
    if sequence.initialize().is_err() {
        return false;
    }
    sequence.next_sequence = u64::MAX;
    sequence.append_at(1, 1, 0, 1, 1, 0, 0, 0, 0) == Ok(u64::MAX)
        && sequence.runtime_info().sequence_exhausted
        && sequence.runtime_info().last_sequence == u64::MAX
        && !sequence.mutation_allowed()
        && sequence.append_at(2, 2, 0, 1, 1, 0, 0, 0, 0) == Err(JournalError::SequenceExhausted)
}

#[cfg(feature = "kernel-tests")]
pub(crate) fn test_trust_epoch_replay_and_rollback_rejection() -> bool {
    reset_test_backend(3);
    let first = JournalRecord::new_with_identity(
        1,
        1,
        ELM_JOURNAL_ACTION_TRUST_ACCEPTANCE,
        0,
        0,
        0,
        0,
        3,
        0,
        ELM_JOURNAL_FLAG_TRUST_ACCEPTANCE,
        [1; 32],
        [2; 32],
        [3; 32],
        [0; 32],
    );
    let second = JournalRecord::new_with_identity(
        2,
        2,
        ELM_JOURNAL_ACTION_TRUST_ACCEPTANCE,
        0,
        0,
        0,
        0,
        5,
        0,
        ELM_JOURNAL_FLAG_TRUST_ACCEPTANCE,
        [1; 32],
        [2; 32],
        [4; 32],
        first.record_hash,
    );
    {
        let mut state = TEST_BACKEND_STATE.lock();
        state.bytes.extend_from_slice(&first.encode());
        state.bytes.extend_from_slice(&second.encode());
    }
    let mut replay = JournalRuntime::new();
    if replay.register_backend(&TEST_BACKEND, true).is_err()
        || replay.initialize().is_err()
        || replay.trust_epochs.len() != 1
        || replay.replay_epochs.len() != 1
        || replay.trust_epochs[0].release_epoch != 5
        || replay.trust_epochs[0].signer_key_id != [4; 32]
    {
        return false;
    }
    replay.append_trust_acceptance_at(
        3,
        JournalTrustEpoch {
            rollback_authority_id: [1; 32],
            module_digest: [2; 32],
            signer_key_id: [5; 32],
            release_epoch: 4,
        },
    ) == Err(JournalError::Rollback)
}

fn write_u16(out: &mut [u8], offset: usize, value: u16) {
    out[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn write_u32(out: &mut [u8], offset: usize, value: u32) {
    out[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn write_i32(out: &mut [u8], offset: usize, value: i32) {
    out[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn write_u64(out: &mut [u8], offset: usize, value: u64) {
    out[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn read_u16(input: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([input[offset], input[offset + 1]])
}

fn read_u32(input: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        input[offset],
        input[offset + 1],
        input[offset + 2],
        input[offset + 3],
    ])
}

fn read_i32(input: &[u8], offset: usize) -> i32 {
    i32::from_le_bytes([
        input[offset],
        input[offset + 1],
        input[offset + 2],
        input[offset + 3],
    ])
}

fn read_u64(input: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes([
        input[offset],
        input[offset + 1],
        input[offset + 2],
        input[offset + 3],
        input[offset + 4],
        input[offset + 5],
        input[offset + 6],
        input[offset + 7],
    ])
}

const fn journal_error_code(error: JournalError) -> i32 {
    match error {
        JournalError::AlreadyRegistered => -1,
        JournalError::AlreadyInitialized => -2,
        JournalError::InvalidBackend => -3,
        JournalError::Capacity => -4,
        JournalError::Io(status) => status,
        JournalError::Malformed => -5,
        JournalError::SequenceExhausted => -6,
        JournalError::Sealed => -7,
        JournalError::Rollback => -8,
    }
}
