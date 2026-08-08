//! 目标架构 relocation 应用与 SOYO runtime relocation 投影。

mod loongarch64;
mod riscv64;

use std::collections::BTreeSet;

use native_abi::TargetArch;
use soyo::Relocation;
use soyo::registry::{RelocationKind, SegmentKind};

use super::error::{LinkError, LinkErrorKind};
use super::model::{LinkImage, LinkedImage, PendingRelocation, SymbolValue};

/// 完成静态 relocation，并把必须等待 image base 的绝对数据地址转成 SOYO relocation。
pub fn apply_relocations(mut image: LinkImage) -> Result<LinkedImage, LinkError> {
    let pending = std::mem::take(&mut image.pending_relocations);
    match image.target_arch {
        TargetArch::Riscv64 => riscv64::apply(&mut image, &pending)?,
        TargetArch::LoongArch64 => loongarch64::apply(&mut image, &pending)?,
    }

    let mut runtime_relocations = Vec::new();
    for relocation in pending.iter().filter(|relocation| relocation.kind == 2) {
        apply_absolute_64(&mut image, relocation, &mut runtime_relocations)?;
    }
    runtime_relocations
        .sort_by_key(|relocation| (relocation.target_segment_index, relocation.target_offset));
    let mut targets = BTreeSet::new();
    if runtime_relocations.iter().any(|relocation| {
        !targets.insert((relocation.target_segment_index, relocation.target_offset))
    }) {
        return Err(LinkError::new(
            LinkErrorKind::InvalidRelocation,
            "多个 runtime relocation 写入同一目标",
        ));
    }

    Ok(LinkedImage {
        target_arch: image.target_arch,
        entry_offset: image.entry_offset,
        image_virtual_size: image.image_virtual_size,
        segments: image.segments,
        symbols: image.symbols,
        runtime_relocations,
        runtime_arrays: image.runtime_arrays,
    })
}

fn apply_absolute_64(
    image: &mut LinkImage,
    relocation: &PendingRelocation,
    output: &mut Vec<Relocation>,
) -> Result<(), LinkError> {
    let segment = image
        .segments
        .get_mut(relocation.target_segment_index)
        .ok_or_else(|| relocation_error(relocation, "R_*_64 目标段不存在"))?;
    if !matches!(
        segment.kind,
        SegmentKind::Rodata | SegmentKind::Data | SegmentKind::Bss
    ) || relocation.target_offset % 8 != 0
        || relocation
            .target_offset
            .checked_add(8)
            .is_none_or(|end| end > segment.memory_size)
    {
        return Err(relocation_error(relocation, "R_*_64 目标范围无效"));
    }
    match relocation.symbol_value {
        SymbolValue::Image(symbol) => {
            let addend = add_signed(symbol, relocation.addend)
                .and_then(|value| i64::try_from(value).ok())
                .ok_or_else(|| relocation_error(relocation, "IMAGE_BASE64 addend 溢出"))?;
            clear_payload_target(segment.payload_mut(), relocation)?;
            output.push(Relocation {
                kind: RelocationKind::ImageBase64,
                target_segment_index: relocation.target_segment_index as u32,
                target_offset: relocation.target_offset,
                source_segment_index: u32::MAX,
                addend,
            });
        }
        SymbolValue::Absolute(value) => {
            let value = add_signed(value, relocation.addend)
                .ok_or_else(|| relocation_error(relocation, "绝对符号加法溢出"))?;
            let offset = usize::try_from(relocation.target_offset)
                .map_err(|_| relocation_error(relocation, "绝对 relocation 偏移溢出"))?;
            let target = segment
                .payload_mut()
                .get_mut(offset..offset + 8)
                .ok_or_else(|| relocation_error(relocation, "绝对 relocation 不在文件承载范围"))?;
            target.copy_from_slice(&value.to_le_bytes());
        }
        SymbolValue::Tls(_) => {
            return Err(relocation_error(
                relocation,
                "SOYO runtime relocation 不允许引用 TLS",
            ));
        }
    }
    Ok(())
}

fn clear_payload_target(
    payload: &mut [u8],
    relocation: &PendingRelocation,
) -> Result<(), LinkError> {
    if payload.is_empty() {
        return Ok(());
    }
    let offset = usize::try_from(relocation.target_offset)
        .map_err(|_| relocation_error(relocation, "runtime relocation 偏移溢出"))?;
    let target = payload
        .get_mut(offset..offset + 8)
        .ok_or_else(|| relocation_error(relocation, "runtime relocation 不在文件承载范围"))?;
    target.fill(0);
    Ok(())
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
