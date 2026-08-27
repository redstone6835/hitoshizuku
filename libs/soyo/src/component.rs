//! SOYO shared component 记录之间的纯格式约束与依赖图规划。

use alloc::vec::Vec;

use native_abi::TargetArch;

use crate::error::{MalformedKind, SoyoError};
use crate::format::validate_string_reference;
use crate::metadata::{AbiImport, ComponentMetadata, ImageSegment};
use crate::registry::{DynamicRelocationKind, PAGE_SIZE, SegmentKind, SegmentPermissions};

const DEPENDENCY_HASH_REQUIRED: u32 = 1;
const SYMBOL_REQUIRED: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComponentGraphIdentity {
    pub component_id: [u8; 16],
    pub abi_id: [u8; 16],
    pub build_id: [u8; 32],
    pub content_hash: [u8; 32],
}

#[derive(Debug, Clone, Copy)]
pub struct ComponentGraphNode<'a> {
    pub identity: ComponentGraphIdentity,
    pub dependencies: &'a [crate::metadata::ComponentDependency],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentGraphPlan {
    /// 每个唯一节点第一次出现时对应的输入索引。
    pub representatives: Vec<usize>,
    /// 每个输入映像对应的唯一节点索引。
    pub input_nodes: Vec<usize>,
    /// 每个唯一节点依赖的唯一节点索引。
    pub dependencies: Vec<Vec<usize>>,
    /// 依赖先于使用者的确定性遍历顺序。
    pub topological_order: Vec<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComponentGraphError {
    Missing,
    Conflict,
    Cycle,
    ResourceExhausted,
}

pub fn plan_component_graph(
    inputs: &[ComponentGraphNode<'_>],
) -> Result<ComponentGraphPlan, ComponentGraphError> {
    let mut representatives: Vec<usize> = Vec::new();
    representatives
        .try_reserve_exact(inputs.len())
        .map_err(|_| ComponentGraphError::ResourceExhausted)?;
    let mut input_nodes = Vec::new();
    input_nodes
        .try_reserve_exact(inputs.len())
        .map_err(|_| ComponentGraphError::ResourceExhausted)?;

    for (input_index, input) in inputs.iter().enumerate() {
        let canonical = representatives
            .iter()
            .position(|representative| inputs[*representative].identity == input.identity);
        match canonical {
            Some(index) => input_nodes.push(index),
            None => {
                input_nodes.push(representatives.len());
                representatives.push(input_index);
            }
        }
    }

    let mut dependencies = Vec::new();
    dependencies
        .try_reserve_exact(representatives.len())
        .map_err(|_| ComponentGraphError::ResourceExhausted)?;
    for representative in &representatives {
        let input = &inputs[*representative];
        let mut resolved = Vec::new();
        resolved
            .try_reserve_exact(input.dependencies.len())
            .map_err(|_| ComponentGraphError::ResourceExhausted)?;
        for dependency in input.dependencies {
            let mut matched = None;
            for (node_index, candidate_input) in representatives.iter().enumerate() {
                let candidate = inputs[*candidate_input].identity;
                if candidate.component_id != dependency.component_id
                    || candidate.abi_id != dependency.abi_id
                    || dependency.flags & DEPENDENCY_HASH_REQUIRED != 0
                        && candidate.content_hash != dependency.content_hash
                {
                    continue;
                }
                if matched.replace(node_index).is_some() {
                    return Err(ComponentGraphError::Conflict);
                }
            }
            resolved.push(matched.ok_or(ComponentGraphError::Missing)?);
        }
        dependencies.push(resolved);
    }

    fn visit(
        index: usize,
        dependencies: &[Vec<usize>],
        marks: &mut [u8],
        order: &mut Vec<usize>,
    ) -> Result<(), ComponentGraphError> {
        match marks[index] {
            1 => return Err(ComponentGraphError::Cycle),
            2 => return Ok(()),
            _ => {}
        }
        marks[index] = 1;
        for dependency in &dependencies[index] {
            visit(*dependency, dependencies, marks, order)?;
        }
        marks[index] = 2;
        order.push(index);
        Ok(())
    }

    let mut marks = Vec::new();
    marks
        .try_reserve_exact(representatives.len())
        .map_err(|_| ComponentGraphError::ResourceExhausted)?;
    marks.resize(representatives.len(), 0);
    let mut topological_order = Vec::new();
    topological_order
        .try_reserve_exact(representatives.len())
        .map_err(|_| ComponentGraphError::ResourceExhausted)?;
    for index in 0..representatives.len() {
        visit(index, &dependencies, &mut marks, &mut topological_order)?;
    }

    Ok(ComponentGraphPlan {
        representatives,
        input_nodes,
        dependencies,
        topological_order,
    })
}

pub fn validate_component_metadata(
    target_arch: TargetArch,
    image_virtual_size: u64,
    segments: &[ImageSegment],
    abi_imports: &[AbiImport],
    strings: &[u8],
    component: &ComponentMetadata,
) -> Result<(), SoyoError> {
    validate_info(target_arch, segments, component)?;
    validate_dependencies(strings, component)?;
    validate_symbol_imports(strings, component)?;
    validate_symbol_exports(target_arch, segments, strings, component)?;
    validate_dynamic_relocations(image_virtual_size, segments, abi_imports, component)?;
    validate_signature(component)?;
    Ok(())
}

fn validate_info(
    target_arch: TargetArch,
    segments: &[ImageSegment],
    component: &ComponentMetadata,
) -> Result<(), SoyoError> {
    let info = &component.info;
    if info.component_id == [0; 16]
        || info.abi_id == [0; 16]
        || info.flags != 0
        || info.call_state_size != PAGE_SIZE
    {
        return Err(SoyoError::Malformed(MalformedKind::Component));
    }
    for entry in [info.init_offset, info.fini_offset] {
        if entry != 0 && !is_raw_code(entry, target_arch, segments) {
            return Err(SoyoError::Malformed(MalformedKind::Component));
        }
    }
    Ok(())
}

fn validate_dependencies(strings: &[u8], component: &ComponentMetadata) -> Result<(), SoyoError> {
    let mut previous = None;
    for dependency in &component.dependencies {
        if dependency.component_id == [0; 16]
            || dependency.abi_id == [0; 16]
            || dependency.flags & !DEPENDENCY_HASH_REQUIRED != 0
            || dependency.flags & DEPENDENCY_HASH_REQUIRED != 0
                && dependency.content_hash == [0; 32]
        {
            return Err(SoyoError::Malformed(MalformedKind::Component));
        }
        validate_string_reference(strings, dependency.diagnostic_name_offset)?;
        let key = (dependency.component_id, dependency.abi_id);
        if previous.is_some_and(|previous| previous >= key) {
            return Err(SoyoError::Malformed(MalformedKind::Ordering));
        }
        previous = Some(key);
    }
    Ok(())
}

fn validate_symbol_imports(strings: &[u8], component: &ComponentMetadata) -> Result<(), SoyoError> {
    let mut previous = None;
    for import in &component.symbol_imports {
        if import.dependency_index as usize >= component.dependencies.len()
            || import.flags & !SYMBOL_REQUIRED != 0
            || import.interface_id == [0; 16]
            || import.symbol_id == [0; 16]
            || import.signature_hash == [0; 32]
        {
            return Err(SoyoError::Malformed(MalformedKind::Symbol));
        }
        validate_string_reference(strings, import.diagnostic_name_offset)?;
        let key = (
            import.dependency_index,
            import.interface_id,
            import.symbol_id,
        );
        if previous.is_some_and(|previous| previous >= key) {
            return Err(SoyoError::Malformed(MalformedKind::Ordering));
        }
        previous = Some(key);
    }
    Ok(())
}

fn validate_symbol_exports(
    target_arch: TargetArch,
    segments: &[ImageSegment],
    strings: &[u8],
    component: &ComponentMetadata,
) -> Result<(), SoyoError> {
    let mut previous = None;
    let mut interface_count = 0u32;
    let mut previous_interface = None;
    for export in &component.symbol_exports {
        if export.flags != 0
            || export.interface_id == [0; 16]
            || export.symbol_id == [0; 16]
            || export.signature_hash == [0; 32]
            || !is_raw_code(export.entry_offset, target_arch, segments)
        {
            return Err(SoyoError::Malformed(MalformedKind::Symbol));
        }
        validate_string_reference(strings, export.diagnostic_name_offset)?;
        let key = (export.interface_id, export.symbol_id);
        if previous.is_some_and(|previous| previous >= key) {
            return Err(SoyoError::Malformed(MalformedKind::Ordering));
        }
        if previous_interface != Some(export.interface_id) {
            interface_count = interface_count
                .checked_add(1)
                .ok_or(SoyoError::Malformed(MalformedKind::Range))?;
            previous_interface = Some(export.interface_id);
        }
        previous = Some(key);
    }
    if component.symbol_exports.is_empty() || component.info.interface_count != interface_count {
        return Err(SoyoError::Malformed(MalformedKind::Component));
    }
    Ok(())
}

fn validate_dynamic_relocations(
    image_virtual_size: u64,
    segments: &[ImageSegment],
    abi_imports: &[AbiImport],
    component: &ComponentMetadata,
) -> Result<(), SoyoError> {
    let mut previous = None;
    for relocation in &component.dynamic_relocations {
        let segment = segments
            .get(relocation.target_segment_index as usize)
            .ok_or(SoyoError::Malformed(MalformedKind::Relocation))?;
        let (width, alignment) = match relocation.kind {
            DynamicRelocationKind::AbiSlot32 => (4, 4),
            DynamicRelocationKind::AbiSlot64 | DynamicRelocationKind::TlsOffset64 => (8, 8),
            DynamicRelocationKind::InterfaceGate => (32, 8),
        };
        if segment.permissions & SegmentPermissions::WRITE.bits() == 0
            || segment.permissions & SegmentPermissions::EXECUTE.bits() != 0
            || relocation.target_offset % alignment != 0
            || relocation
                .target_offset
                .checked_add(width)
                .is_none_or(|end| end > segment.memory_size)
            || segment
                .virtual_offset
                .checked_add(relocation.target_offset)
                .is_none_or(|target| target >= image_virtual_size)
        {
            return Err(SoyoError::Malformed(MalformedKind::Relocation));
        }
        let source_valid = match relocation.kind {
            DynamicRelocationKind::AbiSlot32 | DynamicRelocationKind::AbiSlot64 => {
                (relocation.source_index as usize) < abi_imports.len()
            }
            DynamicRelocationKind::InterfaceGate => {
                relocation.addend == 0
                    && (relocation.source_index as usize) < component.symbol_imports.len()
            }
            DynamicRelocationKind::TlsOffset64 => segments
                .get(relocation.source_index as usize)
                .is_some_and(|source| source.kind == SegmentKind::TlsTemplate),
        };
        if !source_valid {
            return Err(SoyoError::Malformed(MalformedKind::Relocation));
        }
        let key = (relocation.target_segment_index, relocation.target_offset);
        if previous.is_some_and(|previous| previous >= key) {
            return Err(SoyoError::Malformed(MalformedKind::Ordering));
        }
        previous = Some(key);
    }
    Ok(())
}

fn validate_signature(component: &ComponentMetadata) -> Result<(), SoyoError> {
    if component.signature.as_ref().is_some_and(|signature| {
        signature.key_id == [0; 32] || signature.signature == [0; 64] || signature.flags != 0
    }) {
        return Err(SoyoError::Malformed(MalformedKind::Signature));
    }
    Ok(())
}

fn is_raw_code(entry: u64, target_arch: TargetArch, segments: &[ImageSegment]) -> bool {
    let alignment = target_arch.instruction_alignment();
    entry % alignment == 0
        && segments.iter().any(|segment| {
            segment.kind == SegmentKind::Code
                && entry >= segment.virtual_offset
                && entry < segment.virtual_offset.saturating_add(segment.file_size)
        })
}
