//! 可修改且可重新序列化的设备树。

use alloc::{
    collections::BTreeMap,
    string::{String, ToString},
    vec,
    vec::Vec,
};
use core::fmt;

use crate::{DTB_MAGIC, Error, Fdt, ReserveEntry, Tree, TreeError};

const FDT_BEGIN_NODE: u32 = 1;
const FDT_END_NODE: u32 = 2;
const FDT_PROP: u32 = 3;
const FDT_END: u32 = 9;
const HEADER_SIZE: usize = 40;

/// owned 节点属性。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnedProperty {
    pub name: String,
    pub value: Vec<u8>,
}

/// owned 设备树节点。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnedNode {
    pub name: String,
    pub properties: Vec<OwnedProperty>,
    pub children: Vec<OwnedNode>,
}

impl OwnedNode {
    /// 构造空节点。
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            properties: Vec::new(),
            children: Vec::new(),
        }
    }

    /// 查找属性。
    pub fn property(&self, name: &str) -> Option<&[u8]> {
        self.properties
            .iter()
            .find(|property| property.name == name)
            .map(|property| property.value.as_slice())
    }

    /// 可修改地查找属性。
    pub fn property_mut(&mut self, name: &str) -> Option<&mut Vec<u8>> {
        self.properties
            .iter_mut()
            .find(|property| property.name == name)
            .map(|property| &mut property.value)
    }

    /// 按原位置替换属性；新属性追加到已有属性之后、子节点之前。
    pub fn set_property(&mut self, name: impl Into<String>, value: Vec<u8>) {
        let name = name.into();
        if let Some(property) = self
            .properties
            .iter_mut()
            .find(|property| property.name == name)
        {
            property.value = value;
        } else {
            self.properties.push(OwnedProperty { name, value });
        }
    }

    /// 删除属性。
    pub fn remove_property(&mut self, name: &str) -> Option<OwnedProperty> {
        let index = self
            .properties
            .iter()
            .position(|property| property.name == name)?;
        Some(self.properties.remove(index))
    }

    /// 查找直接子节点。
    pub fn child(&self, name: &str) -> Option<&OwnedNode> {
        self.children.iter().find(|child| child.name == name)
    }

    /// 可修改地查找直接子节点。
    pub fn child_mut(&mut self, name: &str) -> Option<&mut OwnedNode> {
        self.children.iter_mut().find(|child| child.name == name)
    }

    /// 合并属性和子节点；同名属性替换，同名子节点递归合并。
    pub fn merge_from(&mut self, overlay: &OwnedNode) {
        for property in &overlay.properties {
            self.set_property(property.name.clone(), property.value.clone());
        }
        for child in &overlay.children {
            if let Some(existing) = self.child_mut(&child.name) {
                existing.merge_from(child);
            } else {
                self.children.push(child.clone());
            }
        }
    }
}

/// 与输入 blob 生命周期无关的完整设备树。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnedTree {
    pub root: OwnedNode,
    pub reservations: Vec<ReserveEntry>,
    pub boot_cpuid_phys: Option<u32>,
}

/// owned tree 构造或序列化错误。
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum OwnedTreeError {
    InvalidTree(TreeError),
    InvalidHierarchy,
    DuplicateRoot,
    InvalidOutput(Error),
    SizeOverflow,
}

impl fmt::Display for OwnedTreeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "owned FDT error: {self:?}")
    }
}

impl From<TreeError> for OwnedTreeError {
    fn from(error: TreeError) -> Self {
        Self::InvalidTree(error)
    }
}

impl OwnedTree {
    /// 深复制一份已校验 FDT。
    pub fn from_fdt(fdt: Fdt<'_>) -> Result<Self, OwnedTreeError> {
        // 先建立规范索引，以拒绝同级重名、重复属性和 phandle 冲突。
        Tree::from_fdt(fdt)?;

        let mut stack: Vec<OwnedNode> = Vec::new();
        let mut root = None;
        for node in fdt.nodes() {
            let depth = node.depth();
            while stack.len() > depth {
                finish_node(&mut stack, &mut root)?;
            }
            if stack.len() != depth {
                return Err(OwnedTreeError::InvalidHierarchy);
            }
            let properties = node
                .properties()
                .map(|property| OwnedProperty {
                    name: property.name().to_string(),
                    value: property.value().to_vec(),
                })
                .collect();
            stack.push(OwnedNode {
                name: node.name().to_string(),
                properties,
                children: Vec::new(),
            });
        }
        while !stack.is_empty() {
            finish_node(&mut stack, &mut root)?;
        }
        Ok(Self {
            root: root.ok_or(OwnedTreeError::InvalidHierarchy)?,
            reservations: fdt.reservations().collect(),
            boot_cpuid_phys: fdt.header().boot_cpuid_phys,
        })
    }

    /// 校验并深复制原始 blob。
    pub fn parse(bytes: &[u8]) -> Result<Self, OwnedTreeError> {
        let fdt = Fdt::parse(bytes)
            .map_err(TreeError::InvalidFdt)
            .map_err(OwnedTreeError::InvalidTree)?;
        Self::from_fdt(fdt)
    }

    /// 按绝对路径查找节点。
    pub fn find_node(&self, path: &str) -> Option<&OwnedNode> {
        let components = path_components(path)?;
        find_owned_node(&self.root, &components)
    }

    /// 可修改地按绝对路径查找节点。
    pub fn find_node_mut(&mut self, path: &str) -> Option<&mut OwnedNode> {
        let components = path_components(path)?;
        find_owned_node_mut(&mut self.root, &components)
    }

    /// 生成规范 v17 DTB，并用严格借用解析器复核输出。
    pub fn to_dtb(&self) -> Result<Vec<u8>, OwnedTreeError> {
        let mut structure = Vec::new();
        let mut strings = Vec::new();
        let mut string_offsets: BTreeMap<&str, u32> = BTreeMap::new();
        let mut events = vec![SerializeEvent::Begin(&self.root)];
        while let Some(event) = events.pop() {
            match event {
                SerializeEvent::Begin(node) => {
                    push_u32(&mut structure, FDT_BEGIN_NODE);
                    structure.extend_from_slice(node.name.as_bytes());
                    structure.push(0);
                    pad(&mut structure, 4);
                    for property in &node.properties {
                        let name_offset = match string_offsets.get(property.name.as_str()) {
                            Some(&offset) => offset,
                            None => {
                                let offset = u32::try_from(strings.len())
                                    .map_err(|_| OwnedTreeError::SizeOverflow)?;
                                strings.extend_from_slice(property.name.as_bytes());
                                strings.push(0);
                                string_offsets.insert(property.name.as_str(), offset);
                                offset
                            }
                        };
                        push_u32(&mut structure, FDT_PROP);
                        push_u32(
                            &mut structure,
                            u32::try_from(property.value.len())
                                .map_err(|_| OwnedTreeError::SizeOverflow)?,
                        );
                        push_u32(&mut structure, name_offset);
                        structure.extend_from_slice(&property.value);
                        pad(&mut structure, 4);
                    }
                    events.push(SerializeEvent::End);
                    for child in node.children.iter().rev() {
                        events.push(SerializeEvent::Begin(child));
                    }
                }
                SerializeEvent::End => push_u32(&mut structure, FDT_END_NODE),
            }
        }
        push_u32(&mut structure, FDT_END);

        let mut blob = vec![0; HEADER_SIZE];
        pad(&mut blob, 8);
        let reserve_offset = blob.len();
        for reservation in &self.reservations {
            blob.extend_from_slice(&reservation.address.to_be_bytes());
            blob.extend_from_slice(&reservation.size.to_be_bytes());
        }
        blob.extend_from_slice(&0u64.to_be_bytes());
        blob.extend_from_slice(&0u64.to_be_bytes());
        pad(&mut blob, 4);
        let structure_offset = blob.len();
        blob.extend_from_slice(&structure);
        let strings_offset = blob.len();
        blob.extend_from_slice(&strings);

        let total_size = blob.len();
        write_header_u32(&mut blob, 0, DTB_MAGIC)?;
        write_header_u32(&mut blob, 4, to_u32(total_size)?)?;
        write_header_u32(&mut blob, 8, to_u32(structure_offset)?)?;
        write_header_u32(&mut blob, 12, to_u32(strings_offset)?)?;
        write_header_u32(&mut blob, 16, to_u32(reserve_offset)?)?;
        write_header_u32(&mut blob, 20, 17)?;
        write_header_u32(&mut blob, 24, 16)?;
        write_header_u32(&mut blob, 28, self.boot_cpuid_phys.unwrap_or(0))?;
        write_header_u32(&mut blob, 32, to_u32(strings.len())?)?;
        write_header_u32(&mut blob, 36, to_u32(structure.len())?)?;

        let parsed = Fdt::parse_strict(&blob).map_err(OwnedTreeError::InvalidOutput)?;
        Tree::from_fdt(parsed).map_err(OwnedTreeError::InvalidTree)?;
        Ok(blob)
    }
}

enum SerializeEvent<'a> {
    Begin(&'a OwnedNode),
    End,
}

fn finish_node(
    stack: &mut Vec<OwnedNode>,
    root: &mut Option<OwnedNode>,
) -> Result<(), OwnedTreeError> {
    let node = stack.pop().ok_or(OwnedTreeError::InvalidHierarchy)?;
    if let Some(parent) = stack.last_mut() {
        parent.children.push(node);
    } else if root.replace(node).is_some() {
        return Err(OwnedTreeError::DuplicateRoot);
    }
    Ok(())
}

fn path_components(path: &str) -> Option<Vec<&str>> {
    if path == "/" {
        return Some(Vec::new());
    }
    let relative = path.strip_prefix('/')?;
    if relative.is_empty() || relative.ends_with('/') {
        return None;
    }
    let components = relative.split('/').collect::<Vec<_>>();
    (!components.iter().any(|component| component.is_empty())).then_some(components)
}

fn find_owned_node<'a>(mut node: &'a OwnedNode, components: &[&str]) -> Option<&'a OwnedNode> {
    for component in components {
        node = node.child(component)?;
    }
    Some(node)
}

fn find_owned_node_mut<'a>(
    node: &'a mut OwnedNode,
    components: &[&str],
) -> Option<&'a mut OwnedNode> {
    let Some((component, remaining)) = components.split_first() else {
        return Some(node);
    };
    let child = node.child_mut(component)?;
    find_owned_node_mut(child, remaining)
}

fn push_u32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_be_bytes());
}

fn pad(bytes: &mut Vec<u8>, alignment: usize) {
    while !bytes.len().is_multiple_of(alignment) {
        bytes.push(0);
    }
}

fn to_u32(value: usize) -> Result<u32, OwnedTreeError> {
    u32::try_from(value).map_err(|_| OwnedTreeError::SizeOverflow)
}

fn write_header_u32(bytes: &mut [u8], offset: usize, value: u32) -> Result<(), OwnedTreeError> {
    let end = offset.checked_add(4).ok_or(OwnedTreeError::SizeOverflow)?;
    let target = bytes
        .get_mut(offset..end)
        .ok_or(OwnedTreeError::SizeOverflow)?;
    target.copy_from_slice(&value.to_be_bytes());
    Ok(())
}
