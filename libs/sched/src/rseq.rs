//! Restartable sequences 的调度事件与纯判定逻辑。
//!
//! 本模块不访问用户地址空间，也不解释架构 trap frame。调度器只记录真实发生的
//! 抢占、迁移和信号事件；kernel 层读取用户 ABI 后调用这里决定是否清除临界区
//! 指针或跳转到 abort handler。

pub const RSEQ_CS_FLAG_NO_RESTART_ON_PREEMPT: u32 = 1 << 0;
pub const RSEQ_CS_FLAG_NO_RESTART_ON_SIGNAL: u32 = 1 << 1;
pub const RSEQ_CS_FLAG_NO_RESTART_ON_MIGRATE: u32 = 1 << 2;
pub const RSEQ_CS_NO_RESTART_FLAGS: u32 = RSEQ_CS_FLAG_NO_RESTART_ON_PREEMPT
    | RSEQ_CS_FLAG_NO_RESTART_ON_SIGNAL
    | RSEQ_CS_FLAG_NO_RESTART_ON_MIGRATE;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum RseqEvent {
    Preempt = RSEQ_CS_FLAG_NO_RESTART_ON_PREEMPT as u8,
    Signal = RSEQ_CS_FLAG_NO_RESTART_ON_SIGNAL as u8,
    Migrate = RSEQ_CS_FLAG_NO_RESTART_ON_MIGRATE as u8,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(transparent)]
pub struct RseqEvents(u8);

impl RseqEvents {
    pub const NONE: Self = Self(0);

    pub const fn from_bits(bits: u8) -> Self {
        Self(bits & RSEQ_CS_NO_RESTART_FLAGS as u8)
    }

    pub const fn bits(self) -> u8 {
        self.0
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub const fn contains(self, event: RseqEvent) -> bool {
        self.0 & event as u8 != 0
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RseqCs {
    pub version: u32,
    pub flags: u32,
    pub start_ip: usize,
    pub post_commit_offset: usize,
    pub abort_ip: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RseqResumeAction {
    Keep,
    Clear,
    AbortTo(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RseqError {
    UnsupportedVersion,
    InvalidFlags,
    InvalidAddress,
    AddressOverflow,
    AbortInsideCriticalSection,
    SignatureMismatch,
}

pub fn validate_signature(expected: u32, actual: u32) -> Result<(), RseqError> {
    if expected == actual {
        Ok(())
    } else {
        Err(RseqError::SignatureMismatch)
    }
}

/// 根据返回 PC 和待处理事件决定 rseq 恢复动作。
///
/// `user_limit` 是用户虚拟地址空间的排他上界。签名和用户内存可访问性由 kernel
/// 层在调用前校验。
pub fn decide_resume(
    pc: usize,
    user_limit: usize,
    rseq_flags: u32,
    cs: RseqCs,
    events: RseqEvents,
) -> Result<RseqResumeAction, RseqError> {
    if events.is_empty() {
        return Ok(RseqResumeAction::Keep);
    }
    if cs.version != 0 {
        return Err(RseqError::UnsupportedVersion);
    }
    if (rseq_flags | cs.flags) & !RSEQ_CS_NO_RESTART_FLAGS != 0 {
        return Err(RseqError::InvalidFlags);
    }

    let flags = rseq_flags | cs.flags;
    if flags & RSEQ_CS_FLAG_NO_RESTART_ON_SIGNAL != 0
        && flags & (RSEQ_CS_FLAG_NO_RESTART_ON_PREEMPT | RSEQ_CS_FLAG_NO_RESTART_ON_MIGRATE)
            != (RSEQ_CS_FLAG_NO_RESTART_ON_PREEMPT | RSEQ_CS_FLAG_NO_RESTART_ON_MIGRATE)
    {
        return Err(RseqError::InvalidFlags);
    }

    if cs.start_ip >= user_limit || cs.abort_ip >= user_limit || cs.abort_ip < 4 {
        return Err(RseqError::InvalidAddress);
    }
    let end = cs
        .start_ip
        .checked_add(cs.post_commit_offset)
        .ok_or(RseqError::AddressOverflow)?;
    if end >= user_limit {
        return Err(RseqError::InvalidAddress);
    }
    if cs.abort_ip >= cs.start_ip && cs.abort_ip < end {
        return Err(RseqError::AbortInsideCriticalSection);
    }

    if pc < cs.start_ip || pc >= end {
        return Ok(RseqResumeAction::Clear);
    }
    if u32::from(events.bits()) & !flags == 0 {
        return Ok(RseqResumeAction::Keep);
    }
    Ok(RseqResumeAction::AbortTo(cs.abort_ip))
}
