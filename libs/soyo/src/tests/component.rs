use alloc::vec;
use alloc::vec::Vec;

use native_abi::TargetArch;

use crate::component::{
    ComponentGraphError, ComponentGraphIdentity, ComponentGraphNode, plan_component_graph,
    validate_component_metadata,
};
use crate::metadata::{
    ComponentDependency, ComponentInfo, ComponentMetadata, DynamicRelocation, ImageSegment,
    SymbolExport, SymbolImport,
};
use crate::registry::ArtifactKind;
use crate::registry::{DynamicRelocationKind, SegmentKind, SegmentPermissions};
use crate::{
    SliceSoyoReader, SoyoReadLimits, SoyoTargetPolicy, read_soyo, validate_component_soyo,
};

use super::fixtures::minimal_component_soyo;

fn code_segment() -> ImageSegment {
    ImageSegment {
        kind: SegmentKind::Code,
        permissions: (SegmentPermissions::READ | SegmentPermissions::EXECUTE).bits(),
        virtual_offset: 0,
        file_offset: 0x1000,
        file_size: 0x400,
        memory_size: 0x1000,
        alignment: 0x1000,
    }
}

fn data_segment() -> ImageSegment {
    ImageSegment {
        kind: SegmentKind::Data,
        permissions: (SegmentPermissions::READ | SegmentPermissions::WRITE).bits(),
        virtual_offset: 0x1000,
        file_offset: 0x2000,
        file_size: 0x100,
        memory_size: 0x1000,
        alignment: 0x1000,
    }
}

fn valid_component() -> ComponentMetadata {
    ComponentMetadata {
        info: ComponentInfo {
            component_id: [1; 16],
            abi_id: [2; 16],
            flags: 0,
            init_offset: 0x40,
            fini_offset: 0x80,
            interface_count: 1,
            call_state_size: 0x1000,
        },
        dependencies: Vec::new(),
        symbol_imports: Vec::new(),
        symbol_exports: vec![SymbolExport {
            interface_id: [3; 16],
            symbol_id: [4; 16],
            signature_hash: [5; 32],
            entry_offset: 0xc0,
            flags: 0,
            diagnostic_name_offset: 0,
        }],
        dynamic_relocations: Vec::new(),
        signature: None,
    }
}

fn validate(component: &ComponentMetadata) -> Result<(), crate::SoyoError> {
    validate_component_metadata(
        TargetArch::Riscv64,
        0x2000,
        &[code_segment(), data_segment()],
        &[],
        b"export\0dependency\0import\0",
        component,
    )
}

#[test]
fn valid_component_metadata_is_accepted() {
    assert_eq!(validate(&valid_component()), Ok(()));
}

#[test]
fn zero_component_identity_is_rejected() {
    let mut component = valid_component();
    component.info.component_id = [0; 16];
    assert!(validate(&component).is_err());
}

#[test]
fn lifecycle_and_export_entries_must_be_raw_code() {
    let mut component = valid_component();
    component.info.init_offset = 0x1000;
    assert!(validate(&component).is_err());

    let mut component = valid_component();
    component.symbol_exports[0].entry_offset = 0x1000;
    assert!(validate(&component).is_err());
}

#[test]
fn dependencies_and_symbols_must_be_canonical_and_unique() {
    let mut component = valid_component();
    let later = ComponentDependency {
        component_id: [9; 16],
        abi_id: [1; 16],
        content_hash: [7; 32],
        flags: 1,
        diagnostic_name_offset: 7,
    };
    let earlier = ComponentDependency {
        component_id: [8; 16],
        ..later.clone()
    };
    component.dependencies = vec![later, earlier];
    assert!(validate(&component).is_err());

    let mut component = valid_component();
    component
        .symbol_exports
        .push(component.symbol_exports[0].clone());
    assert!(validate(&component).is_err());
}

#[test]
fn dynamic_relocation_requires_writable_target_and_matching_source_table() {
    let mut component = valid_component();
    component.dynamic_relocations.push(DynamicRelocation {
        kind: DynamicRelocationKind::InterfaceGate,
        target_segment_index: 0,
        target_offset: 0,
        source_index: 0,
        addend: 0,
    });
    assert!(validate(&component).is_err());

    let mut component = valid_component();
    component.symbol_imports.push(SymbolImport {
        dependency_index: 0,
        flags: 1,
        interface_id: [6; 16],
        symbol_id: [7; 16],
        signature_hash: [8; 32],
        diagnostic_name_offset: 18,
    });
    component.dynamic_relocations.push(DynamicRelocation {
        kind: DynamicRelocationKind::InterfaceGate,
        target_segment_index: 1,
        target_offset: 0,
        source_index: 1,
        addend: 0,
    });
    assert!(validate(&component).is_err());
}

#[test]
fn parser_selects_the_shared_component_table_contract() {
    let bytes = minimal_component_soyo();
    let metadata = read_soyo(&SliceSoyoReader::new(&bytes), SoyoReadLimits::portable())
        .expect("canonical shared component 应解析成功");

    assert_eq!(metadata.header.artifact_kind, ArtifactKind::SharedComponent);
    assert!(metadata.runtime.is_none());
    assert!(metadata.capabilities.is_empty());
    assert!(metadata.imports.is_empty());
    let component = metadata
        .component
        .expect("shared component 必须携带组件 metadata");
    assert_eq!(component.info.component_id, [1; 16]);
    assert_eq!(component.info.abi_id, [2; 16]);
    assert_eq!(component.symbol_exports.len(), 1);
    assert_eq!(component.symbol_exports[0].entry_offset, 0);
}

#[test]
fn component_preflight_binds_native_imports_without_process_runtime() {
    let bytes = minimal_component_soyo();
    let metadata = read_soyo(&SliceSoyoReader::new(&bytes), SoyoReadLimits::portable()).unwrap();
    let plan =
        validate_component_soyo(&metadata, SoyoTargetPolicy::for_kernel(TargetArch::Riscv64))
            .expect("合法 shared component 应完成 preflight");

    assert!(plan.native_binding.call_slots.is_empty());
    assert_eq!(plan.enabled_features, 0);
    assert!(plan.metadata.runtime.is_none());
}

fn graph_identity(component: u8, build: u8, content: u8) -> ComponentGraphIdentity {
    ComponentGraphIdentity {
        component_id: [component; 16],
        abi_id: [0xa; 16],
        build_id: [build; 32],
        content_hash: [content; 32],
    }
}

fn graph_dependency(component: u8, content_hash: Option<u8>) -> ComponentDependency {
    ComponentDependency {
        component_id: [component; 16],
        abi_id: [0xa; 16],
        content_hash: content_hash.map_or([0; 32], |value| [value; 32]),
        flags: u32::from(content_hash.is_some()),
        diagnostic_name_offset: 0,
    }
}

#[test]
fn component_graph_merges_exact_duplicate_images() {
    let root_dependencies = [graph_dependency(2, Some(7))];
    let nodes = [
        ComponentGraphNode {
            identity: graph_identity(1, 1, 1),
            dependencies: &root_dependencies,
        },
        ComponentGraphNode {
            identity: graph_identity(2, 2, 7),
            dependencies: &[],
        },
        ComponentGraphNode {
            identity: graph_identity(2, 2, 7),
            dependencies: &[],
        },
    ];

    let plan = plan_component_graph(&nodes).expect("完全相同的组件映像应合并为单一实例");
    assert_eq!(plan.representatives, vec![0, 1]);
    assert_eq!(plan.input_nodes, vec![0, 1, 1]);
    assert_eq!(plan.dependencies, vec![vec![1], vec![]]);
    assert_eq!(plan.topological_order, vec![1, 0]);
}

#[test]
fn component_graph_merges_diamond_dependency() {
    let root_dependencies = [graph_dependency(2, Some(2)), graph_dependency(3, Some(3))];
    let left_dependencies = [graph_dependency(4, Some(4))];
    let right_dependencies = [graph_dependency(4, Some(4))];
    let nodes = [
        ComponentGraphNode {
            identity: graph_identity(1, 1, 1),
            dependencies: &root_dependencies,
        },
        ComponentGraphNode {
            identity: graph_identity(2, 2, 2),
            dependencies: &left_dependencies,
        },
        ComponentGraphNode {
            identity: graph_identity(3, 3, 3),
            dependencies: &right_dependencies,
        },
        ComponentGraphNode {
            identity: graph_identity(4, 4, 4),
            dependencies: &[],
        },
        ComponentGraphNode {
            identity: graph_identity(4, 4, 4),
            dependencies: &[],
        },
    ];

    let plan = plan_component_graph(&nodes).expect("菱形依赖应共享叶组件");
    assert_eq!(plan.representatives, vec![0, 1, 2, 3]);
    assert_eq!(
        plan.dependencies,
        vec![vec![1, 2], vec![3], vec![3], vec![]]
    );
    assert_eq!(plan.topological_order, vec![3, 1, 2, 0]);
}

#[test]
fn component_graph_rejects_ambiguous_unpinned_dependency() {
    let root_dependencies = [graph_dependency(2, None)];
    let nodes = [
        ComponentGraphNode {
            identity: graph_identity(1, 1, 1),
            dependencies: &root_dependencies,
        },
        ComponentGraphNode {
            identity: graph_identity(2, 2, 2),
            dependencies: &[],
        },
        ComponentGraphNode {
            identity: graph_identity(2, 3, 3),
            dependencies: &[],
        },
    ];

    assert_eq!(
        plan_component_graph(&nodes),
        Err(ComponentGraphError::Conflict)
    );
}

#[test]
fn component_graph_rejects_dependency_cycle() {
    let first_dependencies = [graph_dependency(2, Some(2))];
    let second_dependencies = [graph_dependency(1, Some(1))];
    let nodes = [
        ComponentGraphNode {
            identity: graph_identity(1, 1, 1),
            dependencies: &first_dependencies,
        },
        ComponentGraphNode {
            identity: graph_identity(2, 2, 2),
            dependencies: &second_dependencies,
        },
    ];

    assert_eq!(
        plan_component_graph(&nodes),
        Err(ComponentGraphError::Cycle)
    );
}
