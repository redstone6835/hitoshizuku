/// Trap 类型：区分异常/中断/系统调用等
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrapType {
    Exception(Exception),
    Interrupt(Interrupt),
    Syscall,
}

/// 异常类型详细分类
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exception {
    // Common trap categories
    InstructionMisaligned,
    InstructionAccessFault,
    IllegalInstruction,
    Breakpoint,
    LoadMisaligned,
    LoadAccessFault,
    StoreMisaligned,
    StoreAccessFault,
    EnvironmentCall,
    InstructionPageFault,
    LoadPageFault,
    StorePageFault,
    AddressError,
    AddressAlignmentError,
    // Architecture-specific categories normalized for upper layers
    PageModified,
    PageNoRead,
    PageNoExecute,
    PagePrivilegeIllegal,
    BoundsCheck,
    InstructionPrivilege,
    FloatingPointDisabled,
    VectorExtDisabled,
    AdvancedVectorExtDisabled,
    FloatingPointException,
    Other(usize),
}

/// 中断类型详细分类
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interrupt {
    // Common interrupt categories
    UserSoftware,
    SupervisorSoftware,
    MachineMode,
    UserTimer,
    SupervisorTimer,
    MachineTimer,
    UserExternal,
    SupervisorExternal,
    MachineExternal,
    // Architecture-specific categories normalized for upper layers
    Timer,
    Ipi,
    Hardware(usize),
    Other(usize),
}

/// Trap 原因码（架构相关，统一包装）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrapCause {
    pub code: usize,
    pub arch_id: usize,
    pub is_interrupt: bool,
}

/// 架构解码后的规范化 trap 信息。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodedTrap {
    pub code: usize,
    pub is_interrupt: bool,
    pub trap_type: TrapType,
}

/// Trap 上下文（寄存器等）
#[derive(Debug, Clone, Copy)]
pub struct TrapContext {
    pub pc: usize,
    pub sp: usize,
    pub from_user: bool,
    pub cpu_id: usize,
    pub cause: TrapCause,
    pub bad_addr: usize,
    pub trap_type: TrapType,
    pub syscall_id: usize,
    pub syscall_args: [usize; 6],
    pub return_value: usize,
}

/// Trap 分发后的动作决策。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrapAction {
    Resume,
    Yield,
    KillCurrent(i32),
    Halt,
    Panic,
}
