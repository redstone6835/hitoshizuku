//! Device Tree Blob（DTB / FDT）解析。
//!
//! 提供三个核心类型：
//! - [`Dtb`]：DTB 根视图，由字节切片或原始指针构造。
//! - [`DtbNode`]：节点视图，支持遍历子节点与直接属性。
//! - [`DtbProperty`]：属性（键值对）视图，携带名称与原始值字节。

use core::mem::size_of;
use core::str;

/// DTB 头部魔数（大端）：`0xd00dfeed`。
pub const DTB_MAGIC: u32 = 0xd00d_feed;

// ─────────────────────── 内部结构块 token ─────────────────────────────────

const TOK_BEGIN_NODE: u32 = 1;
const TOK_END_NODE: u32 = 2;
const TOK_PROP: u32 = 3;
const TOK_NOP: u32 = 4;
const TOK_END: u32 = 9;

// ─────────────────────────── DTB 头部 ─────────────────────────────────────

/// DTB 头部（字段已转换为本机端序）。
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DtbHeader {
    pub magic: u32,
    pub total_size: u32,
    pub off_dt_struct: u32,
    pub off_dt_strings: u32,
    pub off_mem_rsvmap: u32,
    pub version: u32,
    pub last_comp_version: u32,
    pub boot_cpuid_phys: u32,
    pub size_dt_strings: u32,
    pub size_dt_struct: u32,
}

impl DtbHeader {
    fn read_from(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < size_of::<Self>() {
            return None;
        }
        let r = |i: usize| u32::from_be_bytes(bytes[i..i + 4].try_into().unwrap());
        Some(Self {
            magic: r(0),
            total_size: r(4),
            off_dt_struct: r(8),
            off_dt_strings: r(12),
            off_mem_rsvmap: r(16),
            version: r(20),
            last_comp_version: r(24),
            boot_cpuid_phys: r(28),
            size_dt_strings: r(32),
            size_dt_struct: r(36),
        })
    }

    /// 魔数是否合法。
    #[inline]
    pub fn has_valid_magic(&self) -> bool {
        self.magic == DTB_MAGIC
    }

    /// `total_size` 是否至少覆盖头部本身。
    #[inline]
    pub fn has_reasonable_size(&self) -> bool {
        (self.total_size as usize) >= size_of::<DtbHeader>()
    }
}

// ─────────────────────────── DTB 属性 ─────────────────────────────────────

/// DTB 属性（键值对）视图。
///
/// 名称为字符串，值为原始字节切片（大端序），调用方自行解释。
#[derive(Clone, Copy, Debug)]
pub struct DtbProperty<'a> {
    name_bytes: &'a [u8],
    value: &'a [u8],
}

impl<'a> DtbProperty<'a> {
    /// 属性名的原始字节（不含 NUL 终止符）。
    #[inline]
    pub fn name_bytes(&self) -> &'a [u8] {
        self.name_bytes
    }

    /// 属性名解析为 UTF-8 字符串；若非法 UTF-8 则返回 `None`。
    #[inline]
    pub fn name(&self) -> Option<&'a str> {
        str::from_utf8(self.name_bytes).ok()
    }

    /// 属性值的原始字节切片（大端序）。
    #[inline]
    pub fn value(&self) -> &'a [u8] {
        self.value
    }
}

// ─────────────────────────── DTB 节点 ─────────────────────────────────────

/// DTB 节点视图。
///
/// 节点要么包含子节点（[`DtbNode::children`]），要么包含键值对属性（[`DtbNode::properties`]），
/// 也可以同时拥有两者。
#[derive(Clone, Copy, Debug)]
pub struct DtbNode<'a> {
    name_bytes: &'a [u8],
    /// 该节点内容切片：位于 BEGIN_NODE 名称之后、对应 END_NODE 之前。
    content: &'a [u8],
    strings_block: &'a [u8],
}

impl<'a> DtbNode<'a> {
    /// 节点名的原始字节（含 `@<addr>` 后缀，若有）。
    #[inline]
    pub fn name_bytes(&self) -> &'a [u8] {
        self.name_bytes
    }

    /// 节点名解析为 UTF-8 字符串；若非法 UTF-8 则返回 `None`。
    #[inline]
    pub fn name(&self) -> Option<&'a str> {
        str::from_utf8(self.name_bytes).ok()
    }

    /// `@` 之前的基础名称（不含地址后缀）。
    ///
    /// 例如 `"memory@80000000"` → `"memory"`；无后缀时返回完整名称字节。
    #[inline]
    pub fn base_name_bytes(&self) -> &'a [u8] {
        self.name_bytes
            .iter()
            .position(|&b| b == b'@')
            .map(|i| &self.name_bytes[..i])
            .unwrap_or(self.name_bytes)
    }

    /// 迭代该节点的所有**直接**子节点。
    #[inline]
    pub fn children(&self) -> DtbChildren<'a> {
        DtbChildren::new(self.content, self.strings_block)
    }

    /// 在直接子节点中查找名称匹配的第一个节点。
    ///
    /// `name` 可以是完整名（如 `"memory@80000000"`）或基础名（如 `"memory"`），两者均能匹配。
    pub fn find_child(&self, name: &str) -> Option<DtbNode<'a>> {
        self.children()
            .find(|n| n.name_bytes == name.as_bytes() || n.base_name_bytes() == name.as_bytes())
    }

    /// 迭代该节点的所有**直接**属性（不递归进入子节点）。
    #[inline]
    pub fn properties(&self) -> DtbProperties<'a> {
        DtbProperties::new(self.content, self.strings_block)
    }

    /// 在直接属性中查找名称为 `name` 的属性。
    pub fn find_property(&self, name: &str) -> Option<DtbProperty<'a>> {
        self.properties().find(|p| p.name_bytes == name.as_bytes())
    }
}

// ──────────────────────── 子节点迭代器 ────────────────────────────────────

/// 直接子节点迭代器，由 [`DtbNode::children`] 或 [`Dtb::children`] 创建。
pub struct DtbChildren<'a> {
    sb: &'a [u8],
    strings: &'a [u8],
    cursor: usize,
    done: bool,
}

impl<'a> DtbChildren<'a> {
    fn new(sb: &'a [u8], strings: &'a [u8]) -> Self {
        Self {
            sb,
            strings,
            cursor: 0,
            done: false,
        }
    }

    fn read_u32(&mut self) -> Option<u32> {
        let (v, next) = read_be_u32(self.sb, self.cursor)?;
        self.cursor = next;
        Some(v)
    }
}

impl<'a> Iterator for DtbChildren<'a> {
    type Item = DtbNode<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        loop {
            let tok = self.read_u32()?;
            match tok {
                TOK_BEGIN_NODE => {
                    let name_end = cstring_end(self.sb, self.cursor)?;
                    let name_bytes = self.sb.get(self.cursor..name_end)?;
                    self.cursor = align4(name_end + 1);

                    let content_start = self.cursor;
                    let content_end = skip_node_body(self.sb, &mut self.cursor)?;
                    let content = self.sb.get(content_start..content_end)?;

                    return Some(DtbNode {
                        name_bytes,
                        content,
                        strings_block: self.strings,
                    });
                }
                TOK_END_NODE | TOK_END => {
                    self.done = true;
                    return None;
                }
                TOK_PROP => {
                    // 跳过本层属性（只产出子节点）
                    let prop_len = self.read_u32()? as usize;
                    let _name_off = self.read_u32()?;
                    self.cursor = align4(self.cursor.checked_add(prop_len)?);
                    if self.cursor > self.sb.len() {
                        self.done = true;
                        return None;
                    }
                }
                TOK_NOP => {}
                _ => {
                    self.done = true;
                    return None;
                }
            }
        }
    }
}

// ──────────────────────── 属性迭代器 ──────────────────────────────────────

/// 直接属性迭代器，由 [`DtbNode::properties`] 创建。
///
/// 只产出当前节点自身的属性，遇到子节点时自动跳过其内容。
pub struct DtbProperties<'a> {
    sb: &'a [u8],
    strings: &'a [u8],
    cursor: usize,
    depth: usize,
    done: bool,
}

impl<'a> DtbProperties<'a> {
    fn new(sb: &'a [u8], strings: &'a [u8]) -> Self {
        Self {
            sb,
            strings,
            cursor: 0,
            depth: 0,
            done: false,
        }
    }

    fn read_u32(&mut self) -> Option<u32> {
        let (v, next) = read_be_u32(self.sb, self.cursor)?;
        self.cursor = next;
        Some(v)
    }
}

impl<'a> Iterator for DtbProperties<'a> {
    type Item = DtbProperty<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        loop {
            let tok = self.read_u32()?;
            match tok {
                TOK_BEGIN_NODE => {
                    let name_end = cstring_end(self.sb, self.cursor)?;
                    self.cursor = align4(name_end + 1);
                    self.depth += 1;
                }
                TOK_END_NODE => {
                    if self.depth == 0 {
                        self.done = true;
                        return None;
                    }
                    self.depth -= 1;
                }
                TOK_PROP => {
                    let prop_len = self.read_u32()? as usize;
                    let name_off = self.read_u32()? as usize;
                    let val_end = self.cursor.checked_add(prop_len)?;
                    let value = self.sb.get(self.cursor..val_end)?;
                    self.cursor = align4(val_end);

                    if self.depth == 0 {
                        let name_bytes = cstr_at(self.strings, name_off)?;
                        return Some(DtbProperty { name_bytes, value });
                    }
                    if self.cursor > self.sb.len() {
                        self.done = true;
                        return None;
                    }
                }
                TOK_NOP => {}
                _ => {
                    self.done = true;
                    return None;
                }
            }
        }
    }
}

/// DTB 头部 `memreserve` 表中的一条保留区描述。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DtbReserveEntry {
    /// 保留区物理起始地址。
    pub address: usize,
    /// 保留区长度（字节）。
    pub size: usize,
}

/// DTB `memreserve` 表迭代器。
///
/// 每次产出一条 `(address, size)` 保留区，遇到全零终止项停止。
pub struct DtbReserveEntries<'a> {
    bytes: &'a [u8],
    cursor: usize,
    done: bool,
}

impl<'a> DtbReserveEntries<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            cursor: 0,
            done: false,
        }
    }
}

impl<'a> Iterator for DtbReserveEntries<'a> {
    type Item = DtbReserveEntry;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }

        let (address, next) = read_be_u64(self.bytes, self.cursor)?;
        let (size, next) = read_be_u64(self.bytes, next)?;
        self.cursor = next;

        if address == 0 && size == 0 {
            self.done = true;
            return None;
        }

        if address > usize::MAX as u64 || size > usize::MAX as u64 {
            self.done = true;
            return None;
        }

        Some(DtbReserveEntry {
            address: address as usize,
            size: size as usize,
        })
    }
}

// ─────────────────────────── Dtb ──────────────────────────────────────────

/// DTB 根视图。
///
/// 通过 [`Dtb::from_bytes`] 或 [`Dtb::from_ptr`] 构造。
/// 使用 [`Dtb::children`] / [`Dtb::find_child`] 进行树形导航。
#[derive(Clone, Copy, Debug)]
pub struct Dtb<'a> {
    bytes: &'a [u8],
    header: DtbHeader,
}

impl<'a> Dtb<'a> {
    /// 从字节切片构造 DTB 视图。
    ///
    /// 依次校验魔数、`total_size` 和内部块布局；任一失败返回 `None`。
    pub fn from_bytes(bytes: &'a [u8]) -> Option<Self> {
        let header = DtbHeader::read_from(bytes)?;
        if !header.has_valid_magic() || !header.has_reasonable_size() {
            return None;
        }
        let dtb = Self {
            bytes: bytes.get(..header.total_size as usize)?,
            header,
        };
        dtb.is_layout_valid().then_some(dtb)
    }

    /// 从原始地址构造 DTB 视图。
    ///
    /// # Safety
    ///
    /// `dtb_ptr` 必须指向合法 DTB 内存，且在返回值生命周期内保持有效。
    pub unsafe fn from_ptr(dtb_ptr: usize) -> Option<Dtb<'static>> {
        if dtb_ptr == 0 {
            return None;
        }
        let header = DtbHeader::read_from(unsafe {
            core::slice::from_raw_parts(dtb_ptr as *const u8, size_of::<DtbHeader>())
        })?;
        if !header.has_valid_magic() || !header.has_reasonable_size() {
            return None;
        }
        Dtb::from_bytes(unsafe {
            core::slice::from_raw_parts(dtb_ptr as *const u8, header.total_size as usize)
        })
    }

    /// 返回头部（已转换为本机端序）。
    #[inline]
    pub fn header(&self) -> DtbHeader {
        self.header
    }

    /// 返回完整 DTB 字节切片。
    #[inline]
    pub fn as_bytes(&self) -> &'a [u8] {
        self.bytes
    }

    /// 返回根节点视图。
    ///
    /// 调用方可通过该节点访问根节点属性，例如
    /// `#address-cells` 和 `#size-cells`。
    pub fn root(&self) -> Option<DtbNode<'a>> {
        let sb = self.structure_block()?;
        let strings = self.strings_block()?;
        let mut cursor = 0usize;
        loop {
            let (tok, next) = read_be_u32(sb, cursor)?;
            cursor = next;
            match tok {
                TOK_BEGIN_NODE => {
                    let name_end = cstring_end(sb, cursor)?;
                    let name_bytes = sb.get(cursor..name_end)?;
                    cursor = align4(name_end + 1);

                    let content_start = cursor;
                    let content_end = skip_node_body(sb, &mut cursor)?;
                    let content = sb.get(content_start..content_end)?;

                    return Some(DtbNode {
                        name_bytes,
                        content,
                        strings_block: strings,
                    });
                }
                TOK_NOP => {}
                _ => return None,
            }
        }
    }

    /// 迭代根节点的所有直接子节点。
    ///
    /// 自动跳过根节点自身的 BEGIN_NODE 头，直接产出其子节点。
    /// 若 DTB 布局非法则返回 `None`。
    pub fn children(&self) -> Option<DtbChildren<'a>> {
        Some(self.root()?.children())
    }

    /// 在根节点的直接子节点中查找名称匹配的节点。
    ///
    /// `name` 可以是完整名（如 `"cpus"`）或带地址后缀的节点名（如 `"memory@80000000"`），
    /// 也可以只提供基础名（如 `"memory"`）进行模糊匹配。
    pub fn find_child(&self, name: &str) -> Option<DtbNode<'a>> {
        self.children()?
            .find(|n| n.name_bytes == name.as_bytes() || n.base_name_bytes() == name.as_bytes())
    }

    /// 迭代 DTB 头部 `memreserve` 表中的保留区。
    pub fn mem_reservations(&self) -> Option<DtbReserveEntries<'a>> {
        Some(DtbReserveEntries::new(self.reserve_map_block()?))
    }

    fn structure_block(&self) -> Option<&'a [u8]> {
        slice_at(
            self.bytes,
            self.header.off_dt_struct,
            self.header.size_dt_struct,
        )
    }

    fn strings_block(&self) -> Option<&'a [u8]> {
        slice_at(
            self.bytes,
            self.header.off_dt_strings,
            self.header.size_dt_strings,
        )
    }

    fn reserve_map_block(&self) -> Option<&'a [u8]> {
        let start = self.header.off_mem_rsvmap as usize;
        let end = self.header.off_dt_struct as usize;
        self.bytes.get(start..end)
    }

    fn is_layout_valid(&self) -> bool {
        let rsvmap_ok = self
            .reserve_map_block()
            .is_some_and(|block| block.len() >= 16);
        self.structure_block().is_some() && self.strings_block().is_some() && rsvmap_ok
    }
}

// ─────────────────────── 内部辅助函数 ─────────────────────────────────────

/// 从 `bytes[cursor..]` 读取一个大端 u32，返回 `(值, 新游标)`。
#[inline]
fn read_be_u32(bytes: &[u8], cursor: usize) -> Option<(u32, usize)> {
    let end = cursor.checked_add(4)?;
    let chunk = bytes.get(cursor..end)?;
    Some((u32::from_be_bytes(chunk.try_into().ok()?), end))
}

/// 从 `bytes[cursor..]` 读取一个大端 u64，返回 `(值, 新游标)`。
#[inline]
fn read_be_u64(bytes: &[u8], cursor: usize) -> Option<(u64, usize)> {
    let end = cursor.checked_add(8)?;
    let chunk = bytes.get(cursor..end)?;
    Some((u64::from_be_bytes(chunk.try_into().ok()?), end))
}

/// 在 `bytes[start..]` 中查找 NUL 字节，返回其在 `bytes` 中的绝对偏移。
#[inline]
fn cstring_end(bytes: &[u8], start: usize) -> Option<usize> {
    start.checked_add(bytes.get(start..)?.iter().position(|&b| b == 0)?)
}

/// 从字符串块的 `offset` 处读取 NUL 结尾字符串（不含 NUL）。
#[inline]
fn cstr_at(block: &[u8], offset: usize) -> Option<&[u8]> {
    let end = cstring_end(block, offset)?;
    block.get(offset..end)
}

/// 向上对齐到 4 字节边界。
#[inline]
fn align4(v: usize) -> usize {
    (v + 3) & !3
}

/// 从 `bytes` 的 `(offset, size)` 区间取子切片。
#[inline]
fn slice_at(bytes: &[u8], offset: u32, size: u32) -> Option<&[u8]> {
    let start = offset as usize;
    bytes.get(start..start.checked_add(size as usize)?)
}

/// 跳过一个节点的完整内容体。
///
/// 调用时游标应已越过该节点的 BEGIN_NODE 名称，位于内容区起始处。
/// 扫描直到对应 END_NODE（深度归零），返回 END_NODE token 的起始偏移
/// （即内容区的结束位置），游标移至 END_NODE 之后。
fn skip_node_body(sb: &[u8], cursor: &mut usize) -> Option<usize> {
    let mut depth = 0usize;
    loop {
        let before = *cursor;
        let (tok, next) = read_be_u32(sb, *cursor)?;
        *cursor = next;
        match tok {
            TOK_BEGIN_NODE => {
                let name_end = cstring_end(sb, *cursor)?;
                *cursor = align4(name_end + 1);
                depth += 1;
            }
            TOK_END_NODE => {
                if depth == 0 {
                    return Some(before);
                }
                depth -= 1;
            }
            TOK_PROP => {
                let (prop_len, next2) = read_be_u32(sb, *cursor)?;
                let (_, next3) = read_be_u32(sb, next2)?; // name_offset
                *cursor = align4(next3.checked_add(prop_len as usize)?);
                if *cursor > sb.len() {
                    return None;
                }
            }
            TOK_NOP => {}
            _ => return None,
        }
    }
}
