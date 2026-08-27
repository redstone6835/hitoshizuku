//! classic BPF（cBPF）解释器与校验器。
//!
//! 实现 Linux net/core/filter.c 的 sk_run_filter 语义：A/X 寄存器、M[0..15]
//! 内存、BPF_ABS/IND 包字节读取（大端组合）、全部 ALU/JMP/MISC 指令。
//! 用于 SO_ATTACH_FILTER（packet socket 与 INET socket 的接收过滤）。

use alloc::vec::Vec;

/// 单条 cBPF 指令（struct sock_filter 布局：code/jt/jf/k）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CbpfInsn {
    pub code: u16,
    pub jt: u8,
    pub jf: u8,
    pub k: u32,
}

/// 校验并编译后的 cBPF 程序。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CbpfProgram {
    instructions: Vec<CbpfInsn>,
}

/// 校验错误。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CbpfError {
    TooManyInstructions,
    InvalidCode,
    JumpOutOfRange,
    MemoryOutOfRange,
}

const BPF_MAXINSNS: usize = 4096;
const BPF_MEMWORDS: usize = 16;
const BPF_MAX_STEPS: u32 = 100_000;

// 指令类
const BPF_LD: u16 = 0x00;
const BPF_LDX: u16 = 0x01;
const BPF_ST: u16 = 0x02;
const BPF_STX: u16 = 0x03;
const BPF_ALU: u16 = 0x04;
const BPF_JMP: u16 = 0x05;
const BPF_RET: u16 = 0x06;
const BPF_MISC: u16 = 0x07;

// 大小
const BPF_W: u16 = 0x00;
const BPF_H: u16 = 0x08;
const BPF_B: u16 = 0x10;

// 模式
const BPF_IMM: u16 = 0x00;
const BPF_ABS: u16 = 0x20;
const BPF_IND: u16 = 0x40;
const BPF_MEM: u16 = 0x60;
const BPF_LEN: u16 = 0x80;
const BPF_MSH: u16 = 0xa0;

// ALU 操作
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

// JMP 操作
const BPF_JA: u16 = 0x00;
const BPF_JEQ: u16 = 0x10;
const BPF_JGT: u16 = 0x20;
const BPF_JGE: u16 = 0x30;
const BPF_JSET: u16 = 0x40;

// 源操作数
const BPF_K: u16 = 0x00;
const BPF_X: u16 = 0x08;
const BPF_A: u16 = 0x10;

// MISC
const BPF_TAX: u16 = 0x00;
const BPF_TXA: u16 = 0x80;

impl CbpfProgram {
    /// 从裸指令构造并校验。
    pub fn compile(instructions: Vec<CbpfInsn>) -> Result<Self, CbpfError> {
        if instructions.len() > BPF_MAXINSNS {
            return Err(CbpfError::TooManyInstructions);
        }
        for (index, insn) in instructions.iter().enumerate() {
            let class = insn.code & 0x07;
            match class {
                BPF_LD | BPF_LDX => {
                    let mode = insn.code & 0xe0;
                    let size = insn.code & 0x18;
                    let valid = match mode {
                        BPF_IMM => size == BPF_W,
                        BPF_ABS | BPF_IND => size == BPF_W || size == BPF_H || size == BPF_B,
                        BPF_MEM => size == BPF_W && (insn.k as usize) < BPF_MEMWORDS,
                        BPF_LEN => size == BPF_W,
                        BPF_MSH => size == BPF_B && class == BPF_LDX,
                        _ => false,
                    };
                    if !valid {
                        return Err(CbpfError::InvalidCode);
                    }
                }
                BPF_ST | BPF_STX => {
                    if insn.code & 0xe0 != BPF_MEM || (insn.k as usize) >= BPF_MEMWORDS {
                        return Err(CbpfError::MemoryOutOfRange);
                    }
                }
                BPF_ALU => {
                    let op = insn.code & 0xf0;
                    let src = insn.code & 0x08;
                    if !(src == BPF_K || src == BPF_X)
                        || !matches!(
                            op,
                            BPF_ADD
                                | BPF_SUB
                                | BPF_MUL
                                | BPF_DIV
                                | BPF_OR
                                | BPF_AND
                                | BPF_LSH
                                | BPF_RSH
                                | BPF_NEG
                                | BPF_MOD
                                | BPF_XOR
                        )
                    {
                        return Err(CbpfError::InvalidCode);
                    }
                }
                BPF_JMP => {
                    let op = insn.code & 0xf0;
                    let src = insn.code & 0x08;
                    if op == BPF_JA {
                        if src != 0 {
                            return Err(CbpfError::InvalidCode);
                        }
                        let target = (index as u32)
                            .checked_add(1)
                            .and_then(|next| next.checked_add(insn.k));
                        if target.is_none_or(|target| target as usize >= instructions.len()) {
                            return Err(CbpfError::JumpOutOfRange);
                        }
                    } else {
                        if !(src == BPF_K || src == BPF_X) {
                            return Err(CbpfError::InvalidCode);
                        }
                        let jt = (index as u32)
                            .checked_add(1)
                            .and_then(|next| next.checked_add(u32::from(insn.jt)));
                        let jf = (index as u32)
                            .checked_add(1)
                            .and_then(|next| next.checked_add(u32::from(insn.jf)));
                        if jt.is_none_or(|target| target as usize >= instructions.len())
                            || jf.is_none_or(|target| target as usize >= instructions.len())
                        {
                            return Err(CbpfError::JumpOutOfRange);
                        }
                    }
                }
                BPF_RET => {
                    if !(insn.code & 0x18 == BPF_K || insn.code & 0x18 == BPF_A) {
                        return Err(CbpfError::InvalidCode);
                    }
                }
                BPF_MISC => {
                    if insn.code & 0xf8 != BPF_TAX && insn.code & 0xf8 != BPF_TXA {
                        return Err(CbpfError::InvalidCode);
                    }
                }
                _ => return Err(CbpfError::InvalidCode),
            }
        }
        Ok(Self { instructions })
    }

    pub fn instructions(&self) -> &[CbpfInsn] {
        &self.instructions
    }

    /// 在数据包字节流上执行过滤。返回 0 表示丢弃，非 0 为 BPF_RET 值。
    pub fn run(&self, data: &[u8]) -> u32 {
        let mut a: u32 = 0;
        let mut x: u32 = 0;
        let mut mem = [0u32; BPF_MEMWORDS];
        let mut pc: usize = 0;
        let mut steps: u32 = 0;

        while pc < self.instructions.len() {
            steps += 1;
            if steps > BPF_MAX_STEPS {
                // 校验器已保证无环；此处为防御性兜底（丢弃）。
                return 0;
            }
            let insn = self.instructions[pc];
            let class = insn.code & 0x07;
            match class {
                BPF_LD | BPF_LDX => {
                    let mode = insn.code & 0xe0;
                    let size = insn.code & 0x18;
                    let width = match size {
                        BPF_W => 4,
                        BPF_H => 2,
                        _ => 1,
                    };
                    match mode {
                        BPF_IMM => {
                            if class == BPF_LD {
                                a = insn.k;
                            } else {
                                x = insn.k;
                            }
                        }
                        BPF_ABS => {
                            if class == BPF_LD {
                                a = load_bytes(data, insn.k as usize, width);
                            } else {
                                x = load_bytes(data, insn.k as usize, width);
                            }
                        }
                        BPF_IND => {
                            let offset = x.wrapping_add(insn.k) as usize;
                            if class == BPF_LD {
                                a = load_bytes(data, offset, width);
                            } else {
                                x = load_bytes(data, offset, width);
                            }
                        }
                        BPF_MEM => {
                            let value = mem[insn.k as usize];
                            if class == BPF_LD {
                                a = value;
                            } else {
                                x = value;
                            }
                        }
                        BPF_LEN => {
                            if class == BPF_LD {
                                a = data.len() as u32;
                            } else {
                                x = data.len() as u32;
                            }
                        }
                        BPF_MSH => {
                            let byte = data.get(insn.k as usize).copied().unwrap_or(0);
                            x = 4 * u32::from(byte & 0x0f);
                        }
                        _ => return 0,
                    }
                }
                BPF_ST => mem[insn.k as usize] = a,
                BPF_STX => mem[insn.k as usize] = x,
                BPF_ALU => {
                    let op = insn.code & 0xf0;
                    let operand = if insn.code & 0x08 == BPF_X { x } else { insn.k };
                    match op {
                        BPF_ADD => a = a.wrapping_add(operand),
                        BPF_SUB => a = a.wrapping_sub(operand),
                        BPF_MUL => a = a.wrapping_mul(operand),
                        BPF_DIV => a = if operand == 0 { 0 } else { a / operand },
                        BPF_OR => a |= operand,
                        BPF_AND => a &= operand,
                        BPF_LSH => a <<= operand,
                        BPF_RSH => a >>= operand,
                        BPF_NEG => a = a.wrapping_neg(),
                        BPF_MOD => a = if operand == 0 { 0 } else { a % operand },
                        BPF_XOR => a ^= operand,
                        _ => return 0,
                    }
                }
                BPF_JMP => {
                    let op = insn.code & 0xf0;
                    if op == BPF_JA {
                        pc = pc
                            .checked_add(1)
                            .and_then(|next| next.checked_add(insn.k as usize))
                            .unwrap_or(self.instructions.len());
                        continue;
                    }
                    let operand = if insn.code & 0x08 == BPF_X { x } else { insn.k };
                    let taken = match op {
                        BPF_JEQ => a == operand,
                        BPF_JGT => a > operand,
                        BPF_JGE => a >= operand,
                        BPF_JSET => a & operand != 0,
                        _ => return 0,
                    };
                    let offset = if taken {
                        u32::from(insn.jt)
                    } else {
                        u32::from(insn.jf)
                    };
                    pc = pc
                        .checked_add(1)
                        .and_then(|next| next.checked_add(offset as usize))
                        .unwrap_or(self.instructions.len());
                    continue;
                }
                BPF_RET => {
                    return if insn.code & 0x18 == BPF_A { a } else { insn.k };
                }
                BPF_MISC => {
                    if insn.code & 0xf8 == BPF_TAX {
                        x = a;
                    } else {
                        a = x;
                    }
                }
                _ => return 0,
            }
            pc += 1;
        }
        0
    }
}

/// 从字节流偏移读取 width 字节并按大端组合为 u32（Linux sk_load 语义）；
/// 越界时返回 0（Linux skb 越界读 0）。
fn load_bytes(data: &[u8], offset: usize, width: usize) -> u32 {
    if offset > data.len() || width > 4 || data.len() - offset < width {
        return 0;
    }
    let mut value: u32 = 0;
    for &byte in &data[offset..offset + width] {
        value = (value << 8) | u32::from(byte);
    }
    value
}

/// 从用户 sock_filter 数组字节构造指令序列（struct sock_filter 8 字节/条）。
pub fn parse_sock_filters(bytes: &[u8]) -> Result<Vec<CbpfInsn>, ()> {
    if bytes.len() % 8 != 0 {
        return Err(());
    }
    let count = bytes.len() / 8;
    let mut instructions = Vec::with_capacity(count);
    for chunk in bytes.chunks_exact(8) {
        instructions.push(CbpfInsn {
            code: u16::from_ne_bytes(chunk[0..2].try_into().unwrap()),
            jt: chunk[2],
            jf: chunk[3],
            k: u32::from_ne_bytes(chunk[4..8].try_into().unwrap()),
        });
    }
    Ok(instructions)
}

/// 把指令序列序列化为 sock_filter 字节数组。
pub fn serialize_sock_filters(instructions: &[CbpfInsn]) -> Vec<u8> {
    let mut out = Vec::with_capacity(instructions.len() * 8);
    for insn in instructions {
        out.extend_from_slice(&insn.code.to_ne_bytes());
        out.push(insn.jt);
        out.push(insn.jf);
        out.extend_from_slice(&insn.k.to_ne_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn insn(code: u16, jt: u8, jf: u8, k: u32) -> CbpfInsn {
        CbpfInsn { code, jt, jf, k }
    }

    /// 经典过滤器：接受以太网 type 0x0800（IP）的帧（tcpdump 生成格式）。
    fn ip_filter() -> Vec<CbpfInsn> {
        vec![
            insn(0x28, 0, 0, 12),     // BPF_LD|BPF_H|BPF_ABS 12（2 字节大端）
            insn(0x15, 1, 0, 0x0800), // BPF_JMP|BPF_JEQ|BPF_K
            insn(0x06, 0, 0, 0),      // BPF_RET|BPF_K 0
            insn(0x06, 0, 0, 0xffff), // BPF_RET|BPF_K 0xffff
        ]
    }

    #[test]
    fn ip_filter_accepts_ipv4_frames() {
        let program = CbpfProgram::compile(ip_filter()).unwrap();
        let mut ip_frame = [0u8; 64];
        ip_frame[12] = 0x08;
        ip_frame[13] = 0x00;
        assert_eq!(program.run(&ip_frame), 0xffff);
    }

    #[test]
    fn ip_filter_rejects_arp_frames() {
        let program = CbpfProgram::compile(ip_filter()).unwrap();
        let mut arp_frame = [0u8; 64];
        arp_frame[12] = 0x08;
        arp_frame[13] = 0x06;
        assert_eq!(program.run(&arp_frame), 0);
    }

    #[test]
    fn out_of_range_load_returns_zero() {
        let program = CbpfProgram::compile(vec![
            insn(0x20, 0, 0, 100), // 越界 ABS 读
            insn(0x06, 0, 0, 0xffff),
        ])
        .unwrap();
        assert_eq!(program.run(&[1, 2, 3]), 0xffff);
    }

    #[test]
    fn ret_a_returns_accumulator() {
        let program = CbpfProgram::compile(vec![
            insn(0x00, 0, 0, 7), // A = 7
            insn(0x16, 0, 0, 0), // ret A
        ])
        .unwrap();
        assert_eq!(program.run(&[]), 7);
    }

    #[test]
    fn arithmetic_ops() {
        let program = CbpfProgram::compile(vec![
            insn(0x00, 0, 0, 10),   // A = 10
            insn(0x04, 0, 0, 5),    // A += 5 -> 15
            insn(0x14, 0, 0, 3),    // A -= 3 -> 12
            insn(0x24, 0, 0, 2),    // A *= 2 -> 24
            insn(0x34, 0, 0, 4),    // A /= 4 -> 6
            insn(0x54, 0, 0, 0x0f), // A &= 0xf -> 6
            insn(0x44, 0, 0, 0x30), // A |= 0x30 -> 0x36
            insn(0x64, 0, 0, 2),    // A <<= 2 -> 0xd8
            insn(0x74, 0, 0, 3),    // A >>= 3 -> 0x1b
            insn(0xa4, 0, 0, 0xff), // A ^= 0xff -> 0xe4
            insn(0x16, 0, 0, 0),    // ret A
        ])
        .unwrap();
        assert_eq!(program.run(&[]), 0xe4);
    }

    #[test]
    fn jump_ops_select_branch() {
        let program = CbpfProgram::compile(vec![
            insn(0x00, 0, 0, 10),
            insn(0x15, 1, 0, 10), // jeq 10: jt -> ret 1，jf -> ret 2
            insn(0x06, 0, 0, 2),  // ret 2
            insn(0x06, 0, 0, 1),  // ret 1
        ])
        .unwrap();
        assert_eq!(program.run(&[]), 1);
    }

    #[test]
    fn validator_rejects_jump_out_of_range() {
        assert_eq!(
            CbpfProgram::compile(vec![
                insn(0x15, 5, 0, 0), // jt 越界
                insn(0x06, 0, 0, 0),
            ]),
            Err(CbpfError::JumpOutOfRange)
        );
        assert_eq!(
            CbpfProgram::compile(vec![insn(0x05, 0, 0, 10)]), // ja 越界
            Err(CbpfError::JumpOutOfRange)
        );
    }

    #[test]
    fn validator_rejects_bad_memory_index() {
        assert_eq!(
            CbpfProgram::compile(vec![insn(0x02, 0, 0, 16)]), // M[16] 越界
            Err(CbpfError::MemoryOutOfRange)
        );
    }

    #[test]
    fn parse_and_serialize_roundtrip() {
        let instructions = ip_filter();
        let bytes = serialize_sock_filters(&instructions);
        assert_eq!(parse_sock_filters(&bytes).unwrap(), instructions);
    }

    #[test]
    fn filter_runs_on_malformed_program_safely() {
        // 无 RET 的程序：pc 越界后返回 0。
        let program = CbpfProgram::compile(vec![insn(0x00, 0, 0, 1), insn(0x00, 0, 0, 2)]).unwrap();
        assert_eq!(program.run(&[]), 0);
    }

    #[test]
    fn memory_ops_work() {
        let program = CbpfProgram::compile(vec![
            insn(0x62, 0, 0, 3),  // BPF_ST|BPF_MEM: M[3] = A
            insn(0x00, 0, 0, 42), // A = 42
            insn(0x62, 0, 0, 3),  // BPF_ST|BPF_MEM: M[3] = 42
            insn(0x61, 0, 0, 3),  // BPF_LDX|BPF_W|BPF_MEM: X = M[3]
            insn(0x87, 0, 0, 0),  // BPF_MISC|BPF_TXA: A = X
            insn(0x16, 0, 0, 0),  // ret A
        ])
        .unwrap();
        assert_eq!(program.run(&[]), 42);
    }
}
