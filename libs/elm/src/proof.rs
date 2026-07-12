//! ELM 镜像证明与完整性基础算法。
//!
//! 这里保持 `no_std`，避免把内核装载路径绑定到用户态工具链。内容摘要和签名
//! 都属于 EBI 协议，不绑定 EKI 或其他容器。
//!
//! 规范摘要覆盖 EBI 语义、镜像内容和来源 identifier，并使用 SHA-256；可选 Ed25519 proof
//! 把摘要绑定到 trust anchor、release epoch 和来源。trust store 负责 signer 查找、撤销、
//! 最低 epoch、防回滚、容量和持久化策略。
//!
//! [`ElmRustAbiFingerprintV1`] 单独绑定 target arch、pointer width、endian、rustc commit、
//! panic strategy、code model、target features 和 kernel API manifest hash。签名有效但 ABI
//! 指纹不兼容的镜像仍必须拒绝执行。

use alloc::string::String;
use alloc::vec::Vec;

use ed25519_dalek::{Signature, Verifier, VerifyingKey};

use crate::ebi::{ElmEbiImage, ElmEbiLoadStatus, ElmEbiRelocationKind, ElmEbiSegmentKind};

/// `ELM_PROOF_SHA256_LEN` 固定布局使用的字节长度或对齐值；不得用宿主平台的隐式布局替代。
pub const ELM_PROOF_SHA256_LEN: usize = 32;
/// `ELM_PROOF_ED25519_PUBLIC_KEY_LEN` 固定布局使用的字节长度或对齐值；不得用宿主平台的隐式布局替代。
pub const ELM_PROOF_ED25519_PUBLIC_KEY_LEN: usize = 32;
/// `ELM_PROOF_ED25519_SIGNATURE_LEN` 固定布局使用的字节长度或对齐值；不得用宿主平台的隐式布局替代。
pub const ELM_PROOF_ED25519_SIGNATURE_LEN: usize = 64;
/// `ELM_PROOF_SOURCE_IDENTIFIER_LEN` 固定布局使用的字节长度或对齐值；不得用宿主平台的隐式布局替代。
pub const ELM_PROOF_SOURCE_IDENTIFIER_LEN: usize = 128;
/// `ELM_PROOF_ABI_VERSION` 所属结构或协议的版本号；生产者和消费者必须据此执行兼容性检查。
pub const ELM_PROOF_ABI_VERSION: u16 = 1;
/// `ELM_RUST_ABI_FINGERPRINT_VERSION` 所属结构或协议的版本号；生产者和消费者必须据此执行兼容性检查。
pub const ELM_RUST_ABI_FINGERPRINT_VERSION: u16 = 1;

const ELM_EBI_CANONICAL_DOMAIN: &[u8] = b"ELM-EBI-CANONICAL-V1\0";
const ELM_EBI_SIGNATURE_DOMAIN: &[u8] = b"ELM-EBI-SIGNATURE-V1\0";

/// `ELM_RUST_ABI_TARGET_FEATURE_FLOAT` 能力位；协商成功前调用方不得假定对应功能可用。
pub const ELM_RUST_ABI_TARGET_FEATURE_FLOAT: u64 = 1 << 0;
/// `ELM_RUST_ABI_TARGET_FEATURE_VECTOR` 能力位；协商成功前调用方不得假定对应功能可用。
pub const ELM_RUST_ABI_TARGET_FEATURE_VECTOR: u64 = 1 << 1;
/// `ELM_RUST_ABI_TARGET_FEATURE_SIMD` 能力位；协商成功前调用方不得假定对应功能可用。
pub const ELM_RUST_ABI_TARGET_FEATURE_SIMD: u64 = 1 << 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
/// `ElmPanicStrategy` 列举该协议位置允许出现的全部稳定类别；未知数值不得直接转为此枚举。
pub enum ElmPanicStrategy {
    /// `AbortThroughRuntime` 表示 `ElmPanicStrategy` 的执行策略：`abort through runtime`。
    AbortThroughRuntime = 1,
}

impl ElmPanicStrategy {
    /// 校验并把原始协议数值转换为强类型表示；未知值返回空值或错误。
    pub const fn from_raw(raw: u8) -> Option<Self> {
        match raw {
            1 => Some(Self::AbortThroughRuntime),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// 绑定 Rust 工具链、目标、布局、panic 策略和内核 API manifest 的 ABI 指纹。
pub struct ElmRustAbiFingerprintV1 {
    /// `rustc_commit_hash` 保存对应对象的完整性摘要；安全决策必须按声明算法验证完整字节。
    pub rustc_commit_hash: [u8; ELM_PROOF_SHA256_LEN],
    /// `target_spec_hash` 保存对应对象的完整性摘要；安全决策必须按声明算法验证完整字节。
    pub target_spec_hash: [u8; ELM_PROOF_SHA256_LEN],
    /// `kernel_api_hash` 保存对应对象的完整性摘要；安全决策必须按声明算法验证完整字节。
    pub kernel_api_hash: [u8; ELM_PROOF_SHA256_LEN],
    /// `elmapi_version` 是该对象、ABI 或契约的版本值，用于装载和协商兼容性。
    pub elmapi_version: u16,
    /// 镜像声明的 panic 处理策略，必须与运行时 ABI 要求匹配。
    pub panic_strategy: ElmPanicStrategy,
    /// 编译镜像采用的目标代码模型。
    pub code_model: u8,
    /// 参与 Rust ABI 指纹的目标 CPU 特性位。
    pub target_features: u64,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
}

impl ElmRustAbiFingerprintV1 {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
    pub const fn new(
        rustc_commit_hash: [u8; ELM_PROOF_SHA256_LEN],
        target_spec_hash: [u8; ELM_PROOF_SHA256_LEN],
        kernel_api_hash: [u8; ELM_PROOF_SHA256_LEN],
        elmapi_version: u16,
        panic_strategy: ElmPanicStrategy,
        code_model: u8,
        target_features: u64,
    ) -> Self {
        Self {
            rustc_commit_hash,
            target_spec_hash,
            kernel_api_hash,
            elmapi_version,
            panic_strategy,
            code_model,
            target_features,
            flags: 0,
        }
    }

    /// 验证当前对象及其关联记录满足全部结构、范围和关系不变量。
    pub fn validate(&self) -> Result<(), ElmEbiLoadStatus> {
        if self.elmapi_version == 0
            || self.code_model == 0
            || self.flags != 0
            || self.rustc_commit_hash == [0; ELM_PROOF_SHA256_LEN]
            || self.target_spec_hash == [0; ELM_PROOF_SHA256_LEN]
            || self.kernel_api_hash == [0; ELM_PROOF_SHA256_LEN]
        {
            return Err(ElmEbiLoadStatus::UnsupportedAbi);
        }
        Ok(())
    }

    fn hash_into(&self, hash: &mut CanonicalHash) {
        hash.bytes(&self.rustc_commit_hash);
        hash.bytes(&self.target_spec_hash);
        hash.bytes(&self.kernel_api_hash);
        hash.u16(self.elmapi_version);
        hash.u8(self.panic_strategy as u8);
        hash.u8(self.code_model);
        hash.u64(self.target_features);
        hash.u32(self.flags);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// EBI 规范摘要、签名算法、signer、release epoch 和来源 identifier 的证明记录。
pub struct ElmEbiProofV1 {
    /// `source_identifier` 是该结构定义的协议属性；其取值范围和生命周期由所属类型约束。
    pub source_identifier: String,
    /// `source_digest` 保存对应对象的完整性摘要；安全决策必须按声明算法验证完整字节。
    pub source_digest: [u8; ELM_PROOF_SHA256_LEN],
    /// `subject_digest` 保存对应对象的完整性摘要；安全决策必须按声明算法验证完整字节。
    pub subject_digest: [u8; ELM_PROOF_SHA256_LEN],
    /// `signer_key_id` 所指对象的稳定运行时标识符。
    pub signer_key_id: [u8; ELM_PROOF_SHA256_LEN],
    /// `signer_public_key` 是该结构定义的协议属性；其取值范围和生命周期由所属类型约束。
    pub signer_public_key: [u8; ELM_PROOF_ED25519_PUBLIC_KEY_LEN],
    /// `release_epoch` 是单调发布或策略纪元，用于拒绝回滚和陈旧更新。
    pub release_epoch: u64,
    /// 该记录的标志位集合；不得设置所属有效掩码之外的位。
    pub flags: u32,
    /// 覆盖规范 EBI 摘要的签名字节。
    pub signature: [u8; ELM_PROOF_ED25519_SIGNATURE_LEN],
}

impl ElmEbiProofV1 {
    /// 执行 `unsigned_message` 定义的模型或协议操作；返回值反映校验后的结果。
    pub fn unsigned_message(&self, fingerprint: &ElmRustAbiFingerprintV1) -> [u8; 32] {
        let mut hash = CanonicalHash::new(ELM_EBI_SIGNATURE_DOMAIN);
        hash.string(&self.source_identifier);
        hash.bytes(&self.source_digest);
        hash.bytes(&self.subject_digest);
        hash.bytes(&self.signer_key_id);
        hash.bytes(&self.signer_public_key);
        hash.u64(self.release_epoch);
        hash.u32(self.flags);
        fingerprint.hash_into(&mut hash);
        hash.finish()
    }

    /// 验证当前对象及其关联记录满足全部结构、范围和关系不变量。
    pub fn validate_shape(&self) -> Result<(), ElmEbiLoadStatus> {
        if self.source_identifier.is_empty()
            || self.source_identifier.len() > ELM_PROOF_SOURCE_IDENTIFIER_LEN
            || self.source_identifier.as_bytes().contains(&0)
            || self.source_digest == [0; ELM_PROOF_SHA256_LEN]
            || self.subject_digest == [0; ELM_PROOF_SHA256_LEN]
            || self.signer_key_id == [0; ELM_PROOF_SHA256_LEN]
            || self.signer_public_key == [0; ELM_PROOF_ED25519_PUBLIC_KEY_LEN]
            || sha256(&self.signer_public_key) != self.signer_key_id
            || self.release_epoch == 0
            || self.flags != 0
            || self.signature == [0; ELM_PROOF_ED25519_SIGNATURE_LEN]
        {
            return Err(ElmEbiLoadStatus::RuntimeRejected);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// trust store 中一个 signer 的公钥、撤销状态和最低允许 release epoch。
pub struct ElmTrustAnchor {
    /// 对象或命名空间的规范 identifier；匹配必须比较完整字节串。
    pub identifier: String,
    /// `rollback_authority_identifier` 是该结构定义的协议属性；其取值范围和生命周期由所属类型约束。
    pub rollback_authority_identifier: String,
    /// 签名验证使用的 Ed25519 公钥字节。
    pub public_key: [u8; ELM_PROOF_ED25519_PUBLIC_KEY_LEN],
    /// `key_id` 所指对象的稳定运行时标识符。
    pub key_id: [u8; ELM_PROOF_SHA256_LEN],
}

impl ElmTrustAnchor {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
    pub fn new(
        identifier: impl Into<String>,
        public_key: [u8; ELM_PROOF_ED25519_PUBLIC_KEY_LEN],
    ) -> Result<Self, ElmEbiLoadStatus> {
        let identifier = identifier.into();
        Self::new_with_rollback_authority(identifier.clone(), identifier, public_key)
    }

    /// 执行 `new_with_rollback_authority` 定义的模型或协议操作；返回值反映校验后的结果。
    pub fn new_with_rollback_authority(
        identifier: impl Into<String>,
        rollback_authority_identifier: impl Into<String>,
        public_key: [u8; ELM_PROOF_ED25519_PUBLIC_KEY_LEN],
    ) -> Result<Self, ElmEbiLoadStatus> {
        let identifier = identifier.into();
        let rollback_authority_identifier = rollback_authority_identifier.into();
        if !valid_identifier(&identifier)
            || !valid_identifier(&rollback_authority_identifier)
            || VerifyingKey::from_bytes(&public_key).is_err()
        {
            return Err(ElmEbiLoadStatus::RuntimeRejected);
        }
        Ok(Self {
            identifier,
            rollback_authority_identifier,
            public_key,
            key_id: sha256(&public_key),
        })
    }

    /// 执行 `rollback_authority_id` 定义的模型或协议操作；返回值反映校验后的结果。
    pub fn rollback_authority_id(&self) -> [u8; ELM_PROOF_SHA256_LEN] {
        sha256(self.rollback_authority_identifier.as_bytes())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// `ElmTrustError` 列举该协议位置允许出现的全部稳定类别；未知数值不得直接转为此枚举。
pub enum ElmTrustError {
    /// `Sealed` 表示 `ElmTrustError` 的错误：`sealed`。
    Sealed,
    /// `Duplicate` 表示 `ElmTrustError` 的错误：`duplicate`。
    Duplicate,
    /// `Capacity` 表示 `ElmTrustError` 的错误：`capacity`。
    Capacity,
    /// `ReservationMissing` 表示 `ElmTrustError` 的错误：`reservation missing`。
    ReservationMissing,
    /// `InvalidAnchor` 表示 `ElmTrustError` 的错误：`invalid anchor`。
    InvalidAnchor,
    /// `UnknownSigner` 表示 `ElmTrustError` 的错误：`unknown signer`。
    UnknownSigner,
    /// `Revoked` 表示 `ElmTrustError` 的错误：`revoked`。
    Revoked,
    /// `Rollback` 表示 `ElmTrustError` 的错误：`rollback`。
    Rollback,
    /// `InvalidProof` 表示 `ElmTrustError` 的错误：`invalid proof`。
    InvalidProof,
    /// `DigestMismatch` 表示 `ElmTrustError` 的错误：`digest mismatch`。
    DigestMismatch,
    /// `SignatureMismatch` 表示 `ElmTrustError` 的错误：`signature mismatch`。
    SignatureMismatch,
    /// `Persistence` 表示 `ElmTrustError` 的错误：`persistence`。
    Persistence,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// 镜像证明通过后返回的 signer 身份、epoch、摘要和策略接受结果。
pub struct ElmTrustAcceptance {
    signer_key_id: [u8; ELM_PROOF_SHA256_LEN],
    rollback_authority_id: [u8; ELM_PROOF_SHA256_LEN],
    module_digest: [u8; ELM_PROOF_SHA256_LEN],
    release_epoch: u64,
}

impl ElmTrustAcceptance {
    /// 执行 `signer_key_id` 定义的模型或协议操作；返回值反映校验后的结果。
    pub const fn signer_key_id(&self) -> [u8; ELM_PROOF_SHA256_LEN] {
        self.signer_key_id
    }

    /// 执行 `rollback_authority_id` 定义的模型或协议操作；返回值反映校验后的结果。
    pub const fn rollback_authority_id(&self) -> [u8; ELM_PROOF_SHA256_LEN] {
        self.rollback_authority_id
    }

    /// 执行 `module_digest` 定义的模型或协议操作；返回值反映校验后的结果。
    pub const fn module_digest(&self) -> [u8; ELM_PROOF_SHA256_LEN] {
        self.module_digest
    }

    /// 执行 `release_epoch` 定义的模型或协议操作；返回值反映校验后的结果。
    pub const fn release_epoch(&self) -> u64 {
        self.release_epoch
    }

    /// 执行 `from_persisted` 定义的模型或协议操作；返回值反映校验后的结果。
    pub fn from_persisted(
        signer_key_id: [u8; ELM_PROOF_SHA256_LEN],
        rollback_authority_id: [u8; ELM_PROOF_SHA256_LEN],
        module_digest: [u8; ELM_PROOF_SHA256_LEN],
        release_epoch: u64,
    ) -> Result<Self, ElmTrustError> {
        if signer_key_id == [0; ELM_PROOF_SHA256_LEN]
            || rollback_authority_id == [0; ELM_PROOF_SHA256_LEN]
            || module_digest == [0; ELM_PROOF_SHA256_LEN]
            || release_epoch == 0
        {
            return Err(ElmTrustError::InvalidProof);
        }
        Ok(Self {
            signer_key_id,
            rollback_authority_id,
            module_digest,
            release_epoch,
        })
    }
}

#[derive(Debug, Clone, Default)]
/// 管理 trust anchor、撤销、防回滚 reservation 和持久化状态的信任库。
pub struct ElmTrustStore {
    anchors: Vec<ElmTrustAnchor>,
    revoked: Vec<[u8; ELM_PROOF_SHA256_LEN]>,
    accepted_epochs: Vec<ElmTrustAcceptance>,
    pending_acceptance_slots: usize,
    sealed: bool,
}

impl ElmTrustStore {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
    pub const fn new() -> Self {
        Self {
            anchors: Vec::new(),
            revoked: Vec::new(),
            accepted_epochs: Vec::new(),
            pending_acceptance_slots: 0,
            sealed: false,
        }
    }

    /// 注册 `anchor`；成功前不会向其他单元公开该对象。
    pub fn register_anchor(&mut self, anchor: ElmTrustAnchor) -> Result<(), ElmTrustError> {
        if self.sealed {
            return Err(ElmTrustError::Sealed);
        }
        if self
            .anchors
            .iter()
            .any(|item| item.key_id == anchor.key_id || item.identifier == anchor.identifier)
        {
            return Err(ElmTrustError::Duplicate);
        }
        if VerifyingKey::from_bytes(&anchor.public_key).is_err() {
            return Err(ElmTrustError::InvalidAnchor);
        }
        self.anchors
            .try_reserve(1)
            .map_err(|_| ElmTrustError::Capacity)?;
        self.anchors.push(anchor);
        Ok(())
    }

    /// 执行 `seal` 定义的模型或协议操作；返回值反映校验后的结果。
    pub fn seal(&mut self) {
        self.sealed = true;
    }

    /// 执行 `sealed` 定义的模型或协议操作；返回值反映校验后的结果。
    pub const fn sealed(&self) -> bool {
        self.sealed
    }

    /// 执行 `revoke` 定义的模型或协议操作；返回值反映校验后的结果。
    pub fn revoke(&mut self, key_id: [u8; ELM_PROOF_SHA256_LEN]) -> Result<(), ElmTrustError> {
        if !self.anchors.iter().any(|anchor| anchor.key_id == key_id) {
            return Err(ElmTrustError::UnknownSigner);
        }
        if !self.revoked.contains(&key_id) {
            self.revoked
                .try_reserve(1)
                .map_err(|_| ElmTrustError::Capacity)?;
            self.revoked.push(key_id);
        }
        Ok(())
    }

    /// 执行 `verify` 定义的模型或协议操作；返回值反映校验后的结果。
    pub fn verify(
        &self,
        image: &ElmEbiImage,
        proof: &ElmEbiProofV1,
        fingerprint: &ElmRustAbiFingerprintV1,
    ) -> Result<ElmTrustAcceptance, ElmTrustError> {
        proof
            .validate_shape()
            .map_err(|_| ElmTrustError::InvalidProof)?;
        fingerprint
            .validate()
            .map_err(|_| ElmTrustError::InvalidProof)?;
        let actual_subject = canonical_ebi_digest(image);
        if actual_subject != proof.subject_digest {
            return Err(ElmTrustError::DigestMismatch);
        }
        if self.revoked.contains(&proof.signer_key_id) {
            return Err(ElmTrustError::Revoked);
        }
        let anchor = self
            .anchors
            .iter()
            .find(|anchor| anchor.key_id == proof.signer_key_id)
            .ok_or(ElmTrustError::UnknownSigner)?;
        if anchor.public_key != proof.signer_public_key {
            return Err(ElmTrustError::UnknownSigner);
        }
        let key = VerifyingKey::from_bytes(&anchor.public_key)
            .map_err(|_| ElmTrustError::InvalidAnchor)?;
        let signature = Signature::from_bytes(&proof.signature);
        key.verify(&proof.unsigned_message(fingerprint), &signature)
            .map_err(|_| ElmTrustError::SignatureMismatch)?;
        let module_digest = sha256(image.unit.manifest.name.as_str().as_bytes());
        let rollback_authority_id = anchor.rollback_authority_id();
        if self.accepted_epochs.iter().any(|epoch| {
            epoch.rollback_authority_id == rollback_authority_id
                && epoch.module_digest == module_digest
                && proof.release_epoch < epoch.release_epoch
        }) {
            return Err(ElmTrustError::Rollback);
        }
        Ok(ElmTrustAcceptance {
            signer_key_id: proof.signer_key_id,
            rollback_authority_id,
            module_digest,
            release_epoch: proof.release_epoch,
        })
    }

    /// 执行 `reserve_acceptance` 定义的模型或协议操作；返回值反映校验后的结果。
    pub fn reserve_acceptance(
        &mut self,
        acceptance: &ElmTrustAcceptance,
    ) -> Result<bool, ElmTrustError> {
        if self.accepted_epochs.iter().any(|epoch| {
            epoch.rollback_authority_id == acceptance.rollback_authority_id
                && epoch.module_digest == acceptance.module_digest
        }) {
            return Ok(false);
        }
        let pending = self
            .pending_acceptance_slots
            .checked_add(1)
            .ok_or(ElmTrustError::Capacity)?;
        self.accepted_epochs
            .try_reserve(pending)
            .map_err(|_| ElmTrustError::Capacity)?;
        self.pending_acceptance_slots = pending;
        Ok(true)
    }

    /// 执行 `cancel_acceptance_reservation` 定义的模型或协议操作；返回值反映校验后的结果。
    pub fn cancel_acceptance_reservation(&mut self, reserved: bool) -> Result<(), ElmTrustError> {
        if !reserved {
            return Ok(());
        }
        let Some(pending) = self.pending_acceptance_slots.checked_sub(1) else {
            return Err(ElmTrustError::ReservationMissing);
        };
        self.pending_acceptance_slots = pending;
        Ok(())
    }

    /// 执行 `accept_reserved` 定义的模型或协议操作；返回值反映校验后的结果。
    pub fn accept_reserved(
        &mut self,
        acceptance: ElmTrustAcceptance,
        reserved: bool,
    ) -> Result<(), ElmTrustError> {
        if reserved {
            let Some(pending) = self.pending_acceptance_slots.checked_sub(1) else {
                return Err(ElmTrustError::ReservationMissing);
            };
            self.pending_acceptance_slots = pending;
        }
        if let Some(epoch) = self.accepted_epochs.iter_mut().find(|epoch| {
            epoch.rollback_authority_id == acceptance.rollback_authority_id
                && epoch.module_digest == acceptance.module_digest
        }) {
            if acceptance.release_epoch >= epoch.release_epoch {
                epoch.signer_key_id = acceptance.signer_key_id;
                epoch.release_epoch = acceptance.release_epoch;
            }
        } else {
            if !reserved {
                self.accepted_epochs
                    .try_reserve(1)
                    .map_err(|_| ElmTrustError::Capacity)?;
            }
            self.accepted_epochs.push(acceptance);
        }
        Ok(())
    }

    /// 执行 `try_accept` 定义的模型或协议操作；返回值反映校验后的结果。
    pub fn try_accept(&mut self, acceptance: ElmTrustAcceptance) -> Result<(), ElmTrustError> {
        let reserved = self.reserve_acceptance(&acceptance)?;
        self.accept_reserved(acceptance, reserved)
    }

    /// 执行 `anchors` 定义的模型或协议操作；返回值反映校验后的结果。
    pub fn anchors(&self) -> &[ElmTrustAnchor] {
        &self.anchors
    }

    /// 执行 `revoked` 定义的模型或协议操作；返回值反映校验后的结果。
    pub fn revoked(&self) -> &[[u8; ELM_PROOF_SHA256_LEN]] {
        &self.revoked
    }

    /// 执行 `accepted_epochs` 定义的模型或协议操作；返回值反映校验后的结果。
    pub fn accepted_epochs(&self) -> &[ElmTrustAcceptance] {
        &self.accepted_epochs
    }

    /// 执行 `pending_acceptance_slots` 定义的模型或协议操作；返回值反映校验后的结果。
    pub const fn pending_acceptance_slots(&self) -> usize {
        self.pending_acceptance_slots
    }
}

fn valid_identifier(identifier: &str) -> bool {
    !identifier.is_empty()
        && identifier.len() <= ELM_PROOF_SOURCE_IDENTIFIER_LEN
        && !identifier.as_bytes().contains(&0)
}

const SHA256_INITIAL: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

const SHA256_K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

#[derive(Clone)]
/// 无堆增量 SHA-256 计算器，用于规范 EBI 摘要和内容完整性验证。
pub struct Sha256 {
    state: [u32; 8],
    buffer: [u8; 64],
    buffer_len: usize,
    total_len: u64,
}

impl Sha256 {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
    pub const fn new() -> Self {
        Self {
            state: SHA256_INITIAL,
            buffer: [0; 64],
            buffer_len: 0,
            total_len: 0,
        }
    }

    /// 执行 `update` 定义的模型或协议操作；返回值反映校验后的结果。
    pub fn update(&mut self, mut input: &[u8]) {
        self.total_len = self.total_len.wrapping_add(input.len() as u64);
        if self.buffer_len != 0 {
            let take = core::cmp::min(64 - self.buffer_len, input.len());
            self.buffer[self.buffer_len..self.buffer_len + take].copy_from_slice(&input[..take]);
            self.buffer_len += take;
            input = &input[take..];
            if self.buffer_len == 64 {
                compress(&mut self.state, &self.buffer);
                self.buffer_len = 0;
            }
        }

        while input.len() >= 64 {
            let mut block = [0u8; 64];
            block.copy_from_slice(&input[..64]);
            compress(&mut self.state, &block);
            input = &input[64..];
        }

        if !input.is_empty() {
            self.buffer[..input.len()].copy_from_slice(input);
            self.buffer_len = input.len();
        }
    }

    /// 执行 `finish` 定义的模型或协议操作；返回值反映校验后的结果。
    pub fn finish(mut self) -> [u8; ELM_PROOF_SHA256_LEN] {
        let bit_len = self.total_len.wrapping_mul(8);
        self.buffer[self.buffer_len] = 0x80;
        self.buffer_len += 1;
        if self.buffer_len > 56 {
            for byte in &mut self.buffer[self.buffer_len..] {
                *byte = 0;
            }
            compress(&mut self.state, &self.buffer);
            self.buffer_len = 0;
        }
        for byte in &mut self.buffer[self.buffer_len..56] {
            *byte = 0;
        }
        self.buffer[56..64].copy_from_slice(&bit_len.to_be_bytes());
        compress(&mut self.state, &self.buffer);

        let mut out = [0u8; ELM_PROOF_SHA256_LEN];
        for (index, word) in self.state.iter().enumerate() {
            out[index * 4..index * 4 + 4].copy_from_slice(&word.to_be_bytes());
        }
        out
    }
}

impl Default for Sha256 {
    fn default() -> Self {
        Self::new()
    }
}

/// 执行 `sha256` 定义的模型或协议操作；返回值反映校验后的结果。
pub fn sha256(input: &[u8]) -> [u8; ELM_PROOF_SHA256_LEN] {
    let mut state = Sha256::new();
    state.update(input);
    state.finish()
}

/// 执行 `sha256_with_zeroed_range` 定义的模型或协议操作；返回值反映校验后的结果。
pub fn sha256_with_zeroed_range(
    bytes: &[u8],
    zero_offset: usize,
    zero_len: usize,
) -> Option<[u8; ELM_PROOF_SHA256_LEN]> {
    let zero_end = zero_offset.checked_add(zero_len)?;
    if zero_end > bytes.len() {
        return None;
    }
    let mut state = Sha256::new();
    state.update(&bytes[..zero_offset]);
    let zeros = [0u8; 64];
    let mut remaining = zero_len;
    while remaining != 0 {
        let take = core::cmp::min(remaining, zeros.len());
        state.update(&zeros[..take]);
        remaining -= take;
    }
    state.update(&bytes[zero_end..]);
    Some(state.finish())
}

/// 执行 `sha256_with_zeroed_ranges` 定义的模型或协议操作；返回值反映校验后的结果。
pub fn sha256_with_zeroed_ranges(
    bytes: &[u8],
    ranges: &[(usize, usize)],
) -> Option<[u8; ELM_PROOF_SHA256_LEN]> {
    let mut state = Sha256::new();
    let zeros = [0u8; 64];
    let mut cursor = 0usize;
    for &(offset, len) in ranges {
        let end = offset.checked_add(len)?;
        if offset < cursor || end > bytes.len() {
            return None;
        }
        state.update(&bytes[cursor..offset]);
        let mut remaining = len;
        while remaining != 0 {
            let take = core::cmp::min(remaining, zeros.len());
            state.update(&zeros[..take]);
            remaining -= take;
        }
        cursor = end;
    }
    state.update(&bytes[cursor..]);
    Some(state.finish())
}

struct CanonicalHash {
    state: Sha256,
}

impl CanonicalHash {
    fn new(domain: &[u8]) -> Self {
        let mut state = Sha256::new();
        state.update(domain);
        Self { state }
    }

    fn u8(&mut self, value: u8) {
        self.state.update(&[value]);
    }

    fn u16(&mut self, value: u16) {
        self.state.update(&value.to_le_bytes());
    }

    fn u32(&mut self, value: u32) {
        self.state.update(&value.to_le_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.state.update(&value.to_le_bytes());
    }

    fn i64(&mut self, value: i64) {
        self.state.update(&value.to_le_bytes());
    }

    fn bytes(&mut self, value: &[u8]) {
        self.u64(value.len() as u64);
        self.state.update(value);
    }

    fn string(&mut self, value: &str) {
        self.bytes(value.as_bytes());
    }

    fn finish(self) -> [u8; ELM_PROOF_SHA256_LEN] {
        self.state.finish()
    }
}

/// 执行 `canonical_ebi_digest` 定义的模型或协议操作；返回值反映校验后的结果。
pub fn canonical_ebi_digest(image: &ElmEbiImage) -> [u8; ELM_PROOF_SHA256_LEN] {
    let mut hash = CanonicalHash::new(ELM_EBI_CANONICAL_DOMAIN);
    let unit = &image.unit;
    hash.string(unit.manifest.name.as_str());
    hash.string(unit.manifest.version.as_str());
    hash.u32(unit.manifest.kind.as_raw());
    hash.u64(unit.manifest.intents.len() as u64);
    for intent in &unit.manifest.intents {
        let kind = match intent.kind {
            crate::nexus::IntentKind::Consume => 1,
            crate::nexus::IntentKind::Offer => 2,
            crate::nexus::IntentKind::Extend => 3,
            crate::nexus::IntentKind::Observe => 4,
            crate::nexus::IntentKind::Control => 5,
        };
        hash.u32(kind);
        hash.string(intent.contract.as_str());
    }
    hash.u64(unit.manifest.offers.len() as u64);
    for offer in &unit.manifest.offers {
        hash.string(offer.contract.as_str());
        hash.u32(offer.mode as u32);
    }
    hash.u32(unit.target.arch as u32);
    hash.u16(unit.target.abi_version);
    hash.u16(unit.target.min_core_version);

    match &unit.menu {
        Some(menu) => {
            hash.u8(1);
            hash.u32(menu.kind as u32);
            hash.u32(menu.flags);
            hash.string(&menu.label);
            hash.string(&menu.description);
            hash.string(&menu.route);
        }
        None => hash.u8(0),
    }

    hash.u64(unit.segments.len() as u64);
    for segment in &unit.segments {
        hash.u32(segment_kind_raw(segment.kind));
        hash.u64(segment.size);
        hash.u32(segment.flags);
        hash.u64(segment.file_size);
        hash.u64(segment.mem_size);
        hash.u64(segment.align);
        hash.u64(segment.content_hash);
    }

    match &unit.entry {
        Some(entry) => {
            hash.u8(1);
            hash.string(&entry.symbol);
        }
        None => hash.u8(0),
    }

    hash.u64(unit.dependencies.len() as u64);
    for dependency in &unit.dependencies {
        hash.string(&dependency.provider_name);
        hash.string(dependency.contract.as_str());
    }
    hash.u64(unit.extension_points.len() as u64);
    for point in &unit.extension_points {
        hash.string(&point.point);
        hash.string(point.contract.as_str());
    }
    hash.u64(unit.extensions.len() as u64);
    for extension in &unit.extensions {
        hash.string(&extension.target_name);
        hash.string(&extension.point);
        hash.string(extension.contract.as_str());
    }
    hash.u64(unit.provider_ports.len() as u64);
    for provider in &unit.provider_ports {
        hash.string(provider.contract.as_str());
        hash.u32(provider.access as u32);
        hash.u32(provider.direction as u32);
        hash.u32(provider.mode as u32);
        hash.u32(provider.flags);
        hash_optional_string(&mut hash, provider.handler_symbol.as_deref());
        hash_optional_string(&mut hash, provider.snapshot_symbol.as_deref());
    }
    hash.u64(unit.imports.len() as u64);
    for import in &unit.imports {
        hash.string(&import.name);
        hash.string(import.contract.as_str());
        hash.u32(import.min_version);
        hash.u32(import.max_version);
        hash.u32(import.flags);
    }
    hash.u64(unit.exports.len() as u64);
    for export in &unit.exports {
        hash.string(&export.name);
        hash.string(export.contract.as_str());
        hash.u32(export.version);
        hash.u32(export.flags);
    }

    match &unit.lifecycle_hooks {
        Some(hooks) => {
            hash.u8(1);
            hash_lifecycle_hook(&mut hash, &hooks.initialize);
            hash_lifecycle_hook(&mut hash, &hooks.finalize);
            hash_optional_lifecycle_hook(&mut hash, hooks.migrate_export.as_ref());
            hash_optional_lifecycle_hook(&mut hash, hooks.migrate_import.as_ref());
            hash_optional_lifecycle_hook(&mut hash, hooks.migrate_abort.as_ref());
        }
        None => hash.u8(0),
    }
    match &unit.api_compatibility {
        Some(api) => {
            hash.u8(1);
            hash.u32(api.root_import_index);
            hash.u64(api.required_features);
            hash.u64(api.compatible_versions.len() as u64);
            for version in &api.compatible_versions {
                hash.u16(*version);
            }
        }
        None => hash.u8(0),
    }

    hash.u64(image.payloads.len() as u64);
    for payload in &image.payloads {
        hash.u32(payload.segment_index);
        hash.u32(segment_kind_raw(payload.kind));
        hash.u64(payload.file_size);
        hash.u64(payload.mem_size);
        hash.bytes(&payload.bytes);
    }
    hash.u64(image.symbol_locations.len() as u64);
    for symbol in &image.symbol_locations {
        hash.string(&symbol.name);
        hash.u32(symbol.segment_index);
        hash.u64(symbol.offset);
        hash.u64(symbol.size);
        hash.u32(symbol.flags);
    }
    hash.u64(image.relocations.len() as u64);
    for relocation in &image.relocations {
        hash.u32(relocation_kind_raw(relocation.kind));
        hash.u32(relocation.flags);
        hash.u32(relocation.target_segment_index);
        hash.u64(relocation.target_offset);
        hash.u32(relocation.value_index);
        hash.i64(relocation.addend);
    }
    if let Some(fingerprint) = &image.abi_fingerprint {
        hash.u8(1);
        fingerprint.hash_into(&mut hash);
    } else {
        hash.u8(0);
    }
    hash.finish()
}

fn hash_optional_string(hash: &mut CanonicalHash, value: Option<&str>) {
    match value {
        Some(value) => {
            hash.u8(1);
            hash.string(value);
        }
        None => hash.u8(0),
    }
}

fn hash_lifecycle_hook(hash: &mut CanonicalHash, hook: &crate::ebi::ElmEbiLifecycleHookDecl) {
    hash.u32(hook.kind as u32);
    hash.string(&hook.symbol);
    hash.u16(hook.rust_abi_version);
    hash.u16(hook.signature as u16);
    hash.u32(hook.flags);
}

fn hash_optional_lifecycle_hook(
    hash: &mut CanonicalHash,
    hook: Option<&crate::ebi::ElmEbiLifecycleHookDecl>,
) {
    match hook {
        Some(hook) => {
            hash.u8(1);
            hash_lifecycle_hook(hash, hook);
        }
        None => hash.u8(0),
    }
}

const fn segment_kind_raw(kind: ElmEbiSegmentKind) -> u32 {
    match kind {
        ElmEbiSegmentKind::Code => 1,
        ElmEbiSegmentKind::ReadOnlyData => 2,
        ElmEbiSegmentKind::Data => 3,
        ElmEbiSegmentKind::Bss => 4,
        ElmEbiSegmentKind::Relocation => 5,
        ElmEbiSegmentKind::Note => 6,
    }
}

const fn relocation_kind_raw(kind: ElmEbiRelocationKind) -> u32 {
    match kind {
        ElmEbiRelocationKind::ImageBase64 => 1,
        ElmEbiRelocationKind::SegmentBase64 => 2,
        ElmEbiRelocationKind::SymbolAbs64 => 3,
        ElmEbiRelocationKind::SymbolRel32 => 4,
        ElmEbiRelocationKind::SymbolRel64 => 5,
        ElmEbiRelocationKind::ImportAbs64 => 6,
        ElmEbiRelocationKind::ImportRel32 => 7,
        ElmEbiRelocationKind::ImportRel64 => 8,
    }
}

fn compress(state: &mut [u32; 8], block: &[u8; 64]) {
    let mut w = [0u32; 64];
    for (index, chunk) in block.chunks_exact(4).enumerate() {
        w[index] = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
    }
    for index in 16..64 {
        let s0 =
            w[index - 15].rotate_right(7) ^ w[index - 15].rotate_right(18) ^ (w[index - 15] >> 3);
        let s1 =
            w[index - 2].rotate_right(17) ^ w[index - 2].rotate_right(19) ^ (w[index - 2] >> 10);
        w[index] = w[index - 16]
            .wrapping_add(s0)
            .wrapping_add(w[index - 7])
            .wrapping_add(s1);
    }

    let mut a = state[0];
    let mut b = state[1];
    let mut c = state[2];
    let mut d = state[3];
    let mut e = state[4];
    let mut f = state[5];
    let mut g = state[6];
    let mut h = state[7];

    for index in 0..64 {
        let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let ch = (e & f) ^ ((!e) & g);
        let temp1 = h
            .wrapping_add(s1)
            .wrapping_add(ch)
            .wrapping_add(SHA256_K[index])
            .wrapping_add(w[index]);
        let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let temp2 = s0.wrapping_add(maj);

        h = g;
        g = f;
        f = e;
        e = d.wrapping_add(temp1);
        d = c;
        c = b;
        b = a;
        a = temp1.wrapping_add(temp2);
    }

    state[0] = state[0].wrapping_add(a);
    state[1] = state[1].wrapping_add(b);
    state[2] = state[2].wrapping_add(c);
    state[3] = state[3].wrapping_add(d);
    state[4] = state[4].wrapping_add(e);
    state[5] = state[5].wrapping_add(f);
    state[6] = state[6].wrapping_add(g);
    state[7] = state[7].wrapping_add(h);
}
