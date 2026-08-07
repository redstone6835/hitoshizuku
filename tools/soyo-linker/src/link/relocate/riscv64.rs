use std::collections::{BTreeMap, BTreeSet};

use soyo::registry::SegmentKind;

use crate::link::error::{LinkError, LinkErrorKind};
use crate::link::model::{LinkImage, PendingRelocation, SymbolValue};

const R_RISCV_CALL_PLT: u32 = 19;
const R_RISCV_PCREL_HI20: u32 = 23;
const R_RISCV_PCREL_LO12_I: u32 = 24;
const R_RISCV_PCREL_LO12_S: u32 = 25;

pub(super) fn apply(
    image: &mut LinkImage,
    relocations: &[PendingRelocation],
) -> Result<(), LinkError> {
    let mut high_values = BTreeMap::new();
    for relocation in relocations {
        if relocation.kind == R_RISCV_PCREL_HI20 {
            require_code_target(image, relocation)?;
            let delta = image_delta(relocation)?;
            let base_register = patch_u_type(image, relocation, delta)?;
            if high_values
                .insert(relocation.place_offset, (delta, base_register))
                .is_some()
            {
                return Err(relocation_error(relocation, "重复的 PCREL_HI20 位置"));
            }
        }
    }

    let mut used_high = BTreeSet::new();
    for relocation in relocations {
        match relocation.kind {
            R_RISCV_CALL_PLT => {
                require_code_target(image, relocation)?;
                patch_call(image, relocation, image_delta(relocation)?)?;
            }
            R_RISCV_PCREL_HI20 => {}
            R_RISCV_PCREL_LO12_I | R_RISCV_PCREL_LO12_S => {
                require_code_target(image, relocation)?;
                let SymbolValue::Image(high_place) = relocation.symbol_value else {
                    return Err(relocation_error(
                        relocation,
                        "PCREL_LO12 配对符号不在映像中",
                    ));
                };
                let high_place = add_signed(high_place, relocation.addend)
                    .ok_or_else(|| relocation_error(relocation, "PCREL_LO12 配对位置溢出"))?;
                let (delta, high_register) = *high_values
                    .get(&high_place)
                    .ok_or_else(|| relocation_error(relocation, "PCREL_LO12 找不到对应的 HI20"))?;
                if !used_high.insert(high_place) {
                    return Err(relocation_error(
                        relocation,
                        "同一个 PCREL_HI20 被多个 LO12 消费",
                    ));
                }
                let low_register = if relocation.kind == R_RISCV_PCREL_LO12_I {
                    patch_i_type(image, relocation, delta)?
                } else {
                    patch_s_type(image, relocation, delta)?
                };
                if high_register != low_register {
                    return Err(relocation_error(
                        relocation,
                        "PCREL_HI20 目标寄存器与 LO12 基址寄存器不一致",
                    ));
                }
            }
            2 => {}
            _ => {
                return Err(LinkError::in_object(
                    LinkErrorKind::UnsupportedRelocation,
                    &relocation.input_path,
                    format!("不支持 RV64 relocation {}", relocation.kind),
                ));
            }
        }
    }
    if used_high.len() != high_values.len() {
        return Err(LinkError::new(
            LinkErrorKind::InvalidRelocation,
            "存在未配对的 RV64 PCREL_HI20",
        ));
    }
    Ok(())
}

fn image_delta(relocation: &PendingRelocation) -> Result<i64, LinkError> {
    let SymbolValue::Image(symbol) = relocation.symbol_value else {
        return Err(relocation_error(
            relocation,
            "指令 relocation 目标不在映像中",
        ));
    };
    let target = i128::from(symbol) + i128::from(relocation.addend);
    let delta = target - i128::from(relocation.place_offset);
    i64::try_from(delta).map_err(|_| relocation_error(relocation, "relocation 差值溢出"))
}

fn patch_call(
    image: &mut LinkImage,
    relocation: &PendingRelocation,
    delta: i64,
) -> Result<(), LinkError> {
    let (high, low) = split_high_low(delta, relocation)?;
    let segment = &mut image.segments[relocation.target_segment_index];
    let offset = usize::try_from(relocation.target_offset)
        .map_err(|_| relocation_error(relocation, "CALL_PLT 偏移超过宿主范围"))?;
    let first = read_word(&segment.payload, offset, relocation)?;
    let second = read_word(&segment.payload, offset + 4, relocation)?;
    if first & 0x7f != 0x17 || second & 0x7f != 0x67 || (first >> 7) & 0x1f != (second >> 15) & 0x1f
    {
        return Err(relocation_error(
            relocation,
            "CALL_PLT 目标不是 AUIPC/JALR 指令对",
        ));
    }
    write_word(
        &mut segment.payload,
        offset,
        (first & 0xfff) | ((high as u32 & 0xfffff) << 12),
    );
    write_word(
        &mut segment.payload,
        offset + 4,
        (second & 0x000f_ffff) | ((low as u32 & 0xfff) << 20),
    );
    Ok(())
}

fn patch_u_type(
    image: &mut LinkImage,
    relocation: &PendingRelocation,
    delta: i64,
) -> Result<u32, LinkError> {
    let (high, _) = split_high_low(delta, relocation)?;
    let segment = &mut image.segments[relocation.target_segment_index];
    let offset = usize::try_from(relocation.target_offset)
        .map_err(|_| relocation_error(relocation, "PCREL_HI20 偏移超过宿主范围"))?;
    let instruction = read_word(&segment.payload, offset, relocation)?;
    if instruction & 0x7f != 0x17 {
        return Err(relocation_error(relocation, "PCREL_HI20 目标不是 AUIPC"));
    }
    write_word(
        &mut segment.payload,
        offset,
        (instruction & 0xfff) | ((high as u32 & 0xfffff) << 12),
    );
    Ok((instruction >> 7) & 0x1f)
}

fn patch_i_type(
    image: &mut LinkImage,
    relocation: &PendingRelocation,
    delta: i64,
) -> Result<u32, LinkError> {
    let (_, low) = split_high_low(delta, relocation)?;
    let segment = &mut image.segments[relocation.target_segment_index];
    let offset = usize::try_from(relocation.target_offset)
        .map_err(|_| relocation_error(relocation, "PCREL_LO12_I 偏移超过宿主范围"))?;
    let instruction = read_word(&segment.payload, offset, relocation)?;
    let opcode = instruction & 0x7f;
    if !matches!(opcode, 0x03 | 0x13 | 0x67) {
        return Err(relocation_error(
            relocation,
            "PCREL_LO12_I 目标不是 I 型指令",
        ));
    }
    write_word(
        &mut segment.payload,
        offset,
        (instruction & 0x000f_ffff) | ((low as u32 & 0xfff) << 20),
    );
    Ok((instruction >> 15) & 0x1f)
}

fn patch_s_type(
    image: &mut LinkImage,
    relocation: &PendingRelocation,
    delta: i64,
) -> Result<u32, LinkError> {
    let (_, low) = split_high_low(delta, relocation)?;
    let segment = &mut image.segments[relocation.target_segment_index];
    let offset = usize::try_from(relocation.target_offset)
        .map_err(|_| relocation_error(relocation, "PCREL_LO12_S 偏移超过宿主范围"))?;
    let instruction = read_word(&segment.payload, offset, relocation)?;
    if instruction & 0x7f != 0x23 {
        return Err(relocation_error(
            relocation,
            "PCREL_LO12_S 目标不是 S 型指令",
        ));
    }
    let immediate = low as u32 & 0xfff;
    let patched =
        (instruction & 0x01ff_f07f) | ((immediate & 0x1f) << 7) | ((immediate & 0xfe0) << 20);
    write_word(&mut segment.payload, offset, patched);
    Ok((instruction >> 15) & 0x1f)
}

fn split_high_low(value: i64, relocation: &PendingRelocation) -> Result<(i64, i64), LinkError> {
    let high = value
        .checked_add(0x800)
        .ok_or_else(|| relocation_error(relocation, "relocation 舍入溢出"))?
        >> 12;
    let low = value - (high << 12);
    if !fits_signed(high, 20) || !fits_signed(low, 12) {
        return Err(LinkError::in_object(
            LinkErrorKind::RelocationOverflow,
            &relocation.input_path,
            format!("RV64 relocation {} 超出可编码范围", relocation.kind),
        ));
    }
    Ok((high, low))
}

fn require_code_target(image: &LinkImage, relocation: &PendingRelocation) -> Result<(), LinkError> {
    if image.segments[relocation.target_segment_index].kind != SegmentKind::Code {
        return Err(relocation_error(relocation, "指令 relocation 不在 CODE 中"));
    }
    Ok(())
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

fn fits_signed(value: i64, bits: u32) -> bool {
    let minimum = -(1i64 << (bits - 1));
    let maximum = (1i64 << (bits - 1)) - 1;
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use native_abi::TargetArch;

    use super::*;
    use crate::link::model::LinkSegment;

    fn call_fixture(first: u32, second: u32, target: u64) -> (LinkImage, PendingRelocation) {
        let mut payload = Vec::new();
        payload.extend_from_slice(&first.to_le_bytes());
        payload.extend_from_slice(&second.to_le_bytes());
        let image = LinkImage {
            target_arch: TargetArch::Riscv64,
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
            kind: R_RISCV_CALL_PLT,
            addend: 0,
            symbol_name: "callee".to_owned(),
            symbol_value: SymbolValue::Image(target),
        };
        (image, relocation)
    }

    #[test]
    fn call_rejects_mismatched_pair_registers() {
        let (mut image, relocation) = call_fixture(0x0000_0097, 0x0001_00e7, 8);
        assert_eq!(
            patch_call(&mut image, &relocation, 8).unwrap_err().kind(),
            LinkErrorKind::InvalidRelocation
        );
    }

    #[test]
    fn call_rejects_out_of_range_target() {
        let (mut image, relocation) = call_fixture(0x0000_0097, 0x0000_80e7, 1u64 << 40);
        assert_eq!(
            patch_call(&mut image, &relocation, 1i64 << 40)
                .unwrap_err()
                .kind(),
            LinkErrorKind::RelocationOverflow
        );
    }

    #[test]
    fn pcrel_pair_rejects_mismatched_base_registers() {
        let (mut image, high) = call_fixture(0x0000_0517, 0x0005_b503, 8);
        let low = PendingRelocation {
            input_path: PathBuf::from("invalid-pcrel.o"),
            target_segment_index: 0,
            target_offset: 4,
            place_offset: 4,
            kind: R_RISCV_PCREL_LO12_I,
            addend: 0,
            symbol_name: ".Lpcrel_hi".to_owned(),
            symbol_value: SymbolValue::Image(0),
        };
        let high = PendingRelocation {
            kind: R_RISCV_PCREL_HI20,
            ..high
        };

        assert_eq!(
            apply(&mut image, &[high, low]).unwrap_err().kind(),
            LinkErrorKind::InvalidRelocation
        );
    }
}
