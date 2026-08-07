use soyo::registry::SegmentKind;

use crate::link::error::{LinkError, LinkErrorKind};
use crate::link::model::{LinkImage, PendingRelocation, SymbolValue};

const R_LARCH_B26: u32 = 66;
const R_LARCH_PCALA_HI20: u32 = 71;
const R_LARCH_PCALA_LO12: u32 = 72;
const R_LARCH_CALL36: u32 = 110;

pub(super) fn apply(
    image: &mut LinkImage,
    relocations: &[PendingRelocation],
) -> Result<(), LinkError> {
    for relocation in relocations {
        match relocation.kind {
            R_LARCH_B26 => {
                require_code_target(image, relocation)?;
                patch_b26(image, relocation)?;
            }
            R_LARCH_PCALA_HI20 => {
                require_code_target(image, relocation)?;
                let low = require_pair(relocations, relocation, R_LARCH_PCALA_LO12, 4)?;
                require_pcala_register_pair(image, relocation, low)?;
                patch_pcala_high(image, relocation)?;
            }
            R_LARCH_PCALA_LO12 => {
                require_code_target(image, relocation)?;
                let high = require_pair(relocations, relocation, R_LARCH_PCALA_HI20, -4)?;
                require_pcala_register_pair(image, high, relocation)?;
                patch_pcala_low(image, relocation)?;
            }
            R_LARCH_CALL36 => {
                require_code_target(image, relocation)?;
                patch_call36(image, relocation)?;
            }
            2 => {}
            _ => {
                return Err(LinkError::in_object(
                    LinkErrorKind::UnsupportedRelocation,
                    &relocation.input_path,
                    format!("不支持 LA64 relocation {}", relocation.kind),
                ));
            }
        }
    }
    Ok(())
}

fn patch_call36(image: &mut LinkImage, relocation: &PendingRelocation) -> Result<(), LinkError> {
    let target = image_target(relocation)?;
    let delta = i128::from(target) - i128::from(relocation.place_offset);
    if delta % 4 != 0 {
        return Err(relocation_error(relocation, "CALL36 目标未按 4 字节对齐"));
    }
    let checked = delta
        .checked_add(0x20_000)
        .ok_or_else(|| overflow_error(relocation, "CALL36 范围检查溢出"))?;
    if !fits_signed(checked, 38) {
        return Err(overflow_error(relocation, "CALL36 超出调用范围"));
    }

    let segment = &mut image.segments[relocation.target_segment_index];
    let offset = target_offset(relocation)?;
    let first = read_word(&segment.payload, offset, relocation)?;
    let second = read_word(&segment.payload, offset + 4, relocation)?;
    if first & 0xfe00_0000 != 0x1e00_0000
        || second & 0xfc00_0000 != 0x4c00_0000
        || first & 0x1f != (second >> 5) & 0x1f
    {
        return Err(relocation_error(
            relocation,
            "CALL36 目标不是 PCADDU18I/JIRL 指令对",
        ));
    }

    let high = ((delta + (1 << 17)) >> 18) as u32 & 0x000f_ffff;
    let low = (delta >> 2) as u32 & 0x0000_ffff;
    write_word(
        &mut segment.payload,
        offset,
        (first & 0xfe00_001f) | (high << 5),
    );
    write_word(
        &mut segment.payload,
        offset + 4,
        (second & 0xfc00_03ff) | (low << 10),
    );
    Ok(())
}

fn patch_b26(image: &mut LinkImage, relocation: &PendingRelocation) -> Result<(), LinkError> {
    let target = image_target(relocation)?;
    let delta = i128::from(target) - i128::from(relocation.place_offset);
    if delta % 4 != 0 {
        return Err(relocation_error(relocation, "B26 目标未按 4 字节对齐"));
    }
    let immediate = delta / 4;
    if !fits_signed(immediate, 26) {
        return Err(overflow_error(relocation, "B26 超出跳转范围"));
    }
    let segment = &mut image.segments[relocation.target_segment_index];
    let offset = target_offset(relocation)?;
    let instruction = read_word(&segment.payload, offset, relocation)?;
    if !matches!(instruction & 0xfc00_0000, 0x5000_0000 | 0x5400_0000) {
        return Err(relocation_error(relocation, "B26 目标不是 B/BL 指令"));
    }
    let immediate = immediate as u32 & 0x03ff_ffff;
    let patched =
        (instruction & 0xfc00_0000) | ((immediate & 0xffff) << 10) | ((immediate >> 16) & 0x3ff);
    write_word(&mut segment.payload, offset, patched);
    Ok(())
}

fn patch_pcala_high(
    image: &mut LinkImage,
    relocation: &PendingRelocation,
) -> Result<(), LinkError> {
    let target = image_target(relocation)?;
    let rounded_target = target
        .checked_add(0x800)
        .ok_or_else(|| overflow_error(relocation, "PCALA 目标舍入溢出"))?;
    let target_page = rounded_target & !0xfff;
    let place_page = relocation.place_offset & !0xfff;
    let immediate = (i128::from(target_page) - i128::from(place_page)) >> 12;
    if !fits_signed(immediate, 20) {
        return Err(overflow_error(relocation, "PCALA_HI20 超出可编码范围"));
    }
    let segment = &mut image.segments[relocation.target_segment_index];
    let offset = target_offset(relocation)?;
    let instruction = read_word(&segment.payload, offset, relocation)?;
    if instruction & 0xfe00_0000 != 0x1a00_0000 {
        return Err(relocation_error(
            relocation,
            "PCALA_HI20 目标不是 PCALAU12I",
        ));
    }
    let patched = (instruction & 0xfe00_001f) | ((immediate as u32 & 0xfffff) << 5);
    write_word(&mut segment.payload, offset, patched);
    Ok(())
}

fn patch_pcala_low(image: &mut LinkImage, relocation: &PendingRelocation) -> Result<(), LinkError> {
    let target = image_target(relocation)?;
    let low = target & 0xfff;
    let segment = &mut image.segments[relocation.target_segment_index];
    let offset = target_offset(relocation)?;
    let instruction = read_word(&segment.payload, offset, relocation)?;
    if !is_pcala_low_consumer(instruction) {
        return Err(relocation_error(
            relocation,
            "PCALA_LO12 目标不是支持的 I12 指令",
        ));
    }
    let patched = (instruction & 0xffc0_03ff) | ((low as u32) << 10);
    write_word(&mut segment.payload, offset, patched);
    Ok(())
}

fn require_pair<'a>(
    relocations: &'a [PendingRelocation],
    relocation: &PendingRelocation,
    expected_kind: u32,
    offset_delta: i64,
) -> Result<&'a PendingRelocation, LinkError> {
    let expected_offset = if offset_delta >= 0 {
        relocation.target_offset.checked_add(offset_delta as u64)
    } else {
        relocation
            .target_offset
            .checked_sub(offset_delta.unsigned_abs())
    }
    .ok_or_else(|| relocation_error(relocation, "PCALA 配对偏移溢出"))?;
    let mut matches = relocations.iter().filter(|candidate| {
        candidate.kind == expected_kind
            && candidate.target_segment_index == relocation.target_segment_index
            && candidate.target_offset == expected_offset
            && candidate.symbol_value == relocation.symbol_value
            && candidate.addend == relocation.addend
    });
    let Some(pair) = matches.next() else {
        return Err(relocation_error(relocation, "PCALA relocation 未唯一配对"));
    };
    if matches.next().is_some() {
        return Err(relocation_error(relocation, "PCALA relocation 未唯一配对"));
    }
    Ok(pair)
}

fn require_pcala_register_pair(
    image: &LinkImage,
    high: &PendingRelocation,
    low: &PendingRelocation,
) -> Result<(), LinkError> {
    let segment = &image.segments[high.target_segment_index];
    let high_instruction = read_word(&segment.payload, target_offset(high)?, high)?;
    let low_instruction = read_word(&segment.payload, target_offset(low)?, low)?;
    if high_instruction & 0x1f != (low_instruction >> 5) & 0x1f {
        return Err(relocation_error(
            low,
            "PCALA_HI20 目标寄存器与 LO12 基址寄存器不一致",
        ));
    }
    Ok(())
}

fn is_pcala_low_consumer(instruction: u32) -> bool {
    matches!(
        instruction & 0xffc0_0000,
        0x0200_0000
            | 0x0240_0000
            | 0x0280_0000
            | 0x02c0_0000
            | 0x0300_0000
            | 0x0340_0000
            | 0x0380_0000
            | 0x03c0_0000
            | 0x0600_0000
            | 0x2800_0000
            | 0x2840_0000
            | 0x2880_0000
            | 0x28c0_0000
            | 0x2900_0000
            | 0x2940_0000
            | 0x2980_0000
            | 0x29c0_0000
            | 0x2a00_0000
            | 0x2a40_0000
            | 0x2a80_0000
            | 0x2ac0_0000
            | 0x2b00_0000
            | 0x2b40_0000
            | 0x2b80_0000
            | 0x2bc0_0000
            | 0x2c00_0000
            | 0x2c40_0000
            | 0x2c80_0000
            | 0x2cc0_0000
            | 0x2e00_0000
            | 0x2e40_0000
            | 0x2e80_0000
            | 0x2ec0_0000
            | 0x2f00_0000
            | 0x2f40_0000
            | 0x2f80_0000
            | 0x2fc0_0000
            | 0x3080_0000
            | 0x3280_0000
    )
}

fn image_target(relocation: &PendingRelocation) -> Result<u64, LinkError> {
    let SymbolValue::Image(symbol) = relocation.symbol_value else {
        return Err(relocation_error(
            relocation,
            "指令 relocation 目标不在映像中",
        ));
    };
    add_signed(symbol, relocation.addend)
        .ok_or_else(|| overflow_error(relocation, "relocation 目标溢出"))
}

fn require_code_target(image: &LinkImage, relocation: &PendingRelocation) -> Result<(), LinkError> {
    if image.segments[relocation.target_segment_index].kind != SegmentKind::Code {
        return Err(relocation_error(relocation, "指令 relocation 不在 CODE 中"));
    }
    Ok(())
}

fn target_offset(relocation: &PendingRelocation) -> Result<usize, LinkError> {
    usize::try_from(relocation.target_offset)
        .map_err(|_| relocation_error(relocation, "relocation 偏移超过宿主范围"))
}

fn read_word(
    bytes: &[u8],
    offset: usize,
    relocation: &PendingRelocation,
) -> Result<u32, LinkError> {
    let value = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| relocation_error(relocation, "relocation 指令越界"))?;
    Ok(u32::from_le_bytes(value.try_into().unwrap()))
}

fn write_word(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn fits_signed(value: i128, bits: u32) -> bool {
    let minimum = -(1i128 << (bits - 1));
    let maximum = (1i128 << (bits - 1)) - 1;
    (minimum..=maximum).contains(&value)
}

fn add_signed(value: u64, addend: i64) -> Option<u64> {
    if addend >= 0 {
        value.checked_add(addend as u64)
    } else {
        value.checked_sub(addend.unsigned_abs())
    }
}

fn relocation_error(relocation: &PendingRelocation, message: &str) -> LinkError {
    LinkError::in_object(
        LinkErrorKind::InvalidRelocation,
        &relocation.input_path,
        format!("{message}: {}", relocation.symbol_name),
    )
}

fn overflow_error(relocation: &PendingRelocation, message: &str) -> LinkError {
    LinkError::in_object(
        LinkErrorKind::RelocationOverflow,
        &relocation.input_path,
        format!("{message}: {}", relocation.symbol_name),
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use native_abi::TargetArch;

    use super::*;
    use crate::link::model::LinkSegment;

    fn call36_fixture(first: u32, second: u32, target: u64) -> (LinkImage, PendingRelocation) {
        let mut payload = Vec::new();
        payload.extend_from_slice(&first.to_le_bytes());
        payload.extend_from_slice(&second.to_le_bytes());
        let image = LinkImage {
            target_arch: TargetArch::LoongArch64,
            entry_offset: 0,
            image_virtual_size: 4096,
            segments: vec![LinkSegment {
                kind: SegmentKind::Code,
                virtual_offset: 0,
                payload,
                memory_size: 8,
                alignment: 4096,
            }],
            symbols: BTreeMap::new(),
            pending_relocations: Vec::new(),
        };
        let relocation = PendingRelocation {
            input_path: PathBuf::from("invalid-call.o"),
            target_segment_index: 0,
            target_offset: 0,
            place_offset: 0,
            kind: R_LARCH_CALL36,
            addend: 0,
            symbol_name: "callee".to_owned(),
            symbol_value: SymbolValue::Image(target),
        };
        (image, relocation)
    }

    #[test]
    fn call36_rejects_mismatched_pair_registers() {
        let (mut image, relocation) = call36_fixture(0x1e00_0001, 0x4c00_0041, 8);
        assert_eq!(
            patch_call36(&mut image, &relocation).unwrap_err().kind(),
            LinkErrorKind::InvalidRelocation
        );
    }

    #[test]
    fn call36_rejects_out_of_range_target() {
        let (mut image, relocation) = call36_fixture(0x1e00_0001, 0x4c00_0021, 1u64 << 38);
        assert_eq!(
            patch_call36(&mut image, &relocation).unwrap_err().kind(),
            LinkErrorKind::RelocationOverflow
        );
    }

    #[test]
    fn pcala_pair_rejects_mismatched_base_registers() {
        let (mut image, high) = call36_fixture(0x1a00_0004, 0x02c0_00a4, 8);
        let low = PendingRelocation {
            input_path: PathBuf::from("invalid-pcala.o"),
            target_segment_index: 0,
            target_offset: 4,
            place_offset: 4,
            kind: R_LARCH_PCALA_LO12,
            addend: 0,
            symbol_name: "value".to_owned(),
            symbol_value: SymbolValue::Image(8),
        };
        let high = PendingRelocation {
            kind: R_LARCH_PCALA_HI20,
            ..high
        };

        assert_eq!(
            apply(&mut image, &[high, low]).unwrap_err().kind(),
            LinkErrorKind::InvalidRelocation
        );
    }
}
