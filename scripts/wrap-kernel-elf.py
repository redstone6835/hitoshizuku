#!/usr/bin/env python3
"""把裸内核镜像包装成最小 ELF64（loongarch64），使 QEMU virt 走 Linux 直启路径。

QEMU 的 loongarch virt 机器对 -kernel 只接受 ELF；裸二进制会被拒绝
（"Failed to load ELF"）。包装后 QEMU 会按直启协议把内核装载到 p_paddr，
并在 a0/a1/a2 中传入 cmdline/FDT，与 `-kernel kernel-la` 的团队用法一致。

用法: wrap-kernel-elf.py <raw-kernel> <output.elf>
"""

import struct
import sys

EM_LOONGARCH = 258
LOAD_ADDR = 0x9000_0000
PH_OFF = 64
PAD = 0x1000  # 文件内偏移对齐（PT_LOAD 对齐惯例）


def main() -> int:
    if len(sys.argv) != 3:
        print(__doc__, file=sys.stderr)
        return 2
    raw_path, out_path = sys.argv[1], sys.argv[2]
    with open(raw_path, "rb") as f:
        raw = f.read()
    filesz = len(raw)
    entry = LOAD_ADDR

    # ELF64 头（64 字节）
    ident = bytes([0x7F, 0x45, 0x4C, 0x46, 0x02, 0x01, 0x01, 0x00]) + bytes(8)
    ehdr = struct.pack(
        "<16sHHIQQQIHHHHHH",
        ident,
        2,          # ET_EXEC
        EM_LOONGARCH,
        1,          # e_version
        entry,
        PH_OFF,     # e_phoff
        0,          # e_shoff
        0,          # e_flags
        64,         # e_ehsize
        56,         # e_phentsize
        1,          # e_phnum
        0, 0, 0,    # e_shentsize/e_shnum/e_shstrndx
    )
    # PT_LOAD（56 字节）
    phdr = struct.pack(
        "<IIQQQQQQ",
        1,          # PT_LOAD
        7,          # p_flags: R|W|X
        PAD,        # p_offset
        LOAD_ADDR,  # p_vaddr
        LOAD_ADDR,  # p_paddr
        filesz,     # p_filesz
        filesz,     # p_memsz
        0x1000,     # p_align
    )
    with open(out_path, "wb") as f:
        f.write(ehdr)
        f.write(phdr)
        assert len(ehdr) + len(phdr) <= PAD
        f.write(bytes(PAD - len(ehdr) - len(phdr)))
        f.write(raw)
    print(f"wrapped {raw_path} ({filesz} bytes) -> {out_path} entry={entry:#x}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
