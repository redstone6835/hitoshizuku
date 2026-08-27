//! LoongArch64 用户空间 ABI 兼容处理。

use super::syscall::nr;

const SCHED_STUBS: [(&[u8], usize); 4] = [
    (b"sched_setparam", nr::SYS_SCHED_SETPARAM),
    (b"sched_setscheduler", nr::SYS_SCHED_SETSCHEDULER),
    (b"sched_getscheduler", nr::SYS_SCHED_GETSCHEDULER),
    (b"sched_getparam", nr::SYS_SCHED_GETPARAM),
];

/// 修复旧版 musl LoongArch64 动态链接器中仍返回 `ENOSYS` 的调度 syscall 桩。
pub fn patch_interpreter_image(interp: &str, bytes: &mut [u8]) {
    if !interp_basename(interp).starts_with("ld-musl-") {
        return;
    }

    for (name, nr) in SCHED_STUBS {
        let Some(off) = elf64_dynsym_file_offset(bytes, name, 16) else {
            continue;
        };
        patch_enosys_stub(bytes, off, nr);
    }
}

fn patch_enosys_stub(bytes: &mut [u8], off: usize, nr: usize) {
    const ENOSYS_STUB_PREFIX: [u8; 12] = [
        0x63, 0xc0, 0xff, 0x02, // addi.d sp, sp, -16
        0x04, 0x68, 0xbf, 0x02, // li.w a0, -38
        0x61, 0x20, 0xc0, 0x29, // st.d ra, sp, 8
    ];
    let Some(prefix) = bytes.get(off..off + ENOSYS_STUB_PREFIX.len()) else {
        return;
    };
    if prefix != ENOSYS_STUB_PREFIX {
        return;
    }
    let Ok(nr) = u16::try_from(nr) else {
        return;
    };

    let mut patch = [0u8; 16];
    write_u32(&mut patch, 0, 0x0280_000b | ((nr as u32) << 10)); // li.w a7, nr
    write_u32(&mut patch, 4, 0x002b_0000); // syscall 0
    write_u32(&mut patch, 8, 0x0040_8084); // slli.w a0, a0, 0
    write_u32(&mut patch, 12, 0x4c00_0020); // ret
    bytes[off..off + patch.len()].copy_from_slice(&patch);
}

fn interp_basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn elf64_dynsym_file_offset(bytes: &[u8], name: &[u8], patch_len: usize) -> Option<usize> {
    if bytes.get(0..4)? != b"\x7fELF" || *bytes.get(4)? != 2 || *bytes.get(5)? != 1 {
        return None;
    }

    const EHDR_OFF_PHOFF: usize = 0x20;
    const EHDR_OFF_SHOFF: usize = 0x28;
    const EHDR_OFF_PHENTSIZE: usize = 0x36;
    const EHDR_OFF_PHNUM: usize = 0x38;
    const EHDR_OFF_SHENTSIZE: usize = 0x3a;
    const EHDR_OFF_SHNUM: usize = 0x3c;
    const SHDR_TYPE_DYNSYM: u32 = 11;
    const SHDR_OFF_TYPE: usize = 0x04;
    const SHDR_OFF_OFFSET: usize = 0x18;
    const SHDR_OFF_SIZE: usize = 0x20;
    const SHDR_OFF_LINK: usize = 0x28;
    const SHDR_OFF_ENTSIZE: usize = 0x38;
    const SYM_OFF_NAME: usize = 0x00;
    const SYM_OFF_VALUE: usize = 0x08;

    let shoff = read_u64(bytes, EHDR_OFF_SHOFF)? as usize;
    let shentsize = read_u16(bytes, EHDR_OFF_SHENTSIZE)? as usize;
    let shnum = read_u16(bytes, EHDR_OFF_SHNUM)? as usize;
    if shoff == 0 || shentsize < 64 || shnum == 0 {
        return None;
    }

    for idx in 0..shnum {
        let sh = shoff.checked_add(idx.checked_mul(shentsize)?)?;
        if read_u32(bytes, sh + SHDR_OFF_TYPE)? != SHDR_TYPE_DYNSYM {
            continue;
        }

        let sym_off = read_u64(bytes, sh + SHDR_OFF_OFFSET)? as usize;
        let sym_size = read_u64(bytes, sh + SHDR_OFF_SIZE)? as usize;
        let sym_entsize = read_u64(bytes, sh + SHDR_OFF_ENTSIZE)? as usize;
        let str_idx = read_u32(bytes, sh + SHDR_OFF_LINK)? as usize;
        if sym_entsize < 24 || str_idx >= shnum {
            continue;
        }

        let str_sh = shoff.checked_add(str_idx.checked_mul(shentsize)?)?;
        let str_off = read_u64(bytes, str_sh + SHDR_OFF_OFFSET)? as usize;
        let str_size = read_u64(bytes, str_sh + SHDR_OFF_SIZE)? as usize;
        let count = sym_size / sym_entsize;

        for sym_idx in 0..count {
            let sym = sym_off.checked_add(sym_idx.checked_mul(sym_entsize)?)?;
            let name_off = read_u32(bytes, sym + SYM_OFF_NAME)? as usize;
            if !elf_str_eq(bytes, str_off, str_size, name_off, name) {
                continue;
            }
            let value = read_u64(bytes, sym + SYM_OFF_VALUE)? as usize;
            return elf64_vaddr_to_file_offset(
                bytes,
                read_u64(bytes, EHDR_OFF_PHOFF)? as usize,
                read_u16(bytes, EHDR_OFF_PHENTSIZE)? as usize,
                read_u16(bytes, EHDR_OFF_PHNUM)? as usize,
                value,
                patch_len,
            );
        }
    }

    None
}

fn elf64_vaddr_to_file_offset(
    bytes: &[u8],
    phoff: usize,
    phentsize: usize,
    phnum: usize,
    vaddr: usize,
    len: usize,
) -> Option<usize> {
    const PT_LOAD: u32 = 1;
    const PHDR_OFF_TYPE: usize = 0x00;
    const PHDR_OFF_OFFSET: usize = 0x08;
    const PHDR_OFF_VADDR: usize = 0x10;
    const PHDR_OFF_FILESZ: usize = 0x20;

    if phoff == 0 || phentsize < 56 || phnum == 0 {
        return None;
    }
    let vend = vaddr.checked_add(len)?;

    for idx in 0..phnum {
        let ph = phoff.checked_add(idx.checked_mul(phentsize)?)?;
        if read_u32(bytes, ph + PHDR_OFF_TYPE)? != PT_LOAD {
            continue;
        }
        let file_off = read_u64(bytes, ph + PHDR_OFF_OFFSET)? as usize;
        let seg_vaddr = read_u64(bytes, ph + PHDR_OFF_VADDR)? as usize;
        let file_size = read_u64(bytes, ph + PHDR_OFF_FILESZ)? as usize;
        let file_end_vaddr = seg_vaddr.checked_add(file_size)?;
        if vaddr < seg_vaddr || vend > file_end_vaddr {
            continue;
        }
        return file_off.checked_add(vaddr - seg_vaddr);
    }

    None
}

fn elf_str_eq(bytes: &[u8], str_off: usize, str_size: usize, name_off: usize, name: &[u8]) -> bool {
    if name_off >= str_size {
        return false;
    }
    let Some(start) = str_off.checked_add(name_off) else {
        return false;
    };
    let Some(max_end) = str_off.checked_add(str_size) else {
        return false;
    };
    let Some(end) = start.checked_add(name.len()) else {
        return false;
    };
    if end >= max_end {
        return false;
    }
    bytes.get(start..end) == Some(name) && bytes.get(end) == Some(&0)
}

fn read_u16(bytes: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        bytes.get(off..off + 2)?.try_into().ok()?,
    ))
}

fn read_u32(bytes: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(off..off + 4)?.try_into().ok()?,
    ))
}

fn read_u64(bytes: &[u8], off: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        bytes.get(off..off + 8)?.try_into().ok()?,
    ))
}

fn write_u32(bytes: &mut [u8], off: usize, value: u32) {
    bytes[off..off + 4].copy_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_stub_uses_selected_syscall_number() {
        let mut bytes = [
            0x63, 0xc0, 0xff, 0x02, 0x04, 0x68, 0xbf, 0x02, 0x61, 0x20, 0xc0, 0x29, 0, 0, 0, 0,
        ];

        patch_enosys_stub(&mut bytes, 0, nr::SYS_SCHED_SETPARAM);

        assert_eq!(
            read_u32(&bytes, 0),
            Some(0x0280_000b | ((nr::SYS_SCHED_SETPARAM as u32) << 10))
        );
        assert_eq!(read_u32(&bytes, 4), Some(0x002b_0000));
    }
}
