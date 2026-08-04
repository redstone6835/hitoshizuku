//! dtc/Linux 设备树 overlay 的原子应用。

use alloc::{
    string::{String, ToString},
    vec,
    vec::Vec,
};
use core::{fmt, str};

use crate::{Fdt, OwnedNode, OwnedProperty, OwnedTree, OwnedTreeError};

const PHANDLE_MAX: u32 = u32::MAX - 1;

/// overlay 解析、重定位或合并错误。
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum OverlayError {
    InvalidOverlay(OwnedTreeError),
    InvalidProperty { path: String, property: String },
    MissingProperty { path: String, property: String },
    MissingNode(String),
    MissingSymbols,
    UnknownSymbol(String),
    MissingPhandle(String),
    PhandleOverflow,
    InvalidFixup(String),
    InvalidFragment(String),
    DuplicateSymbol(String),
    InvalidOutput(OwnedTreeError),
}

impl fmt::Display for OverlayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FDT overlay error: {self:?}")
    }
}

impl OwnedTree {
    /// 原子应用一份标准 dtc/Linux overlay。
    ///
    /// 外部 fixup、局部 phandle 重定位、fragment 合并与 symbol 重写全部在副本上
    /// 完成；最终 DTB 重新序列化并通过严格解析后才替换当前树。
    pub fn apply_overlay(&mut self, overlay: Fdt<'_>) -> Result<(), OverlayError> {
        let mut candidate = self.clone();
        let mut overlay = OwnedTree::from_fdt(overlay).map_err(OverlayError::InvalidOverlay)?;

        let delta = maximum_phandle(&candidate.root)?;
        adjust_local_phandles(&mut overlay.root, delta, true)?;
        if let Some(local_fixups) = overlay.root.child("__local_fixups__").cloned() {
            apply_local_fixups(&mut overlay, &local_fixups, delta, "/")?;
        }
        apply_external_fixups(&candidate, &mut overlay)?;

        let mut fragment_targets = Vec::new();
        for fragment in overlay.root.children.clone() {
            if fragment.name == "__fixups__"
                || fragment.name == "__local_fixups__"
                || fragment.name == "__symbols__"
            {
                continue;
            }
            let Some(contents) = fragment.child("__overlay__") else {
                if fragment.name == "fragment" || fragment.name.starts_with("fragment@") {
                    return Err(OverlayError::InvalidFragment(format_path(
                        "/",
                        &fragment.name,
                    )));
                }
                continue;
            };
            let fragment_path = format_path("/", &fragment.name);
            let target_path = fragment_target_path(&candidate, &fragment, &fragment_path)?;
            let target = candidate
                .find_node_mut(&target_path)
                .ok_or_else(|| OverlayError::MissingNode(target_path.clone()))?;
            target.merge_from(contents);
            fragment_targets.push((format_path(&fragment_path, "__overlay__"), target_path));
        }

        merge_overlay_symbols(&mut candidate, &overlay, &fragment_targets)?;
        for reservation in overlay.reservations {
            if !candidate.reservations.contains(&reservation) {
                candidate.reservations.push(reservation);
            }
        }
        candidate.to_dtb().map_err(OverlayError::InvalidOutput)?;
        *self = candidate;
        Ok(())
    }

    /// 校验并应用原始 overlay blob。
    pub fn apply_overlay_blob(&mut self, bytes: &[u8]) -> Result<(), OverlayError> {
        let overlay = Fdt::parse(bytes)
            .map_err(|error| OverlayError::InvalidOverlay(OwnedTreeError::InvalidOutput(error)))?;
        self.apply_overlay(overlay)
    }
}

fn maximum_phandle(root: &OwnedNode) -> Result<u32, OverlayError> {
    let mut maximum = 0u32;
    let mut nodes = vec![(root, String::from("/"))];
    while let Some((node, path)) = nodes.pop() {
        if let Some(phandle) = node_phandle(node, &path)? {
            maximum = maximum.max(phandle);
        }
        for child in node.children.iter().rev() {
            nodes.push((child, format_path(&path, &child.name)));
        }
    }
    Ok(maximum)
}

fn adjust_local_phandles(node: &mut OwnedNode, delta: u32, root: bool) -> Result<(), OverlayError> {
    if !root || !is_metadata_node(&node.name) {
        let path = String::from("overlay node");
        for name in ["phandle", "linux,phandle"] {
            if let Some(value) = node.property_mut(name) {
                let phandle = read_u32(value).ok_or_else(|| OverlayError::InvalidProperty {
                    path: path.clone(),
                    property: name.to_string(),
                })?;
                let adjusted = adjusted_phandle(phandle, delta)?;
                value.copy_from_slice(&adjusted.to_be_bytes());
            }
        }
    }
    for child in &mut node.children {
        if root && is_metadata_node(&child.name) {
            continue;
        }
        adjust_local_phandles(child, delta, false)?;
    }
    Ok(())
}

fn apply_local_fixups(
    overlay: &mut OwnedTree,
    fixup: &OwnedNode,
    delta: u32,
    target_path: &str,
) -> Result<(), OverlayError> {
    let target = overlay
        .find_node_mut(target_path)
        .ok_or_else(|| OverlayError::MissingNode(target_path.to_string()))?;
    for property in &fixup.properties {
        let offsets = u32_list(&property.value).ok_or_else(|| OverlayError::InvalidProperty {
            path: target_path.to_string(),
            property: property.name.clone(),
        })?;
        let target_value =
            target
                .property_mut(&property.name)
                .ok_or_else(|| OverlayError::MissingProperty {
                    path: target_path.to_string(),
                    property: property.name.clone(),
                })?;
        for offset in offsets {
            patch_phandle(target_value, offset, delta, true).map_err(|_| {
                OverlayError::InvalidFixup(format_fixup(target_path, &property.name, offset))
            })?;
        }
    }
    for child in &fixup.children {
        let child_path = format_path(target_path, &child.name);
        apply_local_fixups(overlay, child, delta, &child_path)?;
    }
    Ok(())
}

fn apply_external_fixups(base: &OwnedTree, overlay: &mut OwnedTree) -> Result<(), OverlayError> {
    let Some(fixups) = overlay.root.child("__fixups__").cloned() else {
        return Ok(());
    };
    let symbols = base.root.child("__symbols__");
    if !fixups.properties.is_empty() && symbols.is_none() {
        return Err(OverlayError::MissingSymbols);
    }
    for fixup in fixups.properties {
        let symbol_path = symbols
            .and_then(|symbols| symbols.property(&fixup.name))
            .and_then(single_string)
            .ok_or_else(|| OverlayError::UnknownSymbol(fixup.name.clone()))?;
        let target = base
            .find_node(symbol_path)
            .ok_or_else(|| OverlayError::MissingNode(symbol_path.to_string()))?;
        let phandle = node_phandle(target, symbol_path)?
            .ok_or_else(|| OverlayError::MissingPhandle(symbol_path.to_string()))?;
        let locations = string_list(&fixup.value).ok_or_else(|| OverlayError::InvalidProperty {
            path: String::from("/__fixups__"),
            property: fixup.name.clone(),
        })?;
        for location in locations {
            let (path, property, offset) = parse_fixup_location(location)
                .ok_or_else(|| OverlayError::InvalidFixup(location.to_string()))?;
            let node = overlay
                .find_node_mut(path)
                .ok_or_else(|| OverlayError::MissingNode(path.to_string()))?;
            let value =
                node.property_mut(property)
                    .ok_or_else(|| OverlayError::MissingProperty {
                        path: path.to_string(),
                        property: property.to_string(),
                    })?;
            patch_phandle(value, offset, phandle, false)
                .map_err(|_| OverlayError::InvalidFixup(location.to_string()))?;
        }
    }
    Ok(())
}

fn fragment_target_path(
    base: &OwnedTree,
    fragment: &OwnedNode,
    fragment_path: &str,
) -> Result<String, OverlayError> {
    match (
        fragment.property("target"),
        fragment.property("target-path"),
    ) {
        (Some(_), Some(_)) | (None, None) => {
            Err(OverlayError::InvalidFragment(fragment_path.to_string()))
        }
        (Some(value), None) => {
            let phandle = read_u32(value).ok_or_else(|| OverlayError::InvalidProperty {
                path: fragment_path.to_string(),
                property: String::from("target"),
            })?;
            find_path_by_phandle(&base.root, phandle)
                .ok_or_else(|| OverlayError::MissingPhandle(fragment_path.to_string()))
        }
        (None, Some(value)) => {
            let path = single_string(value).ok_or_else(|| OverlayError::InvalidProperty {
                path: fragment_path.to_string(),
                property: String::from("target-path"),
            })?;
            if !path.starts_with('/') {
                return Err(OverlayError::InvalidFragment(fragment_path.to_string()));
            }
            Ok(path.to_string())
        }
    }
}

fn merge_overlay_symbols(
    base: &mut OwnedTree,
    overlay: &OwnedTree,
    fragment_targets: &[(String, String)],
) -> Result<(), OverlayError> {
    let Some(symbols) = overlay.root.child("__symbols__") else {
        return Ok(());
    };
    let mut rewritten = Vec::new();
    for symbol in &symbols.properties {
        let overlay_path =
            single_string(&symbol.value).ok_or_else(|| OverlayError::InvalidProperty {
                path: String::from("/__symbols__"),
                property: symbol.name.clone(),
            })?;
        let path = fragment_targets
            .iter()
            .find_map(|(source, target)| rewrite_symbol_path(overlay_path, source, target))
            .ok_or_else(|| OverlayError::InvalidFixup(overlay_path.to_string()))?;
        if base.find_node(&path).is_none() {
            return Err(OverlayError::MissingNode(path));
        }
        rewritten.push((symbol.name.clone(), nul_string(&path)));
    }

    if base.root.child("__symbols__").is_none() {
        base.root.children.push(OwnedNode::new("__symbols__"));
    }
    let target = base
        .root
        .child_mut("__symbols__")
        .expect("symbols node was inserted");
    for (name, value) in rewritten {
        if let Some(existing) = target.property(&name) {
            if existing != value {
                return Err(OverlayError::DuplicateSymbol(name));
            }
        } else {
            target.properties.push(OwnedProperty { name, value });
        }
    }
    Ok(())
}

fn rewrite_symbol_path(path: &str, source: &str, target: &str) -> Option<String> {
    let suffix = path.strip_prefix(source)?;
    if !suffix.is_empty() && !suffix.starts_with('/') {
        return None;
    }
    if target == "/" {
        Some(if suffix.is_empty() {
            String::from("/")
        } else {
            suffix.to_string()
        })
    } else {
        Some(format_join(target, suffix))
    }
}

fn find_path_by_phandle(root: &OwnedNode, phandle: u32) -> Option<String> {
    let mut nodes = vec![(root, String::from("/"))];
    while let Some((node, path)) = nodes.pop() {
        if node_phandle(node, &path).ok().flatten() == Some(phandle) {
            return Some(path);
        }
        for child in node.children.iter().rev() {
            nodes.push((child, format_path(&path, &child.name)));
        }
    }
    None
}

fn node_phandle(node: &OwnedNode, path: &str) -> Result<Option<u32>, OverlayError> {
    let primary = node.property("phandle").map(read_u32).transpose_option();
    let legacy = node
        .property("linux,phandle")
        .map(read_u32)
        .transpose_option();
    let primary = primary.ok_or_else(|| OverlayError::InvalidProperty {
        path: path.to_string(),
        property: String::from("phandle"),
    })?;
    let legacy = legacy.ok_or_else(|| OverlayError::InvalidProperty {
        path: path.to_string(),
        property: String::from("linux,phandle"),
    })?;
    match (primary, legacy) {
        (Some(left), Some(right)) if left != right => Err(OverlayError::InvalidProperty {
            path: path.to_string(),
            property: String::from("linux,phandle"),
        }),
        (Some(value), _) | (_, Some(value)) if value != 0 && value != u32::MAX => Ok(Some(value)),
        (Some(_), _) | (_, Some(_)) => Err(OverlayError::InvalidProperty {
            path: path.to_string(),
            property: String::from("phandle"),
        }),
        (None, None) => Ok(None),
    }
}

trait TransposeOption<T> {
    fn transpose_option(self) -> Option<Option<T>>;
}

impl<T> TransposeOption<T> for Option<Option<T>> {
    fn transpose_option(self) -> Option<Option<T>> {
        match self {
            Some(Some(value)) => Some(Some(value)),
            Some(None) => None,
            None => Some(None),
        }
    }
}

fn adjusted_phandle(phandle: u32, delta: u32) -> Result<u32, OverlayError> {
    if phandle == 0 || phandle == u32::MAX {
        return Err(OverlayError::PhandleOverflow);
    }
    let value = phandle
        .checked_add(delta)
        .filter(|&value| value <= PHANDLE_MAX)
        .ok_or(OverlayError::PhandleOverflow)?;
    Ok(value)
}

fn patch_phandle(value: &mut [u8], offset: u32, adjustment: u32, add: bool) -> Result<(), ()> {
    let offset = usize::try_from(offset).map_err(|_| ())?;
    if !offset.is_multiple_of(4) {
        return Err(());
    }
    let target = value
        .get_mut(offset..offset.checked_add(4).ok_or(())?)
        .ok_or(())?;
    let current = u32::from_be_bytes(target.try_into().map_err(|_| ())?);
    let patched = if add {
        adjusted_phandle(current, adjustment).map_err(|_| ())?
    } else {
        adjustment
    };
    target.copy_from_slice(&patched.to_be_bytes());
    Ok(())
}

fn read_u32(value: &[u8]) -> Option<u32> {
    Some(u32::from_be_bytes(value.try_into().ok()?))
}

fn u32_list(value: &[u8]) -> Option<Vec<u32>> {
    if !value.len().is_multiple_of(4) {
        return None;
    }
    value
        .chunks_exact(4)
        .map(|cell| Some(u32::from_be_bytes(cell.try_into().ok()?)))
        .collect()
}

fn single_string(value: &[u8]) -> Option<&str> {
    let content = value.strip_suffix(&[0])?;
    if content.contains(&0) {
        return None;
    }
    str::from_utf8(content).ok()
}

fn string_list(value: &[u8]) -> Option<Vec<&str>> {
    if value.is_empty() || value.last() != Some(&0) {
        return None;
    }
    value[..value.len() - 1]
        .split(|&byte| byte == 0)
        .map(|entry| str::from_utf8(entry).ok())
        .collect()
}

fn parse_fixup_location(value: &str) -> Option<(&str, &str, u32)> {
    let mut fields = value.rsplitn(3, ':');
    let offset = fields.next()?.parse().ok()?;
    let property = fields.next()?;
    let path = fields.next()?;
    (!property.is_empty() && path.starts_with('/')).then_some((path, property, offset))
}

fn format_fixup(path: &str, property: &str, offset: u32) -> String {
    let mut value = String::from(path);
    value.push(':');
    value.push_str(property);
    value.push(':');
    value.push_str(&offset.to_string());
    value
}

fn is_metadata_node(name: &str) -> bool {
    matches!(name, "__fixups__" | "__local_fixups__" | "__symbols__")
}

fn format_path(parent: &str, child: &str) -> String {
    if parent == "/" {
        let mut path = String::from("/");
        path.push_str(child);
        path
    } else {
        let mut path = String::from(parent);
        path.push('/');
        path.push_str(child);
        path
    }
}

fn format_join(prefix: &str, suffix: &str) -> String {
    let mut result = String::from(prefix);
    result.push_str(suffix);
    result
}

fn nul_string(value: &str) -> Vec<u8> {
    let mut bytes = value.as_bytes().to_vec();
    bytes.push(0);
    bytes
}
