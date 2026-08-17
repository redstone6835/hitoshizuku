#!/usr/bin/env python3
"""把 MyGO 裸二进制内核包装成 QEMU 可加载的 ELF。
用法: wrap-elf.py <arch: la|rv> <input-raw> <output-elf>"""
import struct
import sys

arch, src, dst = sys.argv[1], sys.argv[2], sys.argv[3]
raw = open(src, 'rb').read()
if arch == 'la':
    machine, p_vaddr, p_paddr = 258, 0x9000_0000_9000_0000, 0x9000_0000
else:
    machine, p_vaddr, p_paddr = 243, 0xFFFF_FFC0_8020_0000, 0x8020_0000
header_len = 64 + 56
ehdr = struct.pack('<16sHHIQQQIHHHHHH',
                   b'\x7fELF' + bytes([2, 1, 1, 0, 0, 0, 0, 0, 0, 0]),
                   2, machine, 1, p_vaddr, 64, 0, 0, 64, 56, 1, 0, 0, 0)
phdr = struct.pack('<IIQQQQQQ', 1, 7, header_len, p_vaddr, p_paddr,
                   len(raw), len(raw), 0x1000)
open(dst, 'wb').write(ehdr + phdr + raw)
print(f"{dst}: {len(raw)} bytes, entry={p_vaddr:#x}")
