use std::collections::{BTreeMap, BTreeSet};

use soyo::registry::SegmentKind;

use crate::link::error::{LinkError, LinkErrorKind};
use crate::link::model::{LinkImage, PendingRelocation, SymbolValue};

const R_RISCV_CALL_PLT: u32 = 19;
const R_RISCV_PCREL_HI20: u32 = 23;
const R_RISCV_PCREL_LO12_I: u32 = 24;
const R_RISCV_PCREL_LO12_S: u32 = 25;
const R_RISCV_TPREL_HI20: u32 = 29;
const R_RISCV_TPREL_LO12_I: u32 = 30;
const R_RISCV_TPREL_LO12_S: u32 = 31;
const R_RISCV_TPREL_ADD: u32 = 32;
const R_RISCV_RELAX: u32 = 51;

pub(super) fn apply(
    image: &mut LinkImage,
    relocations: &[PendingRelocation],
) -> Result<(), LinkError> {
    apply_tls(image, relocations)?;
    let mut high_values = BTreeMap::new();
    for relocation in relocations {
        if relocation.kind == R_RISCV_PCREL_HI20 {
            require_code_target(image, relocation)?;
            let delta = image_delta(relocation)?;
            patch_u_type(image, relocation, delta)?;
            if high_values.insert(relocation.place_offset, delta).is_some() {
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
                let delta = *high_values
                    .get(&high_place)
                    .ok_or_else(|| relocation_error(relocation, "PCREL_LO12 找不到对应的 HI20"))?;
                used_high.insert(high_place);
                if relocation.kind == R_RISCV_PCREL_LO12_I {
                    patch_i_type(image, relocation, delta)?;
                } else {
                    patch_s_type(image, relocation, delta)?;
                }
            }
            2 | R_RISCV_RELAX | R_RISCV_TPREL_HI20 | R_RISCV_TPREL_LO12_I
            | R_RISCV_TPREL_LO12_S | R_RISCV_TPREL_ADD => {}
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

#[derive(Clone, Copy)]
struct TlsAdd {
    segment_index: usize,
    symbol_value: SymbolValue,
    addend: i64,
    source_register: u32,
    destination_register: u32,
}

fn apply_tls(image: &mut LinkImage, relocations: &[PendingRelocation]) -> Result<(), LinkError> {
    let mut highs = BTreeMap::new();
    for (index, relocation) in relocations.iter().enumerate() {
        if relocation.kind != R_RISCV_TPREL_HI20 {
            continue;
        }
        require_code_target(image, relocation)?;
        require_tls_symbol(relocation)?;
        let value = tls_value(relocation)?;
        patch_tls_hi(image, relocation, value)?;
        if highs
            .insert(
                (relocation.target_segment_index, relocation.target_offset),
                index,
            )
            .is_some()
        {
            return Err(relocation_error(relocation, "重复的 TPREL_HI20 位置"));
        }
    }

    let mut adds = Vec::new();
    for relocation in relocations {
        if relocation.kind != R_RISCV_TPREL_ADD {
            continue;
        }
        require_code_target(image, relocation)?;
        require_tls_symbol(relocation)?;
        let instruction = read_word(
            &image.segments[relocation.target_segment_index].payload,
            target_offset(relocation)?,
            relocation,
        )?;
        let destination_register = (instruction >> 7) & 0x1f;
        let source_register = (instruction >> 15) & 0x1f;
        if instruction & 0x7f != 0x33
            || (instruction >> 12) & 7 != 0
            || (instruction >> 25) & 0x7f != 0
            || (instruction >> 20) & 0x1f != 4
        {
            return Err(relocation_error(
                relocation,
                "TPREL_ADD 目标不是 rd,rs1,tp 的 ADD",
            ));
        }
        let matching_high = highs.iter().any(|(&(segment_index, _), &high_index)| {
            let high = &relocations[high_index];
            segment_index == relocation.target_segment_index
                && high.symbol_value == relocation.symbol_value
                && high.addend == relocation.addend
                && matches!(
                    hi_register(image, high),
                    Ok(register) if register == source_register
                )
        });
        if !matching_high {
            return Err(relocation_error(relocation, "TPREL_ADD 找不到对应的 HI20"));
        }
        adds.push(TlsAdd {
            segment_index: relocation.target_segment_index,
            symbol_value: relocation.symbol_value,
            addend: relocation.addend,
            source_register,
            destination_register,
        });
    }

    let every_high_has_add = highs.iter().all(|(&(segment_index, _), &high_index)| {
        let high = &relocations[high_index];
        let Ok(register) = hi_register(image, high) else {
            return false;
        };
        adds.iter().any(|add| {
            add.segment_index == segment_index
                && add.symbol_value == high.symbol_value
                && add.addend == high.addend
                && add.source_register == register
        })
    });

    let mut add_has_low = vec![false; adds.len()];
    for relocation in relocations {
        if !matches!(relocation.kind, R_RISCV_TPREL_LO12_I | R_RISCV_TPREL_LO12_S) {
            continue;
        }
        require_code_target(image, relocation)?;
        require_tls_symbol(relocation)?;
        let instruction = read_word(
            &image.segments[relocation.target_segment_index].payload,
            target_offset(relocation)?,
            relocation,
        )?;
        let base_register = (instruction >> 15) & 0x1f;
        let candidate = adds
            .iter()
            .enumerate()
            .filter(|(_index, add)| {
                add.segment_index == relocation.target_segment_index
                    && add.symbol_value == relocation.symbol_value
                    && add.addend == relocation.addend
                    && add.destination_register == base_register
            })
            .next()
            .map(|(index, add)| (index, *add));
        let Some((_add_index, add)) = candidate else {
            return Err(relocation_error(
                relocation,
                "TPREL_LO12 找不到对应的 TPREL_ADD",
            ));
        };
        for (index, candidate) in adds.iter().enumerate() {
            if candidate.segment_index == add.segment_index
                && candidate.symbol_value == relocation.symbol_value
                && candidate.addend == relocation.addend
                && candidate.destination_register == base_register
            {
                add_has_low[index] = true;
            }
        }
        let value = tls_value(relocation)?;
        if relocation.kind == R_RISCV_TPREL_LO12_I {
            patch_tls_i(image, relocation, value)?;
        } else {
            patch_tls_s(image, relocation, value)?;
        }
    }

    if !every_high_has_add || add_has_low.iter().any(|used| !used) {
        return Err(LinkError::new(
            LinkErrorKind::InvalidRelocation,
            "存在未配对的 RV64 TLS relocation",
        ));
    }
    Ok(())
}

fn require_tls_symbol(relocation: &PendingRelocation) -> Result<(), LinkError> {
    if !matches!(relocation.symbol_value, SymbolValue::Tls(_)) {
        return Err(relocation_error(
            relocation,
            "TLS relocation 目标不是 TLS 符号",
        ));
    }
    Ok(())
}

fn tls_value(relocation: &PendingRelocation) -> Result<i64, LinkError> {
    let SymbolValue::Tls(value) = relocation.symbol_value else {
        return Err(relocation_error(
            relocation,
            "TLS relocation 目标不是 TLS 符号",
        ));
    };
    let value = add_signed(value, relocation.addend)
        .ok_or_else(|| overflow_error(relocation, "TLS relocation 加法溢出"))?;
    i64::try_from(value).map_err(|_| overflow_error(relocation, "TLS relocation 超出有符号范围"))
}

fn hi_register(image: &LinkImage, relocation: &PendingRelocation) -> Result<u32, LinkError> {
    let instruction = read_word(
        &image.segments[relocation.target_segment_index].payload,
        target_offset(relocation)?,
        relocation,
    )?;
    Ok((instruction >> 7) & 0x1f)
}

fn patch_tls_hi(
    image: &mut LinkImage,
    relocation: &PendingRelocation,
    value: i64,
) -> Result<(), LinkError> {
    let (high, _) = split_high_low(value, relocation)?;
    let segment = &mut image.segments[relocation.target_segment_index];
    let offset = target_offset(relocation)?;
    let instruction = read_word(&segment.payload, offset, relocation)?;
    if instruction & 0x7f != 0x37 {
        return Err(relocation_error(relocation, "TPREL_HI20 目标不是 LUI"));
    }
    write_word(
        &mut segment.payload,
        offset,
        (instruction & 0xfff) | ((high as u32 & 0xfffff) << 12),
    );
    Ok(())
}

fn patch_tls_i(
    image: &mut LinkImage,
    relocation: &PendingRelocation,
    value: i64,
) -> Result<(), LinkError> {
    let (_, low) = split_high_low(value, relocation)?;
    let segment = &mut image.segments[relocation.target_segment_index];
    let offset = target_offset(relocation)?;
    let instruction = read_word(&segment.payload, offset, relocation)?;
    if !matches!(instruction & 0x7f, 0x03 | 0x13 | 0x67) {
        return Err(relocation_error(
            relocation,
            "TPREL_LO12_I 目标不是 I 型指令",
        ));
    }
    write_word(
        &mut segment.payload,
        offset,
        (instruction & 0x000f_ffff) | ((low as u32 & 0xfff) << 20),
    );
    Ok(())
}

fn patch_tls_s(
    image: &mut LinkImage,
    relocation: &PendingRelocation,
    value: i64,
) -> Result<(), LinkError> {
    let (_, low) = split_high_low(value, relocation)?;
    let segment = &mut image.segments[relocation.target_segment_index];
    let offset = target_offset(relocation)?;
    let instruction = read_word(&segment.payload, offset, relocation)?;
    if instruction & 0x7f != 0x23 {
        return Err(relocation_error(
            relocation,
            "TPREL_LO12_S 目标不是 S 型指令",
        ));
    }
    let immediate = low as u32 & 0xfff;
    write_word(
        &mut segment.payload,
        offset,
        (instruction & 0x01ff_f07f) | ((immediate & 0x1f) << 7) | ((immediate & 0xfe0) << 20),
    );
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

fn target_offset(relocation: &PendingRelocation) -> Result<usize, LinkError> {
    usize::try_from(relocation.target_offset)
        .map_err(|_| relocation_error(relocation, "TLS relocation 偏移超过宿主范围"))
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
            runtime_arrays: crate::link::model::RuntimeArrays::EMPTY,
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

    fn tls_fixture(
        low_instruction: u32,
        symbol_value: SymbolValue,
    ) -> (LinkImage, Vec<PendingRelocation>) {
        let mut payload = Vec::new();
        payload.extend_from_slice(&0x0000_0537u32.to_le_bytes());
        payload.extend_from_slice(&0x0045_05b3u32.to_le_bytes());
        payload.extend_from_slice(&low_instruction.to_le_bytes());
        let image = LinkImage {
            target_arch: TargetArch::Riscv64,
            entry_offset: 0,
            image_virtual_size: 4096,
            segments: vec![LinkSegment {
                kind: SegmentKind::Code,
                virtual_offset: 0,
                payload,
                memory_size: 12,
                alignment: 4096,
            }],
            symbols: BTreeMap::new(),
            pending_relocations: Vec::new(),
            runtime_arrays: crate::link::model::RuntimeArrays::EMPTY,
        };
        let high = PendingRelocation {
            input_path: PathBuf::from("invalid-tls.o"),
            target_segment_index: 0,
            target_offset: 0,
            place_offset: 0,
            kind: R_RISCV_TPREL_HI20,
            addend: 0,
            symbol_name: "tls_value".to_owned(),
            symbol_value,
        };
        let add = PendingRelocation {
            target_offset: 4,
            place_offset: 4,
            kind: R_RISCV_TPREL_ADD,
            ..high.clone()
        };
        let low = PendingRelocation {
            target_offset: 8,
            place_offset: 8,
            kind: R_RISCV_TPREL_LO12_I,
            ..high.clone()
        };
        (image, vec![high, add, low])
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
    fn pcrel_pair_allows_base_register_move() {
        let (mut image, high) = call_fixture(0x0000_0517, 0x0005_0593, 8);
        image.segments[0]
            .payload
            .extend_from_slice(&0x0005_b603u32.to_le_bytes());
        image.segments[0].memory_size = 12;
        let low = PendingRelocation {
            input_path: PathBuf::from("invalid-pcrel.o"),
            target_segment_index: 0,
            target_offset: 8,
            place_offset: 8,
            kind: R_RISCV_PCREL_LO12_I,
            addend: 0,
            symbol_name: ".Lpcrel_hi".to_owned(),
            symbol_value: SymbolValue::Image(0),
        };
        let high = PendingRelocation {
            kind: R_RISCV_PCREL_HI20,
            ..high
        };

        apply(&mut image, &[high, low]).expect("编译器搬运 HI20 基址后仍应允许 LO12 配对");
    }

    #[test]
    fn pcrel_high_can_feed_multiple_matching_low_relocations() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&0x0000_0597u32.to_le_bytes());
        payload.extend_from_slice(&0x0005_a603u32.to_le_bytes());
        payload.extend_from_slice(&0x00c5_a023u32.to_le_bytes());
        let mut image = LinkImage {
            target_arch: TargetArch::Riscv64,
            entry_offset: 0,
            image_virtual_size: 4096,
            segments: vec![LinkSegment {
                kind: SegmentKind::Code,
                virtual_offset: 0,
                payload,
                memory_size: 12,
                alignment: 4096,
            }],
            symbols: BTreeMap::new(),
            pending_relocations: Vec::new(),
            runtime_arrays: crate::link::model::RuntimeArrays::EMPTY,
        };
        let high = PendingRelocation {
            input_path: PathBuf::from("multiple-low.o"),
            target_segment_index: 0,
            target_offset: 0,
            place_offset: 0,
            kind: R_RISCV_PCREL_HI20,
            addend: 0,
            symbol_name: "value".to_owned(),
            symbol_value: SymbolValue::Image(0x100),
        };
        let low_i = PendingRelocation {
            target_offset: 4,
            place_offset: 4,
            kind: R_RISCV_PCREL_LO12_I,
            symbol_name: ".Lpcrel_hi".to_owned(),
            symbol_value: SymbolValue::Image(0),
            ..high.clone()
        };
        let low_s = PendingRelocation {
            target_offset: 8,
            place_offset: 8,
            kind: R_RISCV_PCREL_LO12_S,
            ..low_i.clone()
        };

        apply(&mut image, &[high, low_i, low_s]).expect("同基址的多个 LO12 应合法配对");
    }

    #[test]
    fn tls_rejects_mismatched_low_base_register() {
        let (mut image, relocations) = tls_fixture(0x0006_3603, SymbolValue::Tls(0));
        assert_eq!(
            apply_tls(&mut image, &relocations).unwrap_err().kind(),
            LinkErrorKind::InvalidRelocation
        );
    }

    #[test]
    fn tls_rejects_non_tls_symbol_and_overflow() {
        let (mut image, relocations) = tls_fixture(0x0005_b603, SymbolValue::Image(0));
        assert_eq!(
            apply_tls(&mut image, &relocations).unwrap_err().kind(),
            LinkErrorKind::InvalidRelocation
        );

        let (mut image, relocations) = tls_fixture(0x0005_b603, SymbolValue::Tls(u64::MAX));
        assert_eq!(
            apply_tls(&mut image, &relocations).unwrap_err().kind(),
            LinkErrorKind::RelocationOverflow
        );
    }

    #[test]
    fn tls_accepts_distinct_add_destination_register() {
        let (mut image, relocations) = tls_fixture(0x0005_b603, SymbolValue::Tls(0));

        apply_tls(&mut image, &relocations)
            .expect("TPREL_ADD 可以把 HI20 基址写入不同的目标寄存器");
    }

    #[test]
    fn tls_rejects_unpaired_high_and_low_relocations() {
        let (mut image, relocations) = tls_fixture(0x0005_b603, SymbolValue::Tls(0));
        assert_eq!(
            apply_tls(&mut image, &relocations[..1]).unwrap_err().kind(),
            LinkErrorKind::InvalidRelocation
        );

        let low = relocations[2].clone();
        assert_eq!(
            apply_tls(&mut image, &[low]).unwrap_err().kind(),
            LinkErrorKind::InvalidRelocation
        );
    }

    #[test]
    fn tls_accepts_scheduled_and_shared_add_sequence() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&0x0000_0537u32.to_le_bytes());
        payload.extend_from_slice(&0x0000_0537u32.to_le_bytes());
        payload.extend_from_slice(&0x0045_0533u32.to_le_bytes());
        payload.extend_from_slice(&0x0005_2503u32.to_le_bytes());
        let mut image = LinkImage {
            target_arch: TargetArch::Riscv64,
            entry_offset: 0,
            image_virtual_size: 4096,
            segments: vec![LinkSegment {
                kind: SegmentKind::Code,
                virtual_offset: 0,
                payload,
                memory_size: 16,
                alignment: 4096,
            }],
            symbols: BTreeMap::new(),
            pending_relocations: Vec::new(),
            runtime_arrays: crate::link::model::RuntimeArrays::EMPTY,
        };
        let high = PendingRelocation {
            input_path: PathBuf::from("shared-tls-add.o"),
            target_segment_index: 0,
            target_offset: 0,
            place_offset: 0,
            kind: R_RISCV_TPREL_HI20,
            addend: 0,
            symbol_name: "tls_value".to_owned(),
            symbol_value: SymbolValue::Tls(0),
        };
        let second_high = PendingRelocation {
            target_offset: 4,
            place_offset: 4,
            ..high.clone()
        };
        let add = PendingRelocation {
            target_offset: 8,
            place_offset: 8,
            kind: R_RISCV_TPREL_ADD,
            ..high.clone()
        };
        let low = PendingRelocation {
            target_offset: 12,
            place_offset: 12,
            kind: R_RISCV_TPREL_LO12_I,
            ..high.clone()
        };

        apply_tls(&mut image, &[high, second_high, add, low])
            .expect("控制流汇合后的 TPREL_ADD 应能服务多个匹配 HI20");
    }

    #[test]
    fn tls_accepts_backward_branch_to_shared_low_sequence() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&0x0005_2503u32.to_le_bytes());
        payload.extend_from_slice(&0x0000_0537u32.to_le_bytes());
        payload.extend_from_slice(&0x0045_0533u32.to_le_bytes());
        let mut image = LinkImage {
            target_arch: TargetArch::Riscv64,
            entry_offset: 0,
            image_virtual_size: 4096,
            segments: vec![LinkSegment {
                kind: SegmentKind::Code,
                virtual_offset: 0,
                payload,
                memory_size: 12,
                alignment: 4096,
            }],
            symbols: BTreeMap::new(),
            pending_relocations: Vec::new(),
            runtime_arrays: crate::link::model::RuntimeArrays::EMPTY,
        };
        let low = PendingRelocation {
            input_path: PathBuf::from("backward-tls-low.o"),
            target_segment_index: 0,
            target_offset: 0,
            place_offset: 0,
            kind: R_RISCV_TPREL_LO12_I,
            addend: 0,
            symbol_name: "tls_value".to_owned(),
            symbol_value: SymbolValue::Tls(0),
        };
        let high = PendingRelocation {
            target_offset: 4,
            place_offset: 4,
            kind: R_RISCV_TPREL_HI20,
            ..low.clone()
        };
        let add = PendingRelocation {
            target_offset: 8,
            place_offset: 8,
            kind: R_RISCV_TPREL_ADD,
            ..low.clone()
        };

        apply_tls(&mut image, &[low, high, add]).expect("后置地址计算跳回共享 LO12 的序列应合法");
    }
}
