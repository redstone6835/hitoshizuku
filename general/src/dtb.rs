//! 兼容旧调用路径的 FDT 类型重导出。
//!
//! FDT 二进制格式、树遍历和属性解码均由独立 [`fdt`] crate 实现。
//! 新代码应直接依赖该 crate；本模块仅避免一次性破坏既有内核接口路径。

pub use fdt::{
    AddressError, AddressRange, Cells, ChosenStdout, Error as DtbError, Fdt as Dtb,
    Header as DtbHeader, Node as DtbNode, NodeId, NodeStatus, Properties as DtbProperties,
    Property as DtbProperty, PropertyError as DtbPropertyError, RangeMapping, RegEntry,
    Reservations as DtbReserveEntries, ReserveEntry as DtbReserveEntry, StringList,
    Tree as DtbTree, TreeError as DtbTreeError,
};
