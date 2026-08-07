//! kernel-tests 专用的直接 SOYO 映像。

use alloc::vec::Vec;

use native_abi::TargetArch;
use soyo::test_support::{SoyoLoaderTestEncoder, SoyoTestEncoder};

// 两段 payload 都直接执行 PROCESS_EXIT(slot 0)：a0=42、a6=handle(1, 1)、
// a5=0、a7=0，若调用意外返回则停在末尾自环。固定字节由对应交叉汇编器核对，
// 测试构造和内核构建均不读取 ELF，也不调用 objcopy。
const RISCV64_PROCESS_EXIT: &[u8] = &[
    0x13, 0x05, 0xa0, 0x02, // addi a0, zero, 42
    0x13, 0x08, 0x10, 0x00, // addi a6, zero, 1
    0x13, 0x18, 0x08, 0x02, // slli a6, a6, 32
    0x13, 0x08, 0x18, 0x00, // addi a6, a6, 1
    0x93, 0x07, 0x00, 0x00, // addi a5, zero, 0
    0x93, 0x08, 0x00, 0x00, // addi a7, zero, 0
    0x73, 0x00, 0x00, 0x00, // ecall
    0x6f, 0x00, 0x00, 0x00, // jal zero, 0
];

const LOONGARCH64_PROCESS_EXIT: &[u8] = &[
    0x04, 0xa8, 0xc0, 0x02, // addi.d a0, zero, 42
    0x0a, 0x04, 0xc0, 0x02, // addi.d a6, zero, 1
    0x4a, 0x81, 0x41, 0x00, // slli.d a6, a6, 32
    0x4a, 0x05, 0xc0, 0x02, // addi.d a6, a6, 1
    0x09, 0x00, 0xc0, 0x02, // addi.d a5, zero, 0
    0x0b, 0x00, 0xc0, 0x02, // addi.d a7, zero, 0
    0x00, 0x00, 0x2b, 0x00, // syscall 0
    0x00, 0x00, 0x00, 0x50, // b 0
];

pub(super) fn process_exit_payload(target: TargetArch) -> &'static [u8] {
    match target {
        TargetArch::Riscv64 => RISCV64_PROCESS_EXIT,
        TargetArch::LoongArch64 => LOONGARCH64_PROCESS_EXIT,
    }
}

pub(super) fn process_exit_image(target: TargetArch) -> Vec<u8> {
    SoyoTestEncoder::minimal(target, process_exit_payload(target))
        .encode()
        .expect("固定 SOYO 测试映像必须可编码")
}

pub(super) fn loader_image(target: TargetArch) -> Vec<u8> {
    SoyoLoaderTestEncoder::new(target, process_exit_payload(target))
        .encode()
        .expect("完整 SOYO 测试映像必须可编码")
}

pub(super) fn loader_init_array_image(target: TargetArch) -> Vec<u8> {
    SoyoLoaderTestEncoder::new(target, process_exit_payload(target))
        .with_init_array()
        .encode()
        .expect("init array SOYO 测试映像必须可编码")
}

pub(super) fn loader_small_start_info_image(target: TargetArch) -> Vec<u8> {
    SoyoLoaderTestEncoder::new(target, process_exit_payload(target))
        .with_start_info_max_size(192)
        .encode()
        .expect("StartInfo 边界测试映像必须可编码")
}
