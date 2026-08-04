//! 无分配的 FDT 二进制视图与遍历器。

use core::{fmt, str};

use crate::{Cells, ChosenError, Error, PropertyError, StringList};

/// Flattened Devicetree blob 的大端魔数。
pub const DTB_MAGIC: u32 = 0xd00d_feed;

const FDT_BEGIN_NODE: u32 = 1;
const FDT_END_NODE: u32 = 2;
const FDT_PROP: u32 = 3;
const FDT_NOP: u32 = 4;
const FDT_END: u32 = 9;

const V2_HEADER_SIZE: usize = 32;
const V3_HEADER_SIZE: usize = 36;
const V17_HEADER_SIZE: usize = 40;

/// 已转换为本机端序的 FDT 头部。
///
/// 后期版本才增加的字段使用 `Option` 表示，避免把旧格式中属于下一个块的
/// 字节误当成头部字段。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    /// 魔数，成功解析后恒为 [`DTB_MAGIC`]。
    pub magic: u32,
    /// blob 的精确长度。
    pub total_size: u32,
    /// structure block 的文件内偏移。
    pub off_dt_struct: u32,
    /// strings block 的文件内偏移。
    pub off_dt_strings: u32,
    /// memory reservation block 的文件内偏移。
    pub off_mem_rsvmap: u32,
    /// FDT 格式版本。
    pub version: u32,
    /// 生成方声明的最低兼容版本。
    pub last_compatible_version: u32,
    /// v2 起提供的启动 CPU 物理编号。
    pub boot_cpuid_phys: u32,
    /// v3 起显式提供的 strings block 长度。
    pub size_dt_strings: Option<u32>,
    /// v17 起显式提供的 structure block 长度。
    pub size_dt_struct: Option<u32>,
}

impl Header {
    /// 当前版本的头部长度。
    #[inline]
    pub const fn size(self) -> usize {
        header_size(self.version)
    }
}

/// 一份在构造时已经完整校验的 FDT 借用视图。
///
/// `Fdt`、`Node` 和 `Property` 都只借用 blob。解析成功后，所有公开迭代器
/// 均可安全地把结构末尾当作正常结束，而不会用 `Option` 隐藏格式错误。
#[derive(Clone, Copy)]
pub struct Fdt<'a> {
    bytes: &'a [u8],
    header: Header,
    structure: &'a [u8],
    strings: &'a [u8],
    reservations: &'a [u8],
    root_offset: usize,
}

impl fmt::Debug for Fdt<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Fdt")
            .field("header", &self.header)
            .field("reservations", &(self.reservations.len() / 16))
            .finish_non_exhaustive()
    }
}

impl<'a> Fdt<'a> {
    /// 校验并借用一份完整 FDT blob。
    ///
    /// 支持 libfdt 的完整只读格式版本 v2..=v17，以及声明
    /// `last_comp_version <= 17` 的未来兼容版本。兼容处理包括 v16 以前的完整
    /// 路径节点名和长度至少为 8 的属性值 8-byte 对齐规则。传入切片可以在
    /// `total_size` 后带有其他数据；[`Self::as_bytes`] 只返回 FDT 自身。
    pub fn parse(input: &'a [u8]) -> Result<Self, Error> {
        if input.len() < 4 {
            return Err(Error::TruncatedHeader {
                needed: V2_HEADER_SIZE,
                available: input.len(),
            });
        }
        let magic = read_u32(input, 0).expect("four-byte header prefix was checked");
        if magic != DTB_MAGIC {
            return Err(Error::BadMagic(magic));
        }
        if input.len() < 28 {
            return Err(Error::TruncatedHeader {
                needed: V2_HEADER_SIZE,
                available: input.len(),
            });
        }

        let version = read_u32(input, 20).expect("base header was checked");
        let last_compatible_version = read_u32(input, 24).expect("base header was checked");
        if version < 2 || last_compatible_version > 17 {
            return Err(Error::UnsupportedVersion {
                version,
                last_compatible: last_compatible_version,
            });
        }
        if last_compatible_version > version {
            return Err(Error::InvalidVersion {
                version,
                last_compatible: last_compatible_version,
            });
        }

        let required_header = header_size(version);
        if input.len() < required_header {
            return Err(Error::TruncatedHeader {
                needed: required_header,
                available: input.len(),
            });
        }

        let total_size = read_u32(input, 4).expect("versioned header was checked");
        let total = total_size as usize;
        if total < required_header || total > input.len() {
            return Err(Error::InvalidTotalSize {
                declared: total_size,
                header_size: required_header,
                available: input.len(),
            });
        }
        let bytes = &input[..total];

        let off_dt_struct = read_u32(bytes, 8).expect("versioned header was checked");
        let off_dt_strings = read_u32(bytes, 12).expect("versioned header was checked");
        let off_mem_rsvmap = read_u32(bytes, 16).expect("versioned header was checked");
        let boot_cpuid_phys = read_u32(bytes, 28).expect("v2 header was checked");
        let size_dt_strings =
            (version >= 3).then(|| read_u32(bytes, 32).expect("v3 header was checked"));
        let size_dt_struct =
            (version >= 17).then(|| read_u32(bytes, 36).expect("v17 header was checked"));

        let header = Header {
            magic,
            total_size,
            off_dt_struct,
            off_dt_strings,
            off_mem_rsvmap,
            version,
            last_compatible_version,
            boot_cpuid_phys,
            size_dt_strings,
            size_dt_struct,
        };

        require_alignment("memory reservation block", off_mem_rsvmap, 8)?;
        require_alignment("structure block", off_dt_struct, 4)?;
        require_block_offset(
            "memory reservation block",
            off_mem_rsvmap,
            required_header,
            total_size,
        )?;
        require_block_offset(
            "structure block",
            off_dt_struct,
            required_header,
            total_size,
        )?;
        require_block_offset("strings block", off_dt_strings, required_header, total_size)?;

        let strings_size = match size_dt_strings {
            Some(size) => size,
            None => total_size
                .checked_sub(off_dt_strings)
                .ok_or(Error::BlockOutOfBounds {
                    block: "strings block",
                    offset: off_dt_strings,
                    size: 0,
                    total_size,
                })?,
        };
        let strings = checked_block(
            bytes,
            "strings block",
            off_dt_strings,
            strings_size,
            total_size,
        )?;
        let reservation_end = validate_reservations(bytes, off_mem_rsvmap, total_size)?;
        let reservation_start = off_mem_rsvmap as usize;
        let reservations = &bytes[reservation_start..reservation_end - 16];

        let fixed_regions = [
            Region::new("header", 0, required_header),
            Region::new(
                "memory reservation block",
                reservation_start,
                reservation_end,
            ),
            Region::from_size("strings block", off_dt_strings, strings_size, total_size)?,
        ];
        validate_region_pairs(&fixed_regions)?;

        let structure_start = off_dt_struct as usize;
        let (structure, structure_region, root_offset) = if let Some(structure_size) =
            size_dt_struct
        {
            let region =
                Region::from_size("structure block", off_dt_struct, structure_size, total_size)?;
            for fixed in fixed_regions {
                if region.overlaps(fixed) {
                    return Err(Error::BlocksOverlap {
                        first: region.name,
                        second: fixed.name,
                    });
                }
            }
            let structure = checked_block(
                bytes,
                "structure block",
                off_dt_struct,
                structure_size,
                total_size,
            )?;
            let (root, _) = validate_structure(structure, strings, version, true)?;
            (structure, region, root)
        } else {
            // v2..v16 do not encode a structure size. Bound token scanning at the
            // next known block (if any), then use the first complete FDT_END as
            // the actual structure extent. This is how complete old blobs can
            // still be read when v3+ blocks are not in canonical order.
            for fixed in fixed_regions {
                if fixed.start <= structure_start && structure_start < fixed.end {
                    return Err(Error::BlocksOverlap {
                        first: "structure block",
                        second: fixed.name,
                    });
                }
            }
            let mut limit = total;
            for fixed in fixed_regions {
                if fixed.start > structure_start {
                    limit = limit.min(fixed.start);
                }
            }
            let candidate = bytes
                .get(structure_start..limit)
                .ok_or(Error::BlockOutOfBounds {
                    block: "structure block",
                    offset: off_dt_struct,
                    size: 0,
                    total_size,
                })?;
            let (root, used) = validate_structure(candidate, strings, version, false)?;
            let structure = &candidate[..used];
            let region = Region::new("structure block", structure_start, structure_start + used);
            for fixed in fixed_regions {
                if region.overlaps(fixed) {
                    return Err(Error::BlocksOverlap {
                        first: region.name,
                        second: fixed.name,
                    });
                }
            }
            (structure, region, root)
        };
        debug_assert_eq!(structure_region.start, off_dt_struct as usize);

        Ok(Self {
            bytes,
            header,
            structure,
            strings,
            reservations,
            root_offset,
        })
    }

    /// 返回已转换端序的头部。
    #[inline]
    pub const fn header(&self) -> Header {
        self.header
    }

    /// 返回头部 `total_size` 所描述的精确 blob。
    #[inline]
    pub const fn as_bytes(&self) -> &'a [u8] {
        self.bytes
    }

    /// 返回 structure block 原始字节。
    #[inline]
    pub const fn structure_block(&self) -> &'a [u8] {
        self.structure
    }

    /// 返回 strings block 原始字节。
    #[inline]
    pub const fn strings_block(&self) -> &'a [u8] {
        self.strings
    }

    /// 返回根节点。根节点名总是空字符串。
    #[inline]
    pub const fn root(&self) -> Node<'a> {
        Node {
            fdt: *self,
            offset: self.root_offset,
            depth: 0,
        }
    }

    /// 按先序遍历所有节点。
    #[inline]
    pub const fn nodes(&self) -> Nodes<'a> {
        Nodes {
            fdt: *self,
            cursor: 0,
            depth: 0,
            direct: false,
            pending_direct_child: false,
            done: false,
        }
    }

    /// 迭代 memory reservation block 中的条目。
    #[inline]
    pub const fn reservations(&self) -> Reservations<'a> {
        Reservations {
            bytes: self.reservations,
            cursor: 0,
        }
    }

    /// 按绝对路径查找节点。
    ///
    /// `/` 表示根节点。其他路径不得包含空组件、尾随 `/` 或 alias；alias
    /// 解析由启用 `alloc` 后的 `Tree` 提供。路径组件包含 unit-address 时执行
    /// 完整名称匹配；省略 `@unit-address` 时，仅在同名基础名称无歧义时匹配。
    pub fn find_node(&self, path: &str) -> Option<Node<'a>> {
        if path == "/" {
            return Some(self.root());
        }
        let relative = path.strip_prefix('/')?;
        if relative.is_empty() || relative.ends_with('/') {
            return None;
        }

        let mut node = self.root();
        for component in relative.split('/') {
            if component.is_empty() {
                return None;
            }
            node = find_path_child(node, component)?;
        }
        Some(node)
    }

    /// 零分配解析 `/chosen/stdout-path`。
    ///
    /// 属性查找顺序与 Linux 兼容：先 `stdout-path`，再查历史
    /// `linux,stdout-path`。节点路径同时兼容旧固件的 `/chosen@0`；alias 会通过
    /// `/aliases` 展开，第一个 `:` 后的选项原样保留。
    pub fn chosen_stdout(&self) -> Result<Option<FlatChosenStdout<'a>>, ChosenError> {
        let Some(chosen) = self
            .find_node("/chosen")
            .or_else(|| self.find_node("/chosen@0"))
        else {
            return Ok(None);
        };
        let Some(property) = ["stdout-path", "linux,stdout-path"]
            .into_iter()
            .find_map(|name| chosen.property(name))
        else {
            return Ok(None);
        };
        let raw = property.as_str().map_err(ChosenError::InvalidValue)?;
        let (selector, options) = raw
            .split_once(':')
            .map_or((raw, None), |(path, options)| (path, Some(options)));

        let (path, node) = if selector.starts_with('/') {
            (
                selector,
                self.find_node(selector).ok_or(ChosenError::Unresolved)?,
            )
        } else {
            let aliases = self.find_node("/aliases").ok_or(ChosenError::Unresolved)?;
            let alias = aliases.property(selector).ok_or(ChosenError::Unresolved)?;
            let path = alias.as_str().map_err(ChosenError::InvalidAlias)?;
            if !path.starts_with('/') || path.contains(':') {
                return Err(ChosenError::Unresolved);
            }
            (path, self.find_node(path).ok_or(ChosenError::Unresolved)?)
        };
        Ok(Some(FlatChosenStdout {
            raw,
            path,
            node,
            options,
        }))
    }
}

fn find_path_child<'a>(node: Node<'a>, component: &str) -> Option<Node<'a>> {
    let mut abbreviated = None;
    let mut ambiguous = false;
    for child in node.children() {
        if child.name_bytes() == component.as_bytes() {
            return Some(child);
        }
        if !component.contains('@') && child.base_name_bytes() == component.as_bytes() {
            ambiguous |= abbreviated.is_some();
            abbreviated.get_or_insert(child);
        }
    }
    (!ambiguous).then_some(abbreviated).flatten()
}

/// 默认无分配层解析出的 chosen 控制台。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FlatChosenStdout<'a> {
    /// chosen 属性中的完整原始字符串。
    pub raw: &'a str,
    /// alias 展开后的绝对路径。
    pub path: &'a str,
    /// 目标节点借用视图。
    pub node: Node<'a>,
    /// 第一个 `:` 后的未改写选项。
    pub options: Option<&'a str>,
}

/// structure block 中的节点视图。
#[derive(Clone, Copy)]
pub struct Node<'a> {
    fdt: Fdt<'a>,
    offset: usize,
    depth: usize,
}

impl fmt::Debug for Node<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Node")
            .field("name", &self.name())
            .field("offset", &self.offset)
            .field("depth", &self.depth)
            .finish()
    }
}

impl PartialEq for Node<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.fdt.bytes.as_ptr() == other.fdt.bytes.as_ptr()
            && self.fdt.bytes.len() == other.fdt.bytes.len()
            && self.offset == other.offset
    }
}

impl Eq for Node<'_> {}

impl<'a> Node<'a> {
    /// 节点的规范化名称；v15 及更早格式会去掉编码时的祖先路径。
    #[inline]
    pub fn name_bytes(&self) -> &'a [u8] {
        normalized_name(self.raw_name_bytes(), self.fdt.header.version)
    }

    /// 节点名称。解析阶段已按规范保证其为 ASCII。
    #[inline]
    pub fn name(&self) -> &'a str {
        str::from_utf8(self.name_bytes()).expect("validated FDT node name must be ASCII")
    }

    /// blob 中实际编码的节点名；旧格式中它是绝对路径。
    #[inline]
    pub fn raw_name_bytes(&self) -> &'a [u8] {
        node_name_at(self.fdt.structure, self.offset)
            .expect("validated node offset must have a name")
    }

    /// `@unit-address` 前的节点名部分。
    #[inline]
    pub fn base_name_bytes(&self) -> &'a [u8] {
        let name = self.name_bytes();
        name.iter()
            .position(|&byte| byte == b'@')
            .map(|end| &name[..end])
            .unwrap_or(name)
    }

    /// 节点在根节点之下的深度；根节点为 0。
    #[inline]
    pub const fn depth(&self) -> usize {
        self.depth
    }

    /// BEGIN_NODE token 在 structure block 内的偏移。
    #[inline]
    pub const fn structure_offset(&self) -> usize {
        self.offset
    }

    /// 迭代直接属性。
    #[inline]
    pub fn properties(&self) -> Properties<'a> {
        Properties {
            fdt: self.fdt,
            cursor: after_node_header(self.fdt.structure, self.offset)
                .expect("validated node header must be complete"),
            done: false,
        }
    }

    /// 精确查找直接属性。
    #[inline]
    pub fn property(&self, name: &str) -> Option<Property<'a>> {
        self.properties()
            .find(|property| property.name_bytes() == name.as_bytes())
    }

    /// `property` 的兼容别名。
    #[inline]
    pub fn find_property(&self, name: &str) -> Option<Property<'a>> {
        self.property(name)
    }

    /// 迭代直接子节点。
    #[inline]
    pub fn children(&self) -> Nodes<'a> {
        Nodes {
            fdt: self.fdt,
            cursor: after_node_header(self.fdt.structure, self.offset)
                .expect("validated node header must be complete"),
            depth: self.depth + 1,
            direct: true,
            pending_direct_child: false,
            done: false,
        }
    }

    /// 按完整节点名精确查找直接子节点。
    #[inline]
    pub fn find_child(&self, name: &str) -> Option<Node<'a>> {
        self.children()
            .find(|child| child.name_bytes() == name.as_bytes())
    }

    /// 按不含 unit-address 的基础名查找直接子节点。
    ///
    /// 多个设备可共享基础名，因此该接口只返回第一个；平台发现代码通常应使用
    /// 完整名称、`compatible` 或遍历器。
    #[inline]
    pub fn find_child_by_base_name(&self, name: &str) -> Option<Node<'a>> {
        self.children()
            .find(|child| child.base_name_bytes() == name.as_bytes())
    }

    /// 判断 `compatible` 字符串列表是否包含给定值。
    pub fn is_compatible(&self, compatible: &str) -> bool {
        self.property("compatible")
            .and_then(|property| property.as_string_list().ok())
            .is_some_and(|mut values| values.any(|value| value == compatible))
    }
}

/// 节点直接属性的借用迭代器。
#[derive(Clone, Debug)]
pub struct Properties<'a> {
    fdt: Fdt<'a>,
    cursor: usize,
    done: bool,
}

impl<'a> Iterator for Properties<'a> {
    type Item = Property<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        loop {
            let offset = self.cursor;
            let tag = read_u32(self.fdt.structure, offset)?;
            match tag {
                FDT_NOP => self.cursor += 4,
                FDT_PROP => {
                    let parts = property_at(
                        self.fdt.structure,
                        self.fdt.strings,
                        self.fdt.header.version,
                        offset,
                    )?;
                    self.cursor = parts.next;
                    return Some(Property {
                        name: parts.name,
                        value: parts.value,
                        offset,
                        end: parts.next,
                    });
                }
                FDT_BEGIN_NODE | FDT_END_NODE | FDT_END => {
                    self.done = true;
                    return None;
                }
                _ => unreachable!("Fdt::parse validates every structure token"),
            }
        }
    }
}

impl core::iter::FusedIterator for Properties<'_> {}

/// 一个属性名及其未经解释的原始值。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Property<'a> {
    name: &'a [u8],
    value: &'a [u8],
    offset: usize,
    end: usize,
}

impl<'a> Property<'a> {
    /// 属性名原始字节。解析阶段已按规范保证其为 ASCII。
    #[inline]
    pub const fn name_bytes(&self) -> &'a [u8] {
        self.name
    }

    /// 属性名。
    #[inline]
    pub fn name(&self) -> &'a str {
        str::from_utf8(self.name).expect("validated FDT property name must be ASCII")
    }

    /// 属性值原始字节，保持 FDT 中的大端表示。
    #[inline]
    pub const fn value(&self) -> &'a [u8] {
        self.value
    }

    /// PROP token 在 structure block 内的偏移。
    #[inline]
    pub const fn structure_offset(&self) -> usize {
        self.offset
    }

    /// 属性在 structure block 中占用的完整编码范围。
    ///
    /// 范围包含 `FDT_PROP` token、长度和名称偏移字段、旧版本可能存在的
    /// 8 字节值对齐空隙、属性值以及末尾 4 字节对齐填充。
    #[inline]
    pub fn encoded_structure_range(&self) -> core::ops::Range<usize> {
        self.offset..self.end
    }

    /// 按布尔属性解码；规范布尔属性必须是空值。
    pub fn as_bool(&self) -> Result<bool, PropertyError> {
        if self.value.is_empty() {
            Ok(true)
        } else {
            Err(PropertyError::InvalidLength {
                actual: self.value.len(),
                expected: Some(0),
            })
        }
    }

    /// 解码一个大端 `u32`。
    pub fn as_u32(&self) -> Result<u32, PropertyError> {
        let bytes: [u8; 4] = self
            .value
            .try_into()
            .map_err(|_| PropertyError::InvalidLength {
                actual: self.value.len(),
                expected: Some(4),
            })?;
        Ok(u32::from_be_bytes(bytes))
    }

    /// 解码一个大端 `u64`。
    pub fn as_u64(&self) -> Result<u64, PropertyError> {
        let bytes: [u8; 8] = self
            .value
            .try_into()
            .map_err(|_| PropertyError::InvalidLength {
                actual: self.value.len(),
                expected: Some(8),
            })?;
        Ok(u64::from_be_bytes(bytes))
    }

    /// 解码恰好一个 NUL 结尾 UTF-8 字符串。
    pub fn as_str(&self) -> Result<&'a str, PropertyError> {
        if self.value.last() != Some(&0) {
            return Err(PropertyError::MissingNul);
        }
        let body = &self.value[..self.value.len() - 1];
        if body.contains(&0) {
            return Err(PropertyError::MultipleStrings);
        }
        str::from_utf8(body).map_err(|_| PropertyError::InvalidUtf8)
    }

    /// 校验并迭代 NUL 分隔字符串列表。
    #[inline]
    pub fn as_string_list(&self) -> Result<StringList<'a>, PropertyError> {
        StringList::new(self.value)
    }

    /// 校验并迭代大端 32-bit cells。
    #[inline]
    pub fn cells(&self) -> Result<Cells<'a>, PropertyError> {
        Cells::new(self.value)
    }
}

/// 所有节点或直接子节点的先序迭代器。
#[derive(Clone, Debug)]
pub struct Nodes<'a> {
    fdt: Fdt<'a>,
    cursor: usize,
    depth: usize,
    direct: bool,
    pending_direct_child: bool,
    done: bool,
}

impl<'a> Iterator for Nodes<'a> {
    type Item = Node<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        if self.pending_direct_child {
            self.cursor = skip_node(&self.fdt, self.cursor)?;
            self.pending_direct_child = false;
        }

        // 根遍历从 structure offset 0、depth 0 开始；子节点遍历从父节点内容
        // 开始且 depth 非零。二者通过首 token 是否为根 BEGIN_NODE 区分。
        let direct = self.direct;
        loop {
            let offset = self.cursor;
            let tag = read_u32(self.fdt.structure, offset)?;
            match tag {
                FDT_NOP => self.cursor += 4,
                FDT_PROP => {
                    self.cursor = property_at(
                        self.fdt.structure,
                        self.fdt.strings,
                        self.fdt.header.version,
                        offset,
                    )?
                    .next;
                }
                FDT_BEGIN_NODE => {
                    if direct {
                        self.pending_direct_child = true;
                        return Some(Node {
                            fdt: self.fdt,
                            offset,
                            depth: self.depth,
                        });
                    }
                    let node_depth = self.depth;
                    self.depth += 1;
                    self.cursor = after_node_header(self.fdt.structure, offset)?;
                    return Some(Node {
                        fdt: self.fdt,
                        offset,
                        depth: node_depth,
                    });
                }
                FDT_END_NODE => {
                    self.cursor += 4;
                    if direct {
                        self.done = true;
                        return None;
                    }
                    self.depth = self.depth.checked_sub(1)?;
                }
                FDT_END => {
                    self.done = true;
                    return None;
                }
                _ => unreachable!("Fdt::parse validates every structure token"),
            }
        }
    }
}

impl core::iter::FusedIterator for Nodes<'_> {}

/// memory reservation block 中的一项。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReserveEntry {
    /// 物理起始地址。
    pub address: u64,
    /// 保留长度。
    pub size: u64,
}

/// memory reservation block 迭代器。
#[derive(Clone, Debug)]
pub struct Reservations<'a> {
    bytes: &'a [u8],
    cursor: usize,
}

impl Iterator for Reservations<'_> {
    type Item = ReserveEntry;

    fn next(&mut self) -> Option<Self::Item> {
        let address = read_u64(self.bytes, self.cursor)?;
        let size = read_u64(self.bytes, self.cursor + 8)?;
        self.cursor += 16;
        Some(ReserveEntry { address, size })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = (self.bytes.len() - self.cursor) / 16;
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for Reservations<'_> {}
impl core::iter::FusedIterator for Reservations<'_> {}

#[derive(Clone, Copy)]
struct Region {
    name: &'static str,
    start: usize,
    end: usize,
}

impl Region {
    const fn new(name: &'static str, start: usize, end: usize) -> Self {
        Self { name, start, end }
    }

    fn from_size(name: &'static str, offset: u32, size: u32, total: u32) -> Result<Self, Error> {
        let start = offset as usize;
        let end = start
            .checked_add(size as usize)
            .ok_or(Error::BlockOutOfBounds {
                block: name,
                offset,
                size,
                total_size: total,
            })?;
        Ok(Self::new(name, start, end))
    }

    fn overlaps(self, other: Self) -> bool {
        self.start < self.end
            && other.start < other.end
            && self.start < other.end
            && other.start < self.end
    }
}

#[derive(Clone, Copy)]
struct PropertyParts<'a> {
    name: &'a [u8],
    value: &'a [u8],
    next: usize,
}

const fn header_size(version: u32) -> usize {
    match version {
        0..=1 => 28,
        2 => V2_HEADER_SIZE,
        3..=16 => V3_HEADER_SIZE,
        _ => V17_HEADER_SIZE,
    }
}

fn require_alignment(block: &'static str, offset: u32, alignment: usize) -> Result<(), Error> {
    if (offset as usize).is_multiple_of(alignment) {
        Ok(())
    } else {
        Err(Error::MisalignedBlock {
            block,
            offset,
            alignment,
        })
    }
}

fn require_block_offset(
    block: &'static str,
    offset: u32,
    header_size: usize,
    total_size: u32,
) -> Result<(), Error> {
    if (offset as usize) < header_size || offset > total_size {
        Err(Error::BlockOutOfBounds {
            block,
            offset,
            size: 0,
            total_size,
        })
    } else {
        Ok(())
    }
}

fn validate_region_pairs(regions: &[Region]) -> Result<(), Error> {
    for first in 0..regions.len() {
        for second in first + 1..regions.len() {
            if regions[first].overlaps(regions[second]) {
                return Err(Error::BlocksOverlap {
                    first: regions[first].name,
                    second: regions[second].name,
                });
            }
        }
    }
    Ok(())
}

fn checked_block<'a>(
    bytes: &'a [u8],
    block: &'static str,
    offset: u32,
    size: u32,
    total_size: u32,
) -> Result<&'a [u8], Error> {
    let start = offset as usize;
    let end = start
        .checked_add(size as usize)
        .ok_or(Error::BlockOutOfBounds {
            block,
            offset,
            size,
            total_size,
        })?;
    bytes.get(start..end).ok_or(Error::BlockOutOfBounds {
        block,
        offset,
        size,
        total_size,
    })
}

fn validate_reservations(bytes: &[u8], offset: u32, total_size: u32) -> Result<usize, Error> {
    let mut cursor = offset as usize;
    loop {
        let end = cursor
            .checked_add(16)
            .ok_or(Error::TruncatedReservation { offset: cursor })?;
        let entry = bytes
            .get(cursor..end)
            .ok_or(Error::MissingReservationTerminator { offset })?;
        let address = u64::from_be_bytes(entry[..8].try_into().unwrap());
        let size = u64::from_be_bytes(entry[8..].try_into().unwrap());
        cursor = end;
        if address == 0 && size == 0 {
            return Ok(cursor);
        }
        if cursor > total_size as usize {
            return Err(Error::MissingReservationTerminator { offset });
        }
    }
}

fn validate_structure(
    structure: &[u8],
    strings: &[u8],
    version: u32,
    validate_trailing: bool,
) -> Result<(usize, usize), Error> {
    let mut cursor = 0usize;
    let mut depth = 0usize;
    let mut root_offset = None;
    let mut root_ended = false;
    let mut last_significant_was_end_node = false;

    loop {
        let token_offset = cursor;
        let token =
            read_u32(structure, cursor).ok_or(Error::TruncatedStructure { offset: cursor })?;
        cursor += 4;

        match token {
            // FDT_NOP may appear anywhere in the structure block, including
            // between the root END_NODE and the final FDT_END marker.
            FDT_NOP => {}
            FDT_BEGIN_NODE => {
                if root_ended {
                    return Err(Error::InvalidTokenOrder {
                        offset: token_offset,
                        token,
                    });
                }
                let name_end = find_nul(structure, cursor).ok_or(Error::UnterminatedNodeName {
                    offset: token_offset,
                })?;
                let raw_name = &structure[cursor..name_end];
                let after_nul = name_end + 1;
                let next =
                    align_up(after_nul, 4).ok_or(Error::TruncatedStructure { offset: cursor })?;
                structure
                    .get(after_nul..next)
                    .ok_or(Error::TruncatedStructure { offset: cursor })?;

                if root_offset.is_none() {
                    let valid_root = if version < 16 {
                        raw_name == b"/"
                    } else {
                        raw_name.is_empty()
                    };
                    if depth != 0 || !valid_root {
                        return Err(Error::InvalidRootName {
                            offset: token_offset,
                        });
                    }
                    root_offset = Some(token_offset);
                } else if depth == 0 {
                    return Err(Error::InvalidTokenOrder {
                        offset: token_offset,
                        token,
                    });
                }

                validate_node_name(raw_name, version, depth).map_err(|()| {
                    Error::InvalidNodeName {
                        offset: token_offset,
                    }
                })?;
                depth = depth.checked_add(1).ok_or(Error::UnbalancedNode {
                    offset: token_offset,
                })?;
                cursor = next;
                last_significant_was_end_node = false;
            }
            FDT_END_NODE => {
                if depth == 0 || root_offset.is_none() || root_ended {
                    return Err(Error::UnbalancedNode {
                        offset: token_offset,
                    });
                }
                depth -= 1;
                if depth == 0 {
                    root_ended = true;
                }
                last_significant_was_end_node = true;
            }
            FDT_PROP => {
                if depth == 0 || root_ended {
                    return Err(Error::InvalidTokenOrder {
                        offset: token_offset,
                        token,
                    });
                }
                if last_significant_was_end_node {
                    return Err(Error::PropertyAfterChild {
                        offset: token_offset,
                    });
                }
                let (length, name_offset) =
                    match (read_u32(structure, cursor), read_u32(structure, cursor + 4)) {
                        (Some(length), Some(name_offset)) => (length, name_offset),
                        _ => {
                            return Err(Error::TruncatedProperty {
                                offset: token_offset,
                                length: None,
                            });
                        }
                    };
                cursor += 8;

                if version < 16 && length >= 8 {
                    let aligned = align_up(cursor, 8).ok_or(Error::TruncatedProperty {
                        offset: token_offset,
                        length: Some(length),
                    })?;
                    structure
                        .get(cursor..aligned)
                        .ok_or(Error::TruncatedProperty {
                            offset: token_offset,
                            length: Some(length),
                        })?;
                    cursor = aligned;
                }
                let value_end =
                    cursor
                        .checked_add(length as usize)
                        .ok_or(Error::TruncatedProperty {
                            offset: token_offset,
                            length: Some(length),
                        })?;
                if value_end > structure.len() {
                    return Err(Error::TruncatedProperty {
                        offset: token_offset,
                        length: Some(length),
                    });
                }
                let next = align_up(value_end, 4).ok_or(Error::TruncatedProperty {
                    offset: token_offset,
                    length: Some(length),
                })?;
                structure
                    .get(value_end..next)
                    .ok_or(Error::TruncatedProperty {
                        offset: token_offset,
                        length: Some(length),
                    })?;
                cursor = next;

                let name_start = name_offset as usize;
                if name_start >= strings.len() {
                    return Err(Error::InvalidStringOffset {
                        property_offset: token_offset,
                        string_offset: name_offset,
                    });
                }
                let name_end =
                    find_nul(strings, name_start).ok_or(Error::UnterminatedPropertyName {
                        property_offset: token_offset,
                        string_offset: name_offset,
                    })?;
                if !valid_property_name(&strings[name_start..name_end]) {
                    return Err(Error::InvalidPropertyName {
                        property_offset: token_offset,
                        string_offset: name_offset,
                    });
                }
                last_significant_was_end_node = false;
            }
            FDT_END => {
                if !root_ended || depth != 0 {
                    return Err(Error::UnbalancedNode {
                        offset: token_offset,
                    });
                }
                if validate_trailing {
                    validate_structure_trailing(structure, cursor)?;
                }
                let root = root_offset.ok_or(Error::InvalidTokenOrder {
                    offset: token_offset,
                    token,
                })?;
                return Ok((root, cursor));
            }
            token => {
                return Err(Error::InvalidToken {
                    offset: token_offset,
                    token,
                });
            }
        }

        if cursor >= structure.len() {
            return if root_ended {
                Err(Error::MissingEndToken { offset: cursor })
            } else {
                Err(Error::TruncatedStructure { offset: cursor })
            };
        }
    }
}

fn validate_structure_trailing(structure: &[u8], mut cursor: usize) -> Result<(), Error> {
    while cursor < structure.len() {
        let remaining = structure.len() - cursor;
        if remaining < 4 {
            if structure[cursor..].iter().all(|&byte| byte == 0) {
                return Ok(());
            }
            return Err(Error::NonZeroPadding { offset: cursor });
        }
        let token = read_u32(structure, cursor).expect("four trailing bytes were checked");
        if token != 0 && token != FDT_NOP {
            return Err(Error::InvalidTrailingToken {
                offset: cursor,
                token,
            });
        }
        cursor += 4;
    }
    Ok(())
}

fn validate_node_name(raw: &[u8], version: u32, depth: usize) -> Result<(), ()> {
    if depth == 0 {
        return if (version < 16 && raw == b"/") || (version >= 16 && raw.is_empty()) {
            Ok(())
        } else {
            Err(())
        };
    }

    if version < 16 {
        if raw.first() != Some(&b'/') || raw.last() == Some(&b'/') {
            return Err(());
        }
        if raw.iter().filter(|&&byte| byte == b'/').count() != depth {
            return Err(());
        }
        if raw[1..]
            .split(|&byte| byte == b'/')
            .any(|part| !valid_node_component(part))
        {
            return Err(());
        }
        Ok(())
    } else if valid_node_component(raw) {
        Ok(())
    } else {
        Err(())
    }
}

fn valid_node_component(name: &[u8]) -> bool {
    !name.is_empty()
        && name.iter().all(|&byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b',' | b'.' | b'_' | b'+' | b'-' | b'@')
        })
        && name.iter().filter(|&&byte| byte == b'@').count() <= 1
        && !name.starts_with(b"@")
        && !name.ends_with(b"@")
}

fn valid_property_name(name: &[u8]) -> bool {
    !name.is_empty()
        && name.iter().all(|&byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b',' | b'.' | b'_' | b'+' | b'*' | b'?' | b'#' | b'-')
        })
}

fn property_at<'a>(
    structure: &'a [u8],
    strings: &'a [u8],
    version: u32,
    offset: usize,
) -> Option<PropertyParts<'a>> {
    if read_u32(structure, offset)? != FDT_PROP {
        return None;
    }
    let length = read_u32(structure, offset + 4)? as usize;
    let name_offset = read_u32(structure, offset + 8)? as usize;
    let mut value_start = offset.checked_add(12)?;
    if version < 16 && length >= 8 {
        value_start = align_up(value_start, 8)?;
    }
    let value_end = value_start.checked_add(length)?;
    let next = align_up(value_end, 4)?;
    let name_end = find_nul(strings, name_offset)?;
    Some(PropertyParts {
        name: strings.get(name_offset..name_end)?,
        value: structure.get(value_start..value_end)?,
        next,
    })
}

fn node_name_at(structure: &[u8], offset: usize) -> Option<&[u8]> {
    if read_u32(structure, offset)? != FDT_BEGIN_NODE {
        return None;
    }
    let start = offset.checked_add(4)?;
    let end = find_nul(structure, start)?;
    structure.get(start..end)
}

fn after_node_header(structure: &[u8], offset: usize) -> Option<usize> {
    let start = offset.checked_add(4)?;
    let end = find_nul(structure, start)?;
    align_up(end.checked_add(1)?, 4)
}

fn normalized_name(raw: &[u8], version: u32) -> &[u8] {
    if version >= 16 {
        return raw;
    }
    raw.iter()
        .rposition(|&byte| byte == b'/')
        .map(|slash| &raw[slash + 1..])
        .unwrap_or(raw)
}

fn skip_node(fdt: &Fdt<'_>, offset: usize) -> Option<usize> {
    let mut cursor = after_node_header(fdt.structure, offset)?;
    let mut depth = 1usize;
    while depth != 0 {
        let token = read_u32(fdt.structure, cursor)?;
        match token {
            FDT_NOP => cursor += 4,
            FDT_PROP => {
                cursor = property_at(fdt.structure, fdt.strings, fdt.header.version, cursor)?.next;
            }
            FDT_BEGIN_NODE => {
                depth += 1;
                cursor = after_node_header(fdt.structure, cursor)?;
            }
            FDT_END_NODE => {
                depth -= 1;
                cursor += 4;
            }
            _ => return None,
        }
    }
    Some(cursor)
}

#[inline]
fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let end = offset.checked_add(4)?;
    Some(u32::from_be_bytes(bytes.get(offset..end)?.try_into().ok()?))
}

#[inline]
fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    let end = offset.checked_add(8)?;
    Some(u64::from_be_bytes(bytes.get(offset..end)?.try_into().ok()?))
}

#[inline]
fn find_nul(bytes: &[u8], start: usize) -> Option<usize> {
    start.checked_add(bytes.get(start..)?.iter().position(|&byte| byte == 0)?)
}

#[inline]
fn align_up(value: usize, alignment: usize) -> Option<usize> {
    value
        .checked_add(alignment - 1)
        .map(|sum| sum & !(alignment - 1))
}
