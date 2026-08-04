//! Flattened Devicetree（FDT）规范解析器。
//!
//! 默认构建仅依赖 `core`，在 [`Fdt::parse`] 时完整验证二进制布局与结构，
//! 随后的借用视图和迭代器因而不会把格式错误伪装成遍历结束。启用 `alloc`
//! feature 后，[`Tree`] 提供稳定节点编号、alias/phandle 索引和普通总线地址翻译。

#![no_std]

#[cfg(feature = "alloc")]
extern crate alloc;

#[cfg(test)]
extern crate std;

mod error;
mod flat;
mod property;

#[cfg(feature = "alloc")]
mod interrupt;

#[cfg(feature = "alloc")]
mod memory;

#[cfg(feature = "alloc")]
mod msi;

#[cfg(feature = "alloc")]
mod owned;

#[cfg(feature = "alloc")]
mod overlay;

#[cfg(feature = "alloc")]
mod pci;

#[cfg(feature = "alloc")]
mod specifier;

#[cfg(feature = "alloc")]
mod tree;

#[cfg(feature = "alloc")]
mod wide;

pub use error::{ChosenError, Error, PropertyError};
pub use flat::{
    DTB_MAGIC, Fdt, FlatChosenStdout, Header, Node, Nodes, Properties, Property, Reservations,
    ReserveEntry,
};
pub use property::{Cells, StringList, decode_cells};

#[cfg(feature = "alloc")]
pub use interrupt::{InterruptError, InterruptSpecifier};

#[cfg(feature = "alloc")]
pub use memory::{
    MemoryBank, MemoryDescription, MemoryError, PhysicalRange, ReservedMemory,
    ReservedMemoryPlacement,
};

#[cfg(feature = "alloc")]
pub use msi::{MsiError, MsiParent};

#[cfg(feature = "alloc")]
pub use owned::{OwnedNode, OwnedProperty, OwnedTree, OwnedTreeError};

#[cfg(feature = "alloc")]
pub use overlay::OverlayError;

#[cfg(feature = "alloc")]
pub use pci::{
    PciAddressSpace, PciError, PciInterruptMap, PciInterruptMapEntry, PciMsiMap, PciMsiMapEntry,
    PciRange,
};

#[cfg(feature = "alloc")]
pub use specifier::{IdMap, IdMapEntry, IdMapError, NamedPhandleArgs, PhandleArgs, SpecifierError};

#[cfg(feature = "alloc")]
pub use tree::{
    AddressError, AddressRange, ChosenStdout, NodeId, NodeStatus, RangeMapping, RegEntry, Tree,
    TreeError,
};

#[cfg(feature = "alloc")]
pub use wide::{CellAddressError, CellAddressRange, CellRangeMapping, CellRegEntry, CellValue};

#[cfg(test)]
mod tests;
