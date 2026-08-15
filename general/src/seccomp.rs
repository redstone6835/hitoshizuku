//! seccomp 过滤器：classic BPF 解释器、校验器与任务级过滤器状态。
//!
//! 语义对齐 Linux `kernel/seccomp.c`：
//!
//! - `seccomp_data`：`nr/arch/instruction_pointer/args[6]`；
//! - 动作：`RET_KILL_PROCESS`/`KILL_THREAD`（SIGSYS）、`RET_TRAP`、
//!   `RET_ERRNO`、`RET_USER_NOTIF`、`RET_TRACE`、`RET_LOG`、`RET_ALLOW`；
//! - 过滤器按安装顺序组合（先安装者优先，`SECCOMP_RET_ACTION_FULL` 决定
//!   胜负，`ACTION_ALLOW` 不终结链）；
//! - `SECCOMP_FILTER_FLAG_TSYNC`：本实现过滤器状态为进程级共享
//!   （`Arc<SeccompState>`），TSYNC 语义天然成立；
//! - 无 `no_new_privs` 时安装过滤器需要 `CAP_SYS_ADMIN`。

#![allow(clippy::too_many_arguments)]

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use errno::Errno;
use spin::Mutex;
use vfs::cred::Credentials;

/// 过滤器指令（Linux `struct sock_filter`，8 字节）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SockFilter {
    pub code: u16,
    pub jt: u8,
    pub jf: u8,
    pub k: u32,
}

/// `struct seccomp_data` 布局偏移。
pub const SECCOMP_DATA_NR: usize = 0;
pub const SECCOMP_DATA_ARCH: usize = 8;
pub const SECCOMP_DATA_IP: usize = 16;
pub const SECCOMP_DATA_ARGS: usize = 24;
/// `struct seccomp_data` 总大小（Linux：8+4+4+8+6*8=72 字节）。
pub const SECCOMP_DATA_SIZE: usize = 72;

/// `seccomp(2)` 操作。
pub const SECCOMP_SET_MODE_STRICT: u32 = 0;
pub const SECCOMP_SET_MODE_FILTER: u32 = 1;
pub const SECCOMP_GET_ACTION_AVAIL: u32 = 2;
pub const SECCOMP_GET_NOTIF_SIZES: u32 = 3;

/// `SECCOMP_SET_MODE_FILTER` 标志。
pub const SECCOMP_FILTER_FLAG_TSYNC: u32 = 1;
pub const SECCOMP_FILTER_FLAG_LOG: u32 = 2;
pub const SECCOMP_FILTER_FLAG_SPEC_ALLOW: u32 = 4;
pub const SECCOMP_FILTER_FLAG_NEW_LISTENER: u32 = 8;
pub const SECCOMP_FILTER_FLAG_TSYNC_ESRCH: u32 = 16;
pub const SECCOMP_FILTER_FLAG_WATCH_TIMEOUT: u32 = 32;

/// 动作位（Linux `SECCOMP_RET_ACTION_FULL`）。
pub const SECCOMP_RET_KILL_PROCESS: u32 = 0x8000_0000;
pub const SECCOMP_RET_KILL_THREAD: u32 = 0x0000_0000;
pub const SECCOMP_RET_TRAP: u32 = 0x0003_0000;
pub const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;
pub const SECCOMP_RET_USER_NOTIF: u32 = 0x7fc0_0000;
pub const SECCOMP_RET_TRACE: u32 = 0x7ff0_0000;
pub const SECCOMP_RET_LOG: u32 = 0x7ffc_0000;
pub const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
pub const SECCOMP_RET_ACTION_FULL: u32 = 0xffff_0000;
pub const SECCOMP_RET_DATA: u32 = 0x0000_ffff;

/// BPF 指令数上限（Linux `BPF_MAXINSNS`）。
pub const BPF_MAXINSNS: usize = 4096;

/// `SECCOMP_MODE_*`。
pub const SECCOMP_MODE_DISABLED: i32 = 0;
pub const SECCOMP_MODE_STRICT: i32 = 1;
pub const SECCOMP_MODE_FILTER: i32 = 2;

/// 每个进程的过滤器状态（进程级共享；TSYNC 语义天然成立）。
pub struct SeccompState {
    /// 0 = disabled，1 = strict，2 = filter。
    pub mode: AtomicU32,
    /// 按安装顺序排列的过滤器（先安装者优先）。
    pub filters: Mutex<Vec<Arc<SeccompFilter>>>,
    /// 是否允许 `SECCOMP_RET_LOG` 产生日志（`SECCOMP_FILTER_FLAG_LOG`）。
    pub log: AtomicU32,
    /// 新 listener 的 fd 注册回调（kernel 注入，返回 fd 数字）。
    pub listener_factory: Mutex<Option<fn(&SeccompNotification) -> Result<usize, Errno>>>,
}

impl SeccompState {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            mode: AtomicU32::new(SECCOMP_MODE_DISABLED as u32),
            filters: Mutex::new(Vec::new()),
            log: AtomicU32::new(0),
            listener_factory: Mutex::new(None),
        })
    }

    pub fn mode(&self) -> i32 {
        self.mode.load(Ordering::Acquire) as i32
    }

    pub fn filter_count(&self) -> usize {
        self.filters.lock().len()
    }

    /// 安装一个过滤器。
    pub fn push_filter(&self, filter: Arc<SeccompFilter>) {
        self.filters.lock().push(filter);
        self.mode.store(SECCOMP_MODE_FILTER as u32, Ordering::Release);
    }

    pub fn set_strict(&self) {
        self.filters.lock().clear();
        self.mode.store(SECCOMP_MODE_STRICT as u32, Ordering::Release);
    }

    /// 在 syscall 入口运行过滤器链，返回最终动作（含 data）。
    pub fn run(&self, data: &[u8; SECCOMP_DATA_SIZE]) -> u32 {
        let filters = self.filters.lock();
        let mut action = SECCOMP_RET_ALLOW;
        for filter in filters.iter() {
            let result = filter.run(data);
            let result_action = result & SECCOMP_RET_ACTION_FULL;
            // Linux：除 ALLOW 外，动作按过滤器顺序取第一个非 ALLOW 的
            // 决定性结果（KILL/TRAP/ERRNO/NOTIF/TRACE 都是决定性的）。
            if result_action != SECCOMP_RET_ALLOW {
                action = result;
                if result_action != SECCOMP_RET_LOG {
                    break;
                }
                continue;
            }
        }
        action
    }

    /// strict 模式：仅允许 read/write/_exit/sigreturn。
    pub fn strict_allows(nr: usize) -> bool {
        matches!(
            nr,
            63 | 93 | 94 | 139 | 172 | 173 | 174 // read/write/_exit/exit_group/syscall 等
        )
    }
}

/// 单个过滤器。
/// USER_NOTIF 通知（见 seccomp 通知实现）。
pub struct SeccompNotification {
    pub id: u64,
    pub nr: i64,
    pub arch: u32,
    pub args: [u64; 6],
    pub ret: Mutex<Option<(i64, u8)>>,
}

pub struct SeccompFilter {
    pub id: u64,
    pub insns: Vec<SockFilter>,
    pub flags: u32,
}

impl SeccompFilter {
    /// 校验并构造过滤器。无效程序返回 `EINVAL`。
    pub fn new(insns: Vec<SockFilter>, flags: u32) -> Result<Arc<Self>, Errno> {
        if insns.is_empty() || insns.len() > BPF_MAXINSNS {
            return Err(Errno::EINVAL);
        }
        validate_program(&insns)?;
        static NEXT_FILTER_ID: AtomicU64 = AtomicU64::new(1);
        let id = NEXT_FILTER_ID.fetch_add(1, Ordering::Relaxed);
        Ok(Arc::new(Self { id, insns, flags }))
    }

    /// 运行过滤器，返回动作值。
    pub fn run(&self, data: &[u8; SECCOMP_DATA_SIZE]) -> u32 {
        run_bpf(&self.insns, data)
    }
}

/// BPF 指令集（Linux `bpf_common.h`）。
const BPF_LD: u16 = 0x00;
const BPF_LDX: u16 = 0x01;
const BPF_ST: u16 = 0x02;
const BPF_STX: u16 = 0x03;
const BPF_ALU: u16 = 0x04;
const BPF_JMP: u16 = 0x05;
const BPF_RET: u16 = 0x06;
const BPF_MISC: u16 = 0x07;

const BPF_W: u16 = 0x00;
const BPF_H: u16 = 0x08;
const BPF_B: u16 = 0x10;
const BPF_IMM: u16 = 0x00;
const BPF_ABS: u16 = 0x20;
const BPF_IND: u16 = 0x40;
const BPF_MEM: u16 = 0x60;
const BPF_LEN: u16 = 0x80;
const BPF_MSH: u16 = 0xa0;

const BPF_ADD: u16 = 0x00;
const BPF_SUB: u16 = 0x10;
const BPF_MUL: u16 = 0x20;
const BPF_DIV: u16 = 0x30;
const BPF_OR: u16 = 0x40;
const BPF_AND: u16 = 0x50;
const BPF_LSH: u16 = 0x60;
const BPF_RSH: u16 = 0x70;
const BPF_NEG: u16 = 0x80;
const BPF_MOD: u16 = 0x90;
const BPF_XOR: u16 = 0xa0;

const BPF_JA: u16 = 0x00;
const BPF_JEQ: u16 = 0x10;
const BPF_JGT: u16 = 0x20;
const BPF_JGE: u16 = 0x30;
const BPF_JSET: u16 = 0x40;

const BPF_K: u16 = 0x00;
const BPF_X: u16 = 0x08;

const BPF_A: u16 = 0x10;
const BPF_TAX: u16 = 0x00;
const BPF_TXA: u16 = 0x80;

/// 校验器：检查指令合法性、跳转目标、`BPF_LD/BPF_LDX` 的访问边界。
fn validate_program(insns: &[SockFilter]) -> Result<(), Errno> {
    for (index, insn) in insns.iter().enumerate() {
        let class = insn.code & 0x07;
        match class {
            BPF_LD | BPF_LDX => {
                let mode = insn.code & 0xe0;
                let size = insn.code & 0x18;
                match mode {
                    BPF_IMM => {}
                    BPF_MEM => {
                        if insn.k as usize >= 16 {
                            return Err(Errno::EINVAL);
                        }
                    }
                    BPF_ABS | BPF_IND => {
                        // seccomp_data 共 SECCOMP_DATA_SIZE 字节；H/B 模式不能越过末尾。
                        let max = if size == BPF_W {
                            4
                        } else if size == BPF_H {
                            2
                        } else {
                            1
                        };
                        if insn.k as usize > SECCOMP_DATA_SIZE - max {
                            return Err(Errno::EINVAL);
                        }
                    }
                    BPF_LEN => {}
                    BPF_MSH => return Err(Errno::EINVAL),
                    _ => return Err(Errno::EINVAL),
                }
            }
            BPF_ST | BPF_STX => {
                if insn.k as usize >= 16 {
                    return Err(Errno::EINVAL);
                }
            }
            BPF_ALU => {
                let op = insn.code & 0xf0;
                if op == BPF_NEG && insn.code & 0x08 != 0 {
                    return Err(Errno::EINVAL);
                }
            }
            BPF_JMP => {
                let op = insn.code & 0xf0;
                if op == BPF_JA {
                    let target = index as i64 + 1 + insn.k as i64;
                    if target < 0 || target >= insns.len() as i64 {
                        return Err(Errno::EINVAL);
                    }
                } else {
                    let target_true = index as i64 + 1 + insn.jt as i64;
                    let target_false = index as i64 + 1 + insn.jf as i64;
                    if target_true < 0
                        || target_true >= insns.len() as i64
                        || target_false < 0
                        || target_false >= insns.len() as i64
                    {
                        return Err(Errno::EINVAL);
                    }
                }
            }
            BPF_RET => {
                if insn.code & 0x18 != 0 {
                    return Err(Errno::EINVAL);
                }
            }
            BPF_MISC => {
                if !matches!(insn.code & 0xf8, BPF_TAX | BPF_TXA) {
                    return Err(Errno::EINVAL);
                }
            }
            _ => return Err(Errno::EINVAL),
        }
    }
    Ok(())
}

/// classic BPF 解释器（Linux `__bpf_prog_run` 的 seccomp 语义）。
///
/// 寄存器：`A`（累加器）、`X`（索引）、`M[0..15]`（内存）。`data` 是
/// 64 字节的 `struct seccomp_data`。
pub fn run_bpf(insns: &[SockFilter], data: &[u8; SECCOMP_DATA_SIZE]) -> u32 {
    let mut a: u32 = 0;
    let mut x: u32 = 0;
    let mut mem = [0u32; 16];
    let mut pc: usize = 0;

    while pc < insns.len() {
        let insn = &insns[pc];
        let class = insn.code & 0x07;
        let size = insn.code & 0x18;
        let mode = insn.code & 0xe0;
        let op = insn.code & 0xf0;

        match class {
            BPF_LD => match mode {
                BPF_IMM => a = insn.k,
                BPF_MEM => a = mem[insn.k as usize],
                BPF_ABS => a = load_word(data, insn.k, size),
                BPF_IND => a = load_word(data, x.wrapping_add(insn.k), size),
                BPF_LEN => a = 64,
                _ => return SECCOMP_RET_KILL_THREAD,
            },
            BPF_LDX => match mode {
                BPF_IMM => x = insn.k,
                BPF_MEM => x = mem[insn.k as usize],
                BPF_LEN => x = 64,
                BPF_MSH => {
                    x = load_word(data, insn.k, BPF_B).wrapping_mul(4) & 0xffff_fff0;
                }
                _ => return SECCOMP_RET_KILL_THREAD,
            },
            BPF_ST => mem[insn.k as usize] = a,
            BPF_STX => mem[insn.k as usize] = x,
            BPF_ALU => {
                let src = if insn.code & 0x08 != 0 { x } else { insn.k };
                match op {
                    BPF_ADD => a = a.wrapping_add(src),
                    BPF_SUB => a = a.wrapping_sub(src),
                    BPF_MUL => a = a.wrapping_mul(src),
                    BPF_DIV => {
                        if src == 0 {
                            return SECCOMP_RET_KILL_THREAD;
                        }
                        a /= src;
                    }
                    BPF_OR => a |= src,
                    BPF_AND => a &= src,
                    BPF_LSH => a = a.wrapping_shl(src),
                    BPF_RSH => a = a.wrapping_shr(src),
                    BPF_NEG => a = a.wrapping_neg(),
                    BPF_MOD => {
                        if src == 0 {
                            return SECCOMP_RET_KILL_THREAD;
                        }
                        a %= src;
                    }
                    BPF_XOR => a ^= src,
                    _ => return SECCOMP_RET_KILL_THREAD,
                }
            }
            BPF_JMP => {
                let (condition, _offset) = match op {
                    BPF_JA => (true, insn.k as i32),
                    BPF_JEQ => (a == if insn.code & 0x08 != 0 { x } else { insn.k }, insn.jt as i32 - insn.jf as i32),
                    BPF_JGT => (a > if insn.code & 0x08 != 0 { x } else { insn.k }, insn.jt as i32 - insn.jf as i32),
                    BPF_JGE => (a >= if insn.code & 0x08 != 0 { x } else { insn.k }, insn.jt as i32 - insn.jf as i32),
                    BPF_JSET => (a & if insn.code & 0x08 != 0 { x } else { insn.k } != 0, insn.jt as i32 - insn.jf as i32),
                    _ => return SECCOMP_RET_KILL_THREAD,
                };
                // 跳转语义：JA 相对 +k；条件跳转真分支 +jt、假分支 +jf
                // （均相对下一条指令）。
                let offset = if op == BPF_JA {
                    insn.k as i32
                } else if condition {
                    insn.jt as i32
                } else {
                    insn.jf as i32
                };
                let next = pc as i64 + 1 + offset as i64;
                if next < 0 || next >= insns.len() as i64 {
                    return SECCOMP_RET_KILL_THREAD;
                }
                pc = next as usize;
                continue;
            }
            BPF_RET => return insn.k,
            BPF_MISC => match insn.code & 0xf8 {
                BPF_TAX => x = a,
                BPF_TXA => a = x,
                _ => return SECCOMP_RET_KILL_THREAD,
            },
            _ => return SECCOMP_RET_KILL_THREAD,
        }
        pc += 1;
    }
    SECCOMP_RET_KILL_THREAD
}

fn load_word(data: &[u8; SECCOMP_DATA_SIZE], offset: u32, size: u16) -> u32 {
    let offset = offset as usize;
    match size {
        BPF_W => {
            if offset + 4 > SECCOMP_DATA_SIZE {
                return 0;
            }
            u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap())
        }
        BPF_H => {
            if offset + 2 > SECCOMP_DATA_SIZE {
                return 0;
            }
            u16::from_le_bytes(data[offset..offset + 2].try_into().unwrap()) as u32
        }
        BPF_B => {
            if offset + 1 > 64 {
                return 0;
            }
            data[offset] as u32
        }
        _ => 0,
    }
}

/// 从用户缓冲区解析 BPF 程序（8 字节每条）。
pub fn parse_program(bytes: &[u8]) -> Result<Vec<SockFilter>, Errno> {
    if bytes.len() % 8 != 0 {
        return Err(Errno::EINVAL);
    }
    let count = bytes.len() / 8;
    if count > BPF_MAXINSNS {
        return Err(Errno::EINVAL);
    }
    let mut insns = Vec::with_capacity(count);
    for chunk in bytes.chunks_exact(8) {
        insns.push(SockFilter {
            code: u16::from_le_bytes(chunk[0..2].try_into().unwrap()),
            jt: chunk[2],
            jf: chunk[3],
            k: u32::from_le_bytes(chunk[4..8].try_into().unwrap()),
        });
    }
    Ok(insns)
}

/// 检查 `seccomp(2)` 安装权限（无 `no_new_privs` 时需要 `CAP_SYS_ADMIN`）。
pub fn filter_install_allowed(no_new_privs: bool, cred: &Credentials) -> bool {
    no_new_privs || cred.has_cap(vfs::cred::Capability::SysAdmin)
}

/// `SECCOMP_RET_KILL_*`/`TRAP` 对应的 SIGSYS 投递参数。
pub const SECCOMP_SIGSYS: i32 = 31;

/// 供 procfs/status 显示的模式值。
pub fn mode_for_display(mode: i32) -> i32 {
    mode
}
