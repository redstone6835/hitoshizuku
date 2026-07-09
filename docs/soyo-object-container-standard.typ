#import "config.typ": project-name
#import "styles/document.typ": manual
#import "styles/tokens.typ": line-stroke, mono-font, soft-fill, warm-fill

#show: manual.with(
  title: "SOYO 可拓展对象容器格式标准",
  author: project-name,
)

#let head(body) = table.cell(fill: soft-fill)[#text(weight: "bold")[#body]]
#let warn(body) = block(
  breakable: true,
  width: 100%,
  fill: warm-fill,
  stroke: line-stroke,
  radius: 6pt,
  inset: 8pt,
)[#body]

= SOYO 可拓展对象容器格式标准

本文定义 MyGO 内核原生可拓展对象容器格式 SOYO 的版本 1。SOYO 文件由 Header、String Table、Blob Table、Object Table、Schema Table、Projection Table、Relation Table、Metadata Binding Table、External Reference Table 和 TLV 扩展层组成。符合本文的实现必须按对象记录、schema 记录、projection 记录、relation 记录、metadata 绑定记录和外部引用记录解释文件内容，并按对应 schema 或 projector 校验对象 payload、投影契约、关系约束、策略 TLV 和扩展元数据。

SOYO Core 不内建任何执行 profile，不内建 EBI、PROCESS、ELF、tar 或其它目标格式。具体语义由对象 schema 和 projector 定义。SOYO Core 只提供稳定承载、发现、校验和投影声明框架；任何执行、转换或导出行为都必须由实现显式支持对应 schema 和 projector 后才能发生。SOYO 文件的可操作语义由 Projection Table 给出；没有 projection 的 SOYO 文件只是可检查对象图，不具备默认执行、导出或转换含义。

本文使用“必须”“不得”“应当”“可以”表达规范强度。“必须”和“不得”表示实现必须遵守的硬性约束；“应当”表示推荐约束；“可以”表示允许但不要求。

== 1. 适用范围

本文规定以下内容：

- SOYO v1 Header、字符串池、字节块表、对象表、schema 表、projection 表、relation 表、metadata 绑定表、外部引用表和 TLV 扩展层的二进制布局。
- SOYO v1 对象类型、schema identifier、projector identifier、目标协议 identifier 和 relation type identifier 的通用命名规则。
- SOYO v1 schema 记录、schema 描述、schema 哈希和 schema 解析来源规则。
- SOYO v1 projection 记录、目标协议声明、projector 选择、projector ABI、PRIMARY projection 选择和输入闭包失败规则。
- SOYO v1 relation 记录、对象图约束和 relation payload 承载规则。
- SOYO v1 metadata binding、TLV 扩展元数据和对象级、schema 级、projection 级强制策略规则。
- SOYO v1 解析器、检查器、projector、导出器和文件生产工具的合规要求。

本文不规定以下内容：

- 用户态进程映像的入口、栈、TLS、系统调用策略或绑定槽布局。
- ELM、EBI、EKI、LKM、KLD 或任何内核扩展运行时的字段布局。
- tar、cpio、ELF 或任何归档、链接、调试文件的目标格式布局。
- 任意具体对象 payload 的业务语义。

这些内容应当由独立 schema 规范和 projector 规范定义，并通过本文定义的对象、schema、projection 和 relation 机制承载。

== 2. 文件分层

SOYO 文件由十类区域组成。

#table(
  columns: (0.6fr, 1.5fr, 3.2fr),
  inset: 6pt,
  stroke: line-stroke,
  align: (center, left, left),
  table.header(
    head[顺序],
    head[区域],
    head[职责],
  ),
  [1], [Header], [固定 192 字节，给出版本、通用表位置、通用表项数量和 TLV 链位置。],
  [2], [String Table], [承载对象类型、schema、projector、目标协议和展示元数据引用的 NUL 终止 UTF-8 字符串。],
  [3], [Blob Table], [描述文件中的原始字节块或全零字节块；SOYO Core 不解释 blob 的业务用途。],
  [4], [Object Table], [描述文件中的对象节点；对象 payload 由 schema 和 projector 解释。],
  [5], [Schema Table], [描述 schema 身份、版本、来源、哈希和可选 schema descriptor。],
  [6], [Projection Table], [声明某个对象可由某个 projector 投影为某个目标协议对象或目标文件。],
  [7], [Relation Table], [描述对象之间的有向关系；关系语义由 relation schema 解释。],
  [8], [Metadata Binding Table], [把 TLV 元数据绑定到文件、blob、对象、schema、projection、relation 或外部引用。],
  [9], [External Reference Table], [声明外部 schema、blob、对象、SOYO 文件、projector 或信任锚，但 SOYO Core 不自动取回。],
  [10], [TLV], [承载可扩展信息、展示信息、诊断信息和可选策略；被强制引用的 TLV 必须解析并安装成功。],
)

所有多字节整数均使用 little-endian 编码。解析器不得把不可信字节流直接 cast 为内存结构；实现必须按字段偏移读取并检查边界。

=== 2.1 字符串、identifier、契约和哈希类别

除单项字段另有规定外，`*_off` 字段均引用 String Table 中的 NUL 终止 UTF-8 字符串。字符串比较按字节进行，不执行 Unicode 归一化。identifier 字符串使用点分层命名方式，例如 `mygo.elm.unit.schema`、`mygo.elm.ebi`、`posix.tar.archive`。identifier 只允许小写字母、数字、`.`、`-` 和 `_`，不得包含 `:`、`/`、`?`、`#`、`@` 或空白字符。identifier 最大长度由具体字段规定；未规定时不得超过 128 字节。版本字符串由 schema 或目标协议定义，但不得包含 NUL 字节。

契约字符串仍使用 `name@version` 形式；name 只允许小写字母、数字、`.`、`-` 和 `_`，version 只允许数字和 `.`。

SOYO v1 定义两类哈希字段，其用途必须严格区分：

- 匹配哈希：8 字节 `*_hash` 字段，只能作为快速索引键、缓存键或诊断标签，不提供抗碰撞安全性，不得作为身份认证、schema 绑定、projector 选择、契约真值判定或安全策略决策的唯一依据。
- 完整性哈希：32 字节 content_hash 或 schema_hash 字段，用于 schema descriptor、blob 内容、对象 payload、投影配置字节和外部内容的完整性校验。

匹配哈希统一使用 `SOYO_HASH64_BLAKE3_LO`：对规范字节串计算 BLAKE3-256，取 digest 的前 8 字节并按 little-endian 解释为 u64。字段为 0 表示不提供匹配哈希。字符串哈希的输入是 NUL 终止符之前的 UTF-8 字节，不包含末尾 NUL。匹配哈希命中后，实现仍必须通过字符串逐字节比较、完整性哈希或目标协议定义的强校验确认身份。非 0 匹配哈希与对应字节串不一致时，实现必须记录诊断并回退到字符串逐字节比较或完整性哈希校验；不得仅因匹配哈希不一致而拒绝执行、投影或导出，也不得把不一致的匹配哈希用于任何安全决策。

完整性哈希的算法由所在字段的 hash_alg 指定。HASH_NONE 表示不提供完整性哈希；BLAKE3_256 和 SHA256 均写入完整 32 字节 digest。完整性哈希不截断，不得替换为匹配哈希。schema 解析器和 projector 装载器在遇到非零 schema_hash、content_hash 或外部引用 content_hash 时，必须优先校验完整性哈希；只有 hash_alg == HASH_NONE 或对应对象本身不提供完整性哈希时，才可以退回到 identifier 字符串和版本字符串的精确字节比较。

== 3. Header

Header 长度为 192 字节。header_size 必须为 192。未来版本若扩展 Header，必须提升 version。

#table(
  columns: (0.7fr, 0.6fr, 1.8fr, 3.2fr),
  inset: 5pt,
  stroke: line-stroke,
  align: (left, center, left, left),
  table.header(
    head[偏移],
    head[大小],
    head[字段],
    head[说明],
  ),
  [`0x00`], [4], [`magic`], [`soyo`，字节序列 `0x73 0x6f 0x79 0x6f`。],
  [`0x04`], [2], [`version`], [格式版本。本文定义为 1。],
  [`0x06`], [2], [`header_size`], [Header 大小。v1 必须为 192。],
  [`0x08`], [4], [`flags`], [文件级标志。v1 未定义任何 bit，必须为 0。],
  [`0x0c`], [4], [`reserved0`], [保留，必须为 0。],
  [`0x10`], [8], [`string_offset`], [String Table 文件偏移。],
  [`0x18`], [8], [`string_size`], [String Table 字节数。],
  [`0x20`], [8], [`blob_offset`], [Blob Table 文件偏移。无 blob 时为 0。],
  [`0x28`], [4], [`blob_count`], [Blob 记录数量。],
  [`0x2c`], [2], [`blob_entry_size`], [Blob 记录大小。v1 必须为 96。],
  [`0x2e`], [2], [`reserved1`], [保留，必须为 0。],
  [`0x30`], [8], [`object_offset`], [Object Table 文件偏移。无对象时为 0。],
  [`0x38`], [4], [`object_count`], [对象记录数量。],
  [`0x3c`], [2], [`object_entry_size`], [对象记录大小。v1 必须为 80。],
  [`0x3e`], [2], [`reserved2`], [保留，必须为 0。],
  [`0x40`], [8], [`schema_offset`], [Schema Table 文件偏移。无 schema 时为 0。],
  [`0x48`], [4], [`schema_count`], [schema 记录数量。],
  [`0x4c`], [2], [`schema_entry_size`], [schema 记录大小。v1 必须为 96。],
  [`0x4e`], [2], [`reserved3`], [保留，必须为 0。],
  [`0x50`], [8], [`projection_offset`], [Projection Table 文件偏移。无 projection 时为 0。],
  [`0x58`], [4], [`projection_count`], [projection 记录数量。],
  [`0x5c`], [2], [`projection_entry_size`], [projection 记录大小。v1 必须为 80。],
  [`0x5e`], [2], [`reserved4`], [保留，必须为 0。],
  [`0x60`], [8], [`relation_offset`], [Relation Table 文件偏移。无 relation 时为 0。],
  [`0x68`], [4], [`relation_count`], [relation 记录数量。],
  [`0x6c`], [2], [`relation_entry_size`], [relation 记录大小。v1 必须为 80。],
  [`0x6e`], [2], [`reserved5`], [保留，必须为 0。],
  [`0x70`], [8], [`metadata_offset`], [Metadata Binding Table 文件偏移。无 metadata 绑定时为 0。],
  [`0x78`], [4], [`metadata_count`], [metadata 绑定记录数量。],
  [`0x7c`], [2], [`metadata_entry_size`], [metadata 绑定记录大小。v1 必须为 32。],
  [`0x7e`], [2], [`reserved6`], [保留，必须为 0。],
  [`0x80`], [8], [`external_ref_offset`], [External Reference Table 文件偏移。无外部引用时为 0。],
  [`0x88`], [4], [`external_ref_count`], [外部引用记录数量。],
  [`0x8c`], [2], [`external_ref_entry_size`], [外部引用记录大小。v1 必须为 80。],
  [`0x8e`], [2], [`reserved7`], [保留，必须为 0。],
  [`0x90`], [8], [`tlv_offset`], [TLV 链文件偏移。无 TLV 时为 0。],
  [`0x98`], [8], [`tlv_size`], [TLV 链字节数。无 TLV 时为 0。],
  [`0xa0`], [8], [`file_size`], [SOYO 文件总字节数。必须等于实际文件大小。],
  [`0xa8`], [24], [`reserved8`], [保留，必须全 0。],
)

Header 不包含 profile、程序入口、目标架构、生命周期、绑定需求或 EBI 字段。入口、生命周期、绑定需求、目标架构、系统调用策略、归档条目和协议字段必须由对象 payload、schema 和 projector 表达。

file_size 必须等于实际文件大小，且不得小于 header_size。任一表 count 为 0 时，对应 offset 必须为 0。任一表 count 非 0 时，对应 offset 必须非 0，且 `offset + count * entry_size` 必须位于文件范围内。若 tlv_size == 0，tlv_offset 必须为 0；若 tlv_size != 0，tlv_offset 必须非 0，且 `tlv_offset..tlv_offset+tlv_size` 必须位于文件范围内。所有 `offset + size` 和 `offset + count * entry_size` 计算必须先检查整数溢出。Header、String Table、所有通用表和 TLV 链构成核心元数据区；所有非空核心元数据区必须两两不重叠。

== 4. String Table

String Table 是 SOYO Core 的唯一字符串池。它为 Object Table、Schema Table、Projection Table、Relation Table 和 TLV 中的 `*_off` 字段提供 NUL 终止 UTF-8 字符串。String Table payload 是一个字节串。offset 0 必须为 NUL，表示空字符串或缺省值。

任意非 0 字符串偏移必须落在 String Table 范围内，指向一个 NUL 终止 UTF-8 字符串的首字节，且该字符串不得越过 String Table 末尾。字符串比较按字节进行，不执行 Unicode 归一化。

字段说明为“必需 identifier”“必需名称”“必需 schema”或“不得为 0”的偏移不得使用 offset 0。

== 5. Schema 与对象类型注册

SOYO Core 不注册具体 profile。对象类型、schema、target protocol、projector 和 relation type 均通过 identifier 表达。实现遇到未知 identifier 时，不得根据 identifier 字符串自动推断可执行或可转换语义。

通用检查工具可以展示未知对象、未知 schema 和未知 projection；装载器、projector 或导出器只有在显式支持对应 schema 与 projector 时，才可以执行、投影或导出。

identifier 的建议命名范围如下。

#table(
  columns: (1.2fr, 2.6fr, 2.6fr),
  inset: 6pt,
  stroke: line-stroke,
  align: (left, left, left),
  table.header(
    head[类别],
    head[示例],
    head[含义],
  ),
  [对象类型], [`mygo.elm.unit`], [对象 payload 表示一个 ELM 单元。SOYO Core 不理解其字段。],
  [schema], [`mygo.elm.unit.schema.v1`], [解释 ELM 单元对象 payload 的 schema。],
  [目标协议], [`mygo.elm.ebi`], [projection 输出目标是 EBI 协议对象。],
  [projector], [`mygo.elm.projector.soyo-to-ebi`], [把 ELM 单元对象投影成 EBI 的外部实现。],
  [relation type], [`mygo.elm.depends-on`], [两个对象之间的依赖关系。],
  [归档目标], [`posix.tar.archive`], [projection 输出目标是 tar 归档字节流。],
)

非公开 identifier 若需要通用工具展示，应当在 Schema Table 中提供 descriptor，或通过非强制展示 TLV 附带 schema bundle 说明。SCHEMA_BUNDLE TLV 不得作为 Schema Table descriptor 的来源。任何对象是否可执行、可转换或可导出，只由实现显式支持决定，不得由 identifier 前缀自动推断。

== 6. Blob Table

Blob Table 描述文件中的原始字节块或全零字节块。Blob 只表达“字节在哪里”和“字节是否完整”，不表达“字节是什么”。代码段、数据段、TLS 模板、ELM payload、tar entry、调试符号或其它业务用途均由对象 schema 或 projector 解释。

每个 Blob 记录为 96 字节。

#table(
  columns: (0.7fr, 0.6fr, 1.8fr, 3.2fr),
  inset: 5pt,
  stroke: line-stroke,
  align: (left, center, left, left),
  table.header(
    head[偏移],
    head[大小],
    head[字段],
    head[说明],
  ),
  [`0x00`], [8], [`blob_id`], [文件内唯一 blob id。0 保留，不得使用。],
  [`0x08`], [4], [`flags`], [blob 标志。v1 必须为 0。],
  [`0x0c`], [2], [`source_kind`], [payload 来源类型。],
  [`0x0e`], [2], [`hash_alg`], [完整性哈希算法。],
  [`0x10`], [8], [`file_offset`], [来源为文件范围时的文件偏移。],
  [`0x18`], [8], [`size`], [blob 逻辑字节数；哈希按逻辑内容计算。],
  [`0x20`], [8], [`stored_size`], [文件中存储字节数；未压缩时等于 size。],
  [`0x28`], [4], [`align`], [建议对齐，0 表示无约束；非 0 时必须为 2 的幂。],
  [`0x2c`], [4], [`compression`], [压缩算法；0 表示不压缩。],
  [`0x30`], [4], [`media_type_off`], [内容类型 identifier 偏移；无内容类型时为 0。],
  [`0x34`], [4], [`reserved0`], [保留，必须为 0。],
  [`0x38`], [32], [`content_hash`], [blob 逻辑内容完整性哈希；无哈希时全 0。],
  [`0x58`], [8], [`external_ref_id`], [source_kind 为 EXTERNAL_REF 时引用 External Reference Table；其它来源时必须为 0。],
)

source_kind 使用以下值。

#table(
  columns: (0.7fr, 1.6fr, 3.8fr),
  inset: 6pt,
  stroke: line-stroke,
  align: (center, left, left),
  table.header(
    head[值],
    head[名称],
    head[含义],
  ),
  [1], [`FILE_RANGE`], [`file_offset..file_offset+stored_size` 指向 SOYO 文件中的存储字节。],
  [2], [`ZERO`], [全零字节块，file_offset 必须为 0。],
  [3], [`EXTERNAL_REF`], [blob 内容由 External Reference Table 指向的外部对象提供。Core 不自动取回。],
)

hash_alg 使用以下值。

#table(
  columns: (0.7fr, 1.6fr, 3.8fr),
  inset: 6pt,
  stroke: line-stroke,
  align: (center, left, left),
  table.header(
    head[值],
    head[名称],
    head[含义],
  ),
  [0], [`HASH_NONE`], [不提供完整性哈希，content_hash 必须全 0。],
  [1], [`BLAKE3_256`], [32 字节 BLAKE3 哈希。],
  [2], [`SHA256`], [32 字节 SHA-256 哈希。],
)

compression 使用以下值：

- 0：NONE，不压缩，stored_size 必须等于 size。
- 1：DEFLATE，stored bytes 使用 DEFLATE 压缩，解压后字节数必须等于 size。
- 2：ZSTD，stored bytes 使用 Zstandard 压缩，解压后字节数必须等于 size。
- 3-0xffffffff：保留，v1 必须拒绝。

FILE_RANGE blob 的文件范围由 `file_offset..file_offset+stored_size` 描述，必须位于 SOYO 文件范围内。计算 `file_offset + stored_size` 前必须检查整数溢出。FILE_RANGE blob 的文件范围在任何情况下都不得与 Header、String Table、Blob Table、Object Table、Schema Table、Projection Table、Relation Table、Metadata Binding Table、External Reference Table 或 TLV 链范围发生任何字节重叠。此约束为硬性文件格式约束，优先级高于任何 schema 定义；SOYO Core 不承认任何 schema 具有覆盖或别名化核心控制结构的权限。hash_alg != HASH_NONE 时，实现必须按 compression 解码后校验逻辑内容的 content_hash。ZERO blob 的 file_offset、stored_size、external_ref_id 和 compression 必须为 0；hash_alg != HASH_NONE 时，content_hash 必须等于对应长度全零字节串的完整性哈希。EXTERNAL_REF blob 的 file_offset、stored_size 和 compression 必须为 0，external_ref_id 必须引用 External Reference Table 中 kind 为 BLOB 的记录；是否解析该外部字节块由 projector 或工具决定。

media_type_off 非 0 时必须指向合法 identifier。SOYO Core 不根据 media_type_off 推断可执行、可投影或可导出语义；该字段只为工具、projector 和 schema 提供内容类型提示。

== 7. Object Table

Object Table 描述 SOYO 文件中的对象节点。对象是 schema 解释的最小主语义单元。SOYO Core 只校验对象记录、payload 范围、schema 引用和 required 规则，不解释对象 payload。

每个 Object 记录为 80 字节。

#table(
  columns: (0.7fr, 0.6fr, 1.8fr, 3.2fr),
  inset: 5pt,
  stroke: line-stroke,
  align: (left, center, left, left),
  table.header(
    head[偏移],
    head[大小],
    head[字段],
    head[说明],
  ),
  [`0x00`], [8], [`object_id`], [文件内唯一对象 id。0 保留，不得使用。],
  [`0x08`], [4], [`type_id_off`], [对象类型 identifier 在 String Table 中的偏移，不得为 0。],
  [`0x0c`], [4], [`schema_id`], [解释 payload 的 schema id，必须引用 Schema Table。],
  [`0x10`], [4], [`flags`], [对象标志。],
  [`0x14`], [4], [`reserved0`], [保留，必须为 0。],
  [`0x18`], [8], [`payload_blob_id`], [承载 payload 的 blob id；无 payload 时为 0。],
  [`0x20`], [8], [`payload_offset`], [payload 在 blob 内的偏移。],
  [`0x28`], [8], [`payload_size`], [payload 字节数。],
  [`0x30`], [4], [`name_off`], [对象展示名或稳定名偏移；无名称时为 0。],
  [`0x34`], [4], [`namespace_off`], [命名空间偏移；无命名空间时为 0。],
  [`0x38`], [8], [`reserved1`], [保留，必须为 0。],
  [`0x40`], [16], [`reserved2`], [保留，必须全 0。],
)

Object flags 使用以下 bit。

#table(
  columns: (0.7fr, 1.8fr, 3.6fr),
  inset: 6pt,
  stroke: line-stroke,
  align: (center, left, left),
  table.header(
    head[bit],
    head[名称],
    head[含义],
  ),
  [0], [`REQUIRED`], [当前投影或导出路径需要该对象时，若不认识其 schema，必须失败。],
  [1], [`PRIVATE`], [该对象只供同文件内其它对象或 projector 使用。],
  [2], [`DISPLAY_ONLY`], [对象仅用于展示或诊断，不参与执行语义。],
  [3-31], [保留], [v1 必须为 0。],
)

payload_blob_id 为 0 时，payload_offset 和 payload_size 必须均为 0。payload_blob_id 非 0 时必须引用存在的 blob，且 `payload_offset + payload_size` 不得超过 blob.size。

对象是否可执行、可转换或可导出，不由 type_id 决定，而由 Projection Table 中的投影声明和实际可用 projector 决定。

== 8. Schema Table

Schema Table 描述对象 payload、relation payload、projection 配置或 metadata 的解释方式。Schema Table 是 SOYO 的类型目录，不是业务逻辑。SOYO Core 不根据 schema identifier 执行 EBI、PROCESS、tar 或其它具体语义。

每个 Schema 记录为 96 字节。

#table(
  columns: (0.7fr, 0.6fr, 1.8fr, 3.2fr),
  inset: 5pt,
  stroke: line-stroke,
  align: (left, center, left, left),
  table.header(
    head[偏移],
    head[大小],
    head[字段],
    head[说明],
  ),
  [`0x00`], [4], [`schema_id`], [文件内唯一 schema id。0 保留，不得使用。],
  [`0x04`], [2], [`kind`], [schema 类别。],
  [`0x06`], [2], [`format`], [schema descriptor 格式。],
  [`0x08`], [4], [`flags`], [schema 标志。],
  [`0x0c`], [4], [`schema_name_off`], [schema identifier 偏移，不得为 0。],
  [`0x10`], [4], [`schema_version_off`], [schema 版本字符串偏移，不得为 0。],
  [`0x14`], [4], [`display_name_off`], [展示名称偏移；无展示名称时为 0。],
  [`0x18`], [8], [`descriptor_blob_id`], [schema descriptor 所在 blob；无 descriptor 时为 0。],
  [`0x20`], [8], [`descriptor_offset`], [descriptor 在 blob 内的偏移。],
  [`0x28`], [8], [`descriptor_size`], [descriptor 字节数。],
  [`0x30`], [2], [`hash_alg`], [schema_hash 算法。],
  [`0x32`], [2], [`reserved0`], [保留，必须为 0。],
  [`0x34`], [4], [`reserved1`], [保留，必须为 0。],
  [`0x38`], [32], [`schema_hash`], [schema descriptor 或外部 schema 的完整性哈希；无哈希时全 0。],
  [`0x58`], [8], [`descriptor_ref_id`], [外部 descriptor 引用 id；仅 format 为 EXTERNAL_DESCRIPTOR 时引用 External Reference Table，其它格式必须为 0。],
)

schema kind 使用以下值。

#table(
  columns: (0.7fr, 1.8fr, 3.6fr),
  inset: 6pt,
  stroke: line-stroke,
  align: (center, left, left),
  table.header(
    head[值],
    head[名称],
    head[含义],
  ),
  [1], [`OBJECT_PAYLOAD`], [解释对象 payload。],
  [2], [`RELATION_PAYLOAD`], [解释 relation payload。],
  [3], [`PROJECTION_CONFIG`], [解释 projection 配置。],
  [4], [`METADATA`], [解释 metadata 或 TLV payload。],
  [5], [`VIEW`], [只用于展示或检查工具的视图 schema。],
)

schema descriptor format 使用以下值。

#table(
  columns: (0.7fr, 2fr, 3.4fr),
  inset: 6pt,
  stroke: line-stroke,
  align: (center, left, left),
  table.header(
    head[值],
    head[名称],
    head[含义],
  ),
  [0], [`NONE`], [不提供 descriptor，只声明 schema 身份。],
  [1], [`EMBEDDED_DESCRIPTOR`], [descriptor 位于本文件 blob 中；SOYO Core 只校验边界和哈希。],
  [2], [`EXTERNAL_DESCRIPTOR`], [descriptor 由 External Reference Table 中 kind 为 SCHEMA 的记录提供；Core 不自动取回。],
)

schema flags 使用以下 bit。

#table(
  columns: (0.7fr, 1.8fr, 3.6fr),
  inset: 6pt,
  stroke: line-stroke,
  align: (center, left, left),
  table.header(
    head[bit],
    head[名称],
    head[含义],
  ),
  [0], [`REQUIRED`], [引用该 schema 的 required 对象、relation 或 projection 必须能被当前实现识别。],
  [1], [`EMBEDDED_DESCRIPTOR`], [descriptor 必须来自本文件 blob，且 format 必须为 EMBEDDED_DESCRIPTOR。],
  [2], [`EXTERNAL_ALLOWED`], [descriptor 必须通过 External Reference Table 显式引用，且 format 必须为 EXTERNAL_DESCRIPTOR。],
  [3], [`DISPLAY_ONLY`], [该 schema 只用于展示，不参与执行语义。],
  [4], [`SEALED`], [schema_hash 必须匹配 descriptor 或外部 schema。],
  [5], [`DEPRECATED`], [schema 已废弃但仍可读。],
  [6-31], [保留], [v1 必须为 0。],
)

format 为 NONE 时，descriptor_blob_id、descriptor_offset、descriptor_size 和 descriptor_ref_id 必须全为 0，且不得设置 EMBEDDED_DESCRIPTOR、EXTERNAL_ALLOWED 或 SEALED。format 为 EMBEDDED_DESCRIPTOR 时，必须设置 EMBEDDED_DESCRIPTOR，不得设置 EXTERNAL_ALLOWED，descriptor_blob_id 和 descriptor_size 必须非 0，descriptor_ref_id 必须为 0，且 descriptor 范围不得越过 blob.size。format 为 EXTERNAL_DESCRIPTOR 时，必须设置 EXTERNAL_ALLOWED，不得设置 EMBEDDED_DESCRIPTOR，descriptor_blob_id、descriptor_offset 和 descriptor_size 必须为 0，descriptor_ref_id 必须引用 External Reference Table 中 kind 为 SCHEMA 的记录。SEALED schema 必须满足 hash_alg != HASH_NONE 且 schema_hash 非全 0，并且只能用于 EMBEDDED_DESCRIPTOR 或 EXTERNAL_DESCRIPTOR。非 SEALED schema 若 hash_alg == HASH_NONE，schema_hash 必须全 0。v1 不定义结构兼容算法；schema 是否兼容由 schema identifier、schema version、schema_hash 和实际 projector 共同决定。

== 9. Projection Table

Projection Table 声明某个对象可以通过某个 projector 投影为某个目标协议对象或目标文件。Projection Table 是 SOYO 文件的可操作语义中心：它告诉工具这个对象图可以被看作什么，但不授予执行权限。SOYO Core 不理解目标协议，也不执行 projector。v1 projector 必须由运行环境、工具链或内核显式提供。

每个 Projection 记录为 80 字节。

#table(
  columns: (0.7fr, 0.6fr, 1.8fr, 3.2fr),
  inset: 5pt,
  stroke: line-stroke,
  align: (left, center, left, left),
  table.header(
    head[偏移],
    head[大小],
    head[字段],
    head[说明],
  ),
  [`0x00`], [8], [`projection_id`], [文件内唯一 projection id。0 保留，不得使用。],
  [`0x08`], [8], [`source_object_id`], [源对象 id，必须引用 Object Table。],
  [`0x10`], [4], [`input_schema_id`], [投影器期望的根输入 schema id，必须引用 Schema Table，且必须等于 source object 的 schema_id。],
  [`0x14`], [4], [`config_schema_id`], [projection 配置 schema id；无配置时为 0。],
  [`0x18`], [4], [`target_protocol_id_off`], [目标协议 identifier 偏移，不得为 0。],
  [`0x1c`], [4], [`target_protocol_version_off`], [目标协议版本偏移；无版本要求时为 0。],
  [`0x20`], [4], [`projector_id_off`], [projector identifier 偏移，不得为 0。],
  [`0x24`], [4], [`projector_version_off`], [projector 版本偏移；无版本要求时为 0。],
  [`0x28`], [2], [`projector_abi`], [projector 调用 ABI；0 表示由 projector identifier 自身定义。],
  [`0x2a`], [2], [`closure_policy`], [输入闭包策略。],
  [`0x2c`], [2], [`projector_source`], [projector 来源策略。v1 只允许 ENVIRONMENT_ONLY。],
  [`0x2e`], [2], [`trust_policy`], [信任策略。],
  [`0x30`], [4], [`flags`], [projection 标志。],
  [`0x34`], [4], [`priority`], [多个 projection 的排序键，数值小者优先。],
  [`0x38`], [8], [`config_blob_id`], [projection 配置 blob；无配置时为 0。],
  [`0x40`], [8], [`config_offset`], [配置在 blob 内的偏移。],
  [`0x48`], [8], [`config_size`], [配置字节数。],
)

projection flags 使用以下 bit。

#table(
  columns: (0.7fr, 1.8fr, 3.6fr),
  inset: 6pt,
  stroke: line-stroke,
  align: (center, left, left),
  table.header(
    head[bit],
    head[名称],
    head[含义],
  ),
  [0], [`PRIMARY`], [该 projection 是 source object 在对应目标协议下的默认可操作视图。同一 source_object_id、target_protocol_id 与 target_protocol_version 组合下不得包含多个 PRIMARY projection。],
  [1], [`HIDDEN`], [通用工具默认不展示该 projection。],
  [2], [`DIAGNOSTIC`], [投影结果用于诊断或展示。],
  [3], [`LOSSLESS`], [声明该投影应当无损。projector 无法保证无损时必须失败。],
  [4], [`LOSSY_ALLOWED`], [允许有损投影。],
  [5], [`REQUIRES_TRUSTED_INPUT`], [执行投影前必须由 projector 验证输入闭包的信任对象。],
  [6-31], [保留], [v1 必须为 0。],
)

LOSSLESS 和 LOSSY_ALLOWED 不得同时设置。PRIMARY 不得与 DIAGNOSTIC 或 HIDDEN 同时设置。REQUIRES_TRUSTED_INPUT 必须与 trust_policy == TRUSTED_INPUT_REQUIRED 保持一致：设置其中任一项时，另一项也必须表达相同信任要求；否则 projection 格式无效。TRUSTED_INPUT_REQUIRED 不得与 PROJECTOR_DEFINED closure_policy 同时使用；可信输入投影必须能在验签前由 SOYO Core 规则确定最低输入闭包。

closure_policy 使用以下值。

#table(
  columns: (0.7fr, 1.8fr, 3.6fr),
  inset: 6pt,
  stroke: line-stroke,
  align: (center, left, left),
  table.header(
    head[值],
    head[名称],
    head[含义],
  ),
  [0], [`SELF_ONLY`], [输入闭包只包含 source_object_id 指向的对象。],
  [1], [`REQUIRED_RELATIONS`], [输入闭包包含从 source object 出发经 REQUIRED relation 可达的对象。],
  [2], [`DECLARED_RELATIONS`], [输入闭包由 projector 解析 projection config 后按声明的 relation type 集合收集；Core 只校验配置边界。],
  [3], [`PROJECTOR_DEFINED`], [输入闭包由 projector 决定，但 projector 必须能输出诊断信息。],
)

projector_source 使用以下值。

#table(
  columns: (0.7fr, 1.8fr, 3.6fr),
  inset: 6pt,
  stroke: line-stroke,
  align: (center, left, left),
  table.header(
    head[值],
    head[名称],
    head[含义],
  ),
  [0], [`ENVIRONMENT_ONLY`], [projector 必须由运行环境、工具链或内核显式提供。SOYO Core 不加载、不执行、不下载 projector。],
)

trust_policy 使用以下值。

#table(
  columns: (0.7fr, 1.8fr, 3.6fr),
  inset: 6pt,
  stroke: line-stroke,
  align: (center, left, left),
  table.header(
    head[值],
    head[名称],
    head[含义],
  ),
  [0], [`UNSPECIFIED`], [由 projector 或调用方决定信任要求。],
  [1], [`TRUSTED_INPUT_REQUIRED`], [投影前必须验证输入闭包的签名或信任证明。],
)

config_blob_id 为 0 时，config_offset 和 config_size 必须均为 0，config_schema_id 必须为 0。config_blob_id 非 0 时必须引用存在的 blob，且配置范围不得越过 blob.size，config_schema_id 必须引用 Schema Table。配置 payload 的语义由 projector 规范解释。closure_policy 为 DECLARED_RELATIONS 时，config_blob_id 必须引用本文件内的非 EXTERNAL_REF blob；否则 projector 需要先解析外部内容才能确定输入闭包，会破坏闭包收集与信任验证的启动顺序。

Projection 选择规则如下：调用方指定 source object 和目标协议时，通用工具应当把该 source object 在该目标协议下的 PRIMARY projection 作为默认可操作视图；同一 source_object_id、target_protocol_id 与 target_protocol_version 组合下存在多个 PRIMARY projection 时，文件格式无效。调用方未指定 source object 或目标协议时，通用工具不得假设存在全局唯一默认操作，应展示所有可用 PRIMARY projection；没有 PRIMARY 但存在 projection 时，工具应按 priority 排序展示；没有 projection 时，SOYO 文件只能作为对象图展示，不具备默认执行、导出或转换语义。

示例：一个 ELM 对象可以声明目标协议 identifier 为 `mygo.elm.ebi`，projector identifier 为 `mygo.elm.projector.soyo-to-ebi`。SOYO Core 仍然不理解 EBI 字段；实际转换由该 projector 完成。

示例：一个归档对象可以声明目标协议 identifier 为 `posix.tar.archive`，projector identifier 为 `mygo.soyo.projector.tar`。SOYO Core 仍然不理解 tar header、checksum 或 pax 扩展；实际导出由 tar projector 完成。

== 10. Relation Table

Relation Table 描述对象之间的有向关系。Relation 可以表达依赖、包含、拓展、入口、调试链接、归档成员、资源归属等关系，但 SOYO Core 不解释 relation_type_id 的业务含义。

每个 Relation 记录为 80 字节。

#table(
  columns: (0.7fr, 0.6fr, 1.8fr, 3.2fr),
  inset: 5pt,
  stroke: line-stroke,
  align: (left, center, left, left),
  table.header(
    head[偏移],
    head[大小],
    head[字段],
    head[说明],
  ),
  [`0x00`], [8], [`relation_id`], [文件内唯一 relation id。0 保留，不得使用。],
  [`0x08`], [8], [`source_object_id`], [源对象 id，必须引用 Object Table。],
  [`0x10`], [8], [`target_object_id`], [目标对象 id；无目标对象时为 0。],
  [`0x18`], [4], [`schema_id`], [解释 relation payload 的 schema id；无 payload 时可为 0。],
  [`0x1c`], [4], [`flags`], [relation 标志。],
  [`0x20`], [4], [`relation_type_id_off`], [关系类型 identifier 偏移，不得为 0。],
  [`0x24`], [4], [`name_off`], [关系展示名或稳定名偏移；无名称时为 0。],
  [`0x28`], [8], [`payload_blob_id`], [relation payload 所在 blob；无 payload 时为 0。],
  [`0x30`], [8], [`payload_offset`], [payload 在 blob 内的偏移。],
  [`0x38`], [8], [`payload_size`], [payload 字节数。],
  [`0x40`], [16], [`reserved0`], [保留，必须全 0。],
)

relation flags 使用以下 bit。

#table(
  columns: (0.7fr, 1.8fr, 3.6fr),
  inset: 6pt,
  stroke: line-stroke,
  align: (center, left, left),
  table.header(
    head[bit],
    head[名称],
    head[含义],
  ),
  [0], [`REQUIRED`], [当前投影路径要求该关系可被理解；不支持时必须失败。],
  [1], [`ORDERED`], [同类关系存在顺序语义，顺序由 relation_id 或 payload 定义。],
  [2], [`WEAK`], [目标对象缺失时不阻断非 strict 投影。],
  [3], [`PRIVATE`], [该关系只供同文件内其它对象或 projector 使用。],
  [4-31], [保留], [v1 必须为 0。],
)

REQUIRED 和 WEAK 不得同时设置。REQUIRED relation 必须引用非 0 target_object_id；无目标 relation 只能用于非 REQUIRED 关系。ORDERED 只声明同类关系存在顺序语义，不改变闭包遍历必须按 relation_id 升序处理的 Core 规则。

payload_blob_id 为 0 时，payload_offset 和 payload_size 必须均为 0，schema_id 可以为 0。payload_blob_id 非 0 时必须引用存在的 blob，schema_id 必须引用 Schema Table，且 payload 范围不得越过 blob.size。

Projection 输入闭包使用 Relation Table 收集依赖对象。闭包内 REQUIRED relation 指向的对象必须存在且可被当前 projector 理解；闭包内 unknown required object、schema 或 relation 会导致当前 projection 失败。闭包外的未知对象、schema 或 relation 不影响当前 projection。WEAK relation 的目标对象缺失时不阻断非 strict projector，但 projector 可以把该缺失记录为诊断信息。

=== 10.1 闭包遍历约束

当实现为 projection 收集输入闭包时，必须把 Relation Table 视为有向图，只沿 `source_object_id -> target_object_id` 方向遍历。遍历顺序必须确定：同一 source object 的候选 relation 按 relation_id 升序处理，最终闭包对象集合按 object_id 升序输出。`SELF_ONLY` 闭包只包含 source object；`REQUIRED_RELATIONS` 只跟随 flags 包含 REQUIRED 的 relation；`DECLARED_RELATIONS` 的 relation type 集合由 projector 解析 projection config 后决定，SOYO Core 只校验 config blob 边界和 config_schema_id 引用；`PROJECTOR_DEFINED` 完全由 projector 决定，但 projector 必须能输出诊断信息。

闭包遍历必须维护已访问 object_id 集合。遇到已访问对象时，实现必须停止继续展开该边，且不得递归进入同一对象；这不是 Core 级格式错误。是否禁止某类 relation 构成环，由 relation schema、目标协议或 projector 规定。若对应 schema 或 projector 声明该关系必须为 DAG，则检测到环时当前 projection 必须失败。

实现必须设置闭包资源上限，包括最大闭包对象数、最大遍历 relation 数和最大遍历深度。用户态工具应当至少支持 1024 层深度；内核态解析器可以采用更小限制。超过资源上限时，当前 projection 必须以资源限制失败，不得把部分闭包当作完整输入继续投影。

== 11. Metadata Binding Table

Metadata Binding Table 把 TLV 元数据绑定到文件、blob、对象、schema、projection、relation 或外部引用。TLV 不再直接挂在 Object Table 中，所有 metadata 绑定都通过本表表达。

每个 Metadata Binding 记录为 32 字节。

#table(
  columns: (0.7fr, 0.6fr, 1.8fr, 3.2fr),
  inset: 5pt,
  stroke: line-stroke,
  align: (left, center, left, left),
  table.header(
    head[偏移],
    head[大小],
    head[字段],
    head[说明],
  ),
  [`0x00`], [8], [`binding_id`], [文件内唯一 metadata binding id。0 保留，不得使用。],
  [`0x08`], [2], [`owner_kind`], [metadata 归属对象类别。],
  [`0x0a`], [2], [`flags`], [metadata binding 标志。],
  [`0x0c`], [4], [`metadata_schema_id`], [metadata schema id；MANDATORY 绑定必须非 0，非强制绑定无 schema 时为 0。],
  [`0x10`], [8], [`owner_id`], [归属对象 id；FILE 归属时为 0。],
  [`0x18`], [4], [`tlv_first`], [TLV 链中同文件偏移顺序的首个 TLV ordinal。],
  [`0x1c`], [4], [`tlv_count`], [绑定的 TLV 数量。],
)

owner_kind 使用以下值。

#table(
  columns: (0.7fr, 1.8fr, 3.6fr),
  inset: 6pt,
  stroke: line-stroke,
  align: (center, left, left),
  table.header(
    head[值],
    head[名称],
    head[含义],
  ),
  [1], [`FILE`], [metadata 绑定到整个 SOYO 文件。],
  [2], [`BLOB`], [metadata 绑定到 Blob Table 记录。],
  [3], [`OBJECT`], [metadata 绑定到 Object Table 记录。],
  [4], [`SCHEMA`], [metadata 绑定到 Schema Table 记录。],
  [5], [`PROJECTION`], [metadata 绑定到 Projection Table 记录。],
  [6], [`RELATION`], [metadata 绑定到 Relation Table 记录。],
  [7], [`EXTERNAL_REF`], [metadata 绑定到 External Reference Table 记录。],
)

metadata binding flags 使用以下 bit。

#table(
  columns: (0.7fr, 1.8fr, 3.6fr),
  inset: 6pt,
  stroke: line-stroke,
  align: (center, left, left),
  table.header(
    head[bit],
    head[名称],
    head[含义],
  ),
  [0], [`MANDATORY`], [绑定的 TLV 必须可识别、可解析并可安装；失败只影响 owner。],
  [1], [`DISPLAY_ONLY`], [绑定的 TLV 只用于展示。],
  [2], [`DIAGNOSTIC`], [绑定的 TLV 用于诊断。],
  [3-15], [保留], [v1 必须为 0。],
)

MANDATORY 不得与 DISPLAY_ONLY 或 DIAGNOSTIC 同时设置。MANDATORY binding 必须指定 metadata_schema_id，且该 schema 的 kind 必须为 METADATA 或 VIEW。非 MANDATORY binding 的 metadata_schema_id 可以为 0；非 0 时同样必须引用 METADATA 或 VIEW schema。metadata_schema_id 解释该 binding 覆盖的完整 TLV ordinal 范围，而不是只解释首个 TLV 或每个 TLV 的局部片段。

tlv_first 和 tlv_count 以 TLV 链文件偏移顺序计算 ordinal。tlv_count 必须非 0。绑定范围必须完全落在 TLV 链内。同一 owner 可以有多个 metadata binding，按 binding_id 升序应用。非 MANDATORY binding 范围可以重叠；同一 owner 下的 MANDATORY binding 范围不得互相重叠。不同 owner 可以绑定同一段 TLV，表示共享同一份 metadata 事实；共享范围内任一 mandatory metadata 解析失败时，只拒绝对应 owner 参与的 projection 或展示路径，不得把失败扩散到无关 owner 或无关 projection。

== 12. External Reference Table

External Reference Table 声明文件外部的 schema、blob、对象、SOYO 文件、projector 或信任锚。SOYO Core 只记录引用，不自动取回、不自动下载、不自动执行外部内容。

每个 External Reference 记录为 80 字节。

#table(
  columns: (0.7fr, 0.6fr, 1.8fr, 3.2fr),
  inset: 5pt,
  stroke: line-stroke,
  align: (left, center, left, left),
  table.header(
    head[偏移],
    head[大小],
    head[字段],
    head[说明],
  ),
  [`0x00`], [8], [`external_ref_id`], [文件内唯一外部引用 id。0 保留，不得使用。],
  [`0x08`], [2], [`kind`], [外部引用类别。],
  [`0x0a`], [2], [`flags`], [外部引用标志。],
  [`0x0c`], [4], [`reserved0`], [保留，必须为 0。],
  [`0x10`], [4], [`identifier_off`], [外部对象 identifier 偏移；无 identifier 时为 0。],
  [`0x14`], [4], [`version_off`], [外部对象版本偏移；无版本要求时为 0。],
  [`0x18`], [4], [`locator_off`], [resolver 私有定位 identifier 偏移；无定位信息时为 0。],
  [`0x1c`], [4], [`namespace_off`], [resolver 命名空间 identifier 偏移；无命名空间时为 0。],
  [`0x20`], [2], [`hash_alg`], [content_hash 算法。],
  [`0x22`], [2], [`reserved1`], [保留，必须为 0。],
  [`0x24`], [4], [`reserved2`], [保留，必须为 0。],
  [`0x28`], [32], [`content_hash`], [外部内容完整性哈希；无哈希时全 0。],
  [`0x48`], [8], [`reserved3`], [保留，必须为 0。],
)

kind 使用以下值。

#table(
  columns: (0.7fr, 1.8fr, 3.6fr),
  inset: 6pt,
  stroke: line-stroke,
  align: (center, left, left),
  table.header(
    head[值],
    head[名称],
    head[含义],
  ),
  [1], [`SCHEMA`], [外部 schema。],
  [2], [`BLOB`], [外部字节块。],
  [3], [`OBJECT`], [外部对象。],
  [4], [`SOYO_FILE`], [外部 SOYO 文件。],
  [5], [`PROJECTOR`], [外部 projector。v1 只能用于诊断，不得自动执行。],
  [6], [`TRUST_ANCHOR`], [外部信任锚。],
)

external reference flags 使用以下 bit。

#table(
  columns: (0.7fr, 1.8fr, 3.6fr),
  inset: 6pt,
  stroke: line-stroke,
  align: (center, left, left),
  table.header(
    head[bit],
    head[名称],
    head[含义],
  ),
  [0], [`REQUIRED`], [引用无法解析时，相关 projection 必须失败。],
  [1], [`DISPLAY_ONLY`], [引用只用于展示。],
  [2-15], [保留], [v1 必须为 0。],
)

REQUIRED external reference 必须设置 hash_alg != HASH_NONE，且 content_hash 必须非全 0。参与 projection 的 REQUIRED external reference 若无法解析或 content_hash 校验失败，当前 projection 必须失败。

External Reference Table 不定义网络访问、文件系统访问或包管理语义。locator_off 和 namespace_off 均为 resolver 私有 identifier；SOYO Core 不得把它们解释为文件路径、包名、网络地址或下载地址。只有显式支持对应 namespace 的 projector、工具或运行环境才可以解析 locator。是否解析外部引用由 projector 或工具决定；Core 不得自动取回外部内容。外部 projector 在 v1 中不得作为可执行来源，只能用于诊断或显式工具操作。

== 13. Trust Boundary

SOYO v1 不定义完整证书体系，也不要求 SOYO Core 强制验签。信任语义由目标运行时、projector 和调用方决定。但为了让可执行投影、内核投影和外部对象投影具备稳定安全边界，SOYO v1 定义标准信任对象和签名覆盖范围。

标准信任对象使用普通 Object Table 表达：

```text
type_id = soyo.trust.signature
schema = soyo.trust.signature.v1
```

当 Projection flags 设置 REQUIRES_TRUSTED_INPUT，或 trust_policy 为 TRUSTED_INPUT_REQUIRED 时，projector 必须先按第 10 章收集当前 projection 的输入闭包，然后按标准信任对象发现规则查找候选信任对象。至少一个标准信任对象必须满足第 13.1 节的最低覆盖集、通过第 13.2 节的规范化字节流验证，并通过第 13.3 节的 payload、密钥锚和签名算法校验；否则当前 projection 必须失败。

标准信任对象发现规则如下：实现必须扫描 Object Table 中 type_id 为 `soyo.trust.signature` 且 schema 为 `soyo.trust.signature.v1` 的对象，把它们作为候选信任对象。候选信任对象不要求通过 relation 进入普通输入闭包；只有当其 coverage 完整覆盖当前 projection 的最低覆盖集时，才可用于满足 TRUSTED_INPUT_REQUIRED。扫描候选对象不得赋予其它对象执行语义，也不得把候选对象自动加入 projector 的业务输入闭包。

签名覆盖范围必须按规范化顺序计算。v1 预留以下 canonical coverage 类别：

- Header 的所有字段。
- String Table、Blob Table、Object Table、Schema Table、Projection Table、Relation Table、Metadata Binding Table 和 External Reference Table。
- 被签名对象选择的 TLV ordinal 范围。
- 被签名对象选择的 blob 字节范围。
- 被签名对象选择的外部引用记录及其 content_hash。

签名对象不得覆盖自己的签名字节字段。具体证书链格式、撤销策略和本地信任策略由目标运行环境定义。SOYO Core 只保证这些字节范围可被稳定定位和规范化排序，不自动信任任何签名对象。

=== 13.1 可信投影最低覆盖集

当 projection 要求 TRUSTED_INPUT_REQUIRED 时，成功验签的标准信任对象 coverage 必须至少覆盖当前 projection 的最低覆盖集。coverage 可以包含额外 chunk；额外 chunk 不改变最低覆盖集要求。若最低覆盖集中要求覆盖某个表记录，而 SOYO_CANONICAL_STREAM_V1 只定义该表的完整表级 chunk，则该要求等价于覆盖对应完整表级 chunk。

最低覆盖集包含以下内容：

- Header 完整字节和 String Table 完整字节。
- Blob Table、Object Table、Schema Table、Projection Table、Relation Table 和 Metadata Binding Table 中与当前 projection 输入闭包相关的记录；在 v1 中这些记录通过对应完整表级 chunk 覆盖。
- 当前 projection 记录、source object 记录、输入闭包内所有 object 记录，以及这些 object 的 payload blob 字节范围。
- 输入闭包遍历实际使用的 REQUIRED relation 或 DECLARED relation 记录，以及这些 relation 的 payload blob 字节范围。
- 输入闭包内 object、relation、projection config、mandatory metadata 和 required external reference 引用的 schema 记录。若 schema 使用 EMBEDDED_DESCRIPTOR，则必须覆盖 descriptor blob 字节范围；若 schema 使用 EXTERNAL_DESCRIPTOR，则必须覆盖对应 External Reference 记录。
- 当前 projection 的 config blob 字节范围；若 config_blob_id 为 0，则无此项。
- 当前 projection、输入闭包对象、闭包 relation、闭包 schema、闭包 external reference 和 FILE owner 上参与当前 projection 的 MANDATORY metadata binding 记录，以及这些 binding 覆盖的完整 TLV 条目。非 mandatory metadata 只有在 projector 声明其影响投影结果时才进入最低覆盖集。
- 当前 projection 实际需要解析的 REQUIRED external reference 记录。外部引用记录可以由 External Reference Table 完整表级 chunk 覆盖，也可以由 kind 12 单记录 chunk 覆盖；同一 external reference record 不得同时使用两种覆盖方式。外部内容本体不进入 canonical stream；若需要约束外部内容，必须覆盖 External Reference 记录中的 content_hash，并由 resolver 校验外部内容哈希。
- 用于验证该信任对象的 TRUST_ANCHOR External Reference 记录。该记录可以由 External Reference Table 完整表级 chunk 覆盖，也可以由 kind 12 单记录 chunk 覆盖；同一 trust anchor record 不得同时使用两种覆盖方式。
- 该信任对象的 Object 记录和 payload blob 字节范围；payload 中 signature bytes 必须通过 zero range 置零后参与 canonical stream。

如果输入闭包内存在多个标准信任对象，只要其中一个信任对象完整覆盖最低覆盖集且验证成功，TRUSTED_INPUT_REQUIRED 即满足。其它信任对象格式错误或验签失败不得导致文件整体 malformed，但 projector 可以把它们记录为诊断信息；若没有任何信任对象满足最低覆盖集，当前 projection 必须失败。

=== 13.2 规范化字节流

需要验证信任对象时，projector 必须构造 `SOYO_CANONICAL_STREAM_V1`。该字节流只由文件中已经通过 Core 校验的 on-disk 字节生成，不得使用内存结构体布局、宿主机字节序或解析器私有 padding。

规范化字节流由若干 chunk 顺序拼接而成。每个 chunk 使用以下头部，所有整数均为 little-endian：

```text
kind: u16
flags: u16
id: u64
offset: u64
size: u64
payload: [u8; size]
```

flags 在 v1 中必须为 0。kind 使用以下值：1 表示 Header，2 表示 String Table，3 表示 Blob Table，4 表示 Object Table，5 表示 Schema Table，6 表示 Projection Table，7 表示 Relation Table，8 表示 Metadata Binding Table，9 表示 External Reference Table，10 表示 TLV 条目，11 表示 blob 字节范围，12 表示 external reference 记录。

coverage entry 生成 chunk 时必须满足以下映射规则：

- Header chunk 必须完整覆盖 Header，id 必须为 0，offset 必须为 0，size 必须等于 header_size。
- String Table chunk 必须完整覆盖 String Table，id 必须为 0，offset 必须等于 string_offset，size 必须等于 string_size。
- 表级 chunk 必须完整覆盖对应 on-disk 表范围，id 必须为 0，offset 必须等于表文件偏移，size 必须等于 `count * entry_size`。v1 不允许对通用表做部分覆盖。
- TLV chunk 必须完整覆盖一个 TLV 条目，id 必须为 TLV ordinal，offset 必须为该条目的文件偏移，size 必须为 8 + len。TLV 结构错误时不得构造 TLV chunk。
- blob 字节 chunk 的 id 必须为 blob_id，offset 必须为 blob 内偏移，size 必须非 0，且 `offset + size` 不得超过 blob.size。FILE_RANGE blob 的 payload 来自对应文件字节；ZERO blob 的 payload 为等长 0 字节；EXTERNAL_REF blob 没有本地字节，不得生成 kind 11 chunk，只能通过对应 External Reference 记录约束外部内容。
- external reference chunk 必须完整覆盖一个 External Reference 记录，id 必须为 external_ref_id，offset 必须为该记录的文件偏移，size 必须等于 external_ref_entry_size。

由 coverage entry 生成的所有 chunk 必须按 `(kind, id, offset, size)` 升序拼接。重复 chunk、指向不存在 Header/String Table/表/TLV/blob/external reference 的 coverage entry、部分表 coverage、越界 blob coverage、空 blob coverage 或 EXTERNAL_REF blob 字节 coverage 均使当前信任对象无效。

zero range 只允许作用于该信任对象自身 payload 所在的 blob 字节 chunk。规范化时，zero range 覆盖的 payload 字节必须用等长 0 字节替换；zero range 不得作用于 Header、String Table、任一通用表、TLV、其它对象 payload 或 external reference record。信任对象 signature bytes 所在的 blob 字节范围不得与任何其它对象 payload、relation payload、schema descriptor 或 projection config 字节范围重叠；若存在这种别名化，该信任对象无效。若 coverage 未包含该信任对象 payload 所在字节范围，或 zero range 无法完全覆盖 signature bytes，该信任对象无效。外部内容本体不进入规范化字节流；需要约束外部内容时，只覆盖 External Reference 记录及其 content_hash。

=== 13.3 标准签名对象 payload

`soyo.trust.signature.v1` 的对象 payload 必须使用以下固定布局。所有偏移均相对于该 trust object 的 payload 起始位置，所有整数均为 little-endian：

```text
version: u16                 // v1 必须为 1
algorithm: u16               // v1 只定义 1 = ED25519
digest_algorithm: u16        // v1 只定义 1 = BLAKE3_256
reserved0: u16               // 必须为 0
coverage_count: u32
zero_range_count: u32
coverage_offset: u32
zero_range_offset: u32
key_id_offset: u32
key_id_size: u32
signature_offset: u32
signature_size: u32
reserved1: [u8; 16]          // 必须全 0
```

coverage entry 使用以下布局：

```text
kind: u16
flags: u16                   // v1 必须为 0
id: u64
offset: u64
size: u64
```

zero range entry 使用以下布局：

```text
offset: u32
size: u32
```

coverage_count 和 zero_range_count 必须非 0。coverage_offset、zero_range_offset、key_id_offset、signature_offset 及其 size 范围必须完全位于 trust object payload 内，且不得发生整数溢出。coverage entry 和 zero range entry 自身不得越过 trust object payload，entry 数量乘以 entry 大小不得发生整数溢出。coverage entry 的 flags 必须为 0，且必须能按第 13.2 节映射为有效 chunk。zero range entry 的 size 必须非 0，且只能覆盖该 trust object payload 内的字节。

key_id_size 必须非 0。key_id 字节必须是合法 UTF-8 identifier，并且必须与 External Reference Table 中唯一一条 kind 为 TRUST_ANCHOR 的记录的 identifier_off 字符串逐字节相同。若不存在匹配 TRUST_ANCHOR、存在多个匹配 TRUST_ANCHOR、匹配记录无法由当前运行环境解析为可信公钥，或匹配记录设置了 hash_alg 但 resolver 返回内容的 content_hash 校验失败，则该信任对象无效。SOYO Core 不定义密钥文件格式；运行环境只能把 key_id 解释为信任锚 identifier，不得把它解释为路径、网络地址或包名。

algorithm 为 ED25519 时，signature_size 必须为 64。digest_algorithm 为 BLAKE3_256 时，验证 canonical stream 前必须对 `SOYO_CANONICAL_STREAM_V1` 计算 BLAKE3-256 digest，并以该 digest 作为签名消息。zero range 必须至少覆盖 signature 字节范围；若 zero range 无法完全覆盖 signature bytes，该信任对象无效。v1 不定义其它算法；遇到未知 algorithm 或 digest_algorithm 时，当前信任对象不可用于 TRUSTED_INPUT_REQUIRED projection。

== 14. 强制规则与能力边界

SOYO v1 的强制规则由 projection 输入闭包内的对象、schema、relation、external reference 和 metadata binding 共同表达。强制规则不赋予 SOYO Core 业务语义，只规定当前执行、投影或导出路径在不理解某项数据时必须失败。

通用规则如下：

- unknown optional object：不在当前 projection 输入闭包内时可以跳过。
- unknown required object：位于当前 projection 输入闭包内时必须失败。
- unknown optional schema：可以保留为不可解释对象，供展示工具显示。
- unknown required schema：引用它的闭包内 object、relation 或 projection config 必须失败。
- unknown projection：可以展示但不得假装完成投影；PRIMARY projection 不可用时，默认操作失败。
- unknown optional relation：不在当前 projection 输入闭包内时可以忽略。
- unknown required relation：位于当前 projection 输入闭包内时必须失败。
- unknown required external reference：被当前 projection 输入闭包引用时必须失败。
- unknown non-mandatory TLV：可以跳过。
- unknown mandatory metadata：引用它的 owner 参与当前 projection 时必须失败。

能力声明本身应当通过对象 schema 或 projection 配置表达。SOYO Core 不定义系统调用权限、ELM 权限、VFS 权限、网络权限或设备权限。若某个目标运行时需要安装能力或策略，projector 必须把对应对象和 metadata 转换为目标运行时理解的协议对象；无法表达或安装 required 能力时，投影必须失败。

== 15. TLV 扩展元数据

TLV 层承载可扩展附加元数据。未被 Metadata Binding Table 强制引用的 TLV 不参与最低装载条件。TLV 错误分为三类：TLV range error、TLV chain structural error 和 TLV payload decode error。被 MANDATORY metadata binding 引用的 TLV 必须被识别、解析并安装成功。

TLV 不承载对象主语义，不代替 Object Table、Schema Table、Projection Table、Relation Table、Metadata Binding Table、External Reference Table 或 Blob Table。入口、段映射、生命周期钩子、归档条目、绑定槽、投影核心字段和协议必需字段不得只放入 TLV。

每个 TLV 条目由 8 字节头部和可变长 payload 组成。

#table(
  columns: (0.7fr, 0.6fr, 1.8fr, 3.2fr),
  inset: 5pt,
  stroke: line-stroke,
  align: (left, center, left, left),
  table.header(
    head[偏移],
    head[大小],
    head[字段],
    head[说明],
  ),
  [`0x00`], [4], [`tag`], [类型标识，u32 little-endian。],
  [`0x04`], [4], [`len`], [payload 字节数，u32 little-endian。],
  [`0x08`], [`len`], [`payload`], [条目内容。],
)

TLV 链范围由 Header tlv_offset 和 tlv_size 唯一确定。tlv_offset、tlv_size 或 `tlv_offset + tlv_size` 越界属于 TLV range error，使整个 SOYO 文件格式无效。解析器必须只在 `tlv_offset..tlv_offset+tlv_size` 范围内扫描 TLV，不得把范围外的文件后缀或 padding 解释为 TLV。tag == 0 && len == 0 表示可选终止符；终止符之后到 TLV 链范围末尾的字节必须全部为 0。没有终止符时，解析器在正好消费完 tlv_size 字节后停止。tag == 0 && len != 0、TLV 头部截断、payload 越界或终止符后存在非 0 字节属于 TLV chain structural error。出现 TLV chain structural error 时，不依赖 TLV metadata 的对象图检查可以继续；所有 Metadata Binding 视为不可解析，引用 mandatory metadata 的 owner 不得参与 projection 或展示路径。由于 TLV ordinal 序列无法可靠建立，解析器必须先报告 TLV chain structural error，并跳过 metadata binding 的 tlv_first..tlv_first+tlv_count ordinal 范围校验，不得在坏链上构造部分 ordinal 结果。

=== 15.1 TLV payload 编码

除单个 tag 另有规定外，TLV payload 使用以下编码之一：

- bytes：不解释的字节序列。
- utf8：UTF-8 字符串，不要求以 NUL 结尾。
- kv：UTF-8 文本，每行一个 key=value 条目，行分隔符为 LF。空行必须忽略。
- tag-specific-binary：tag 专用二进制 payload，具体布局由该 tag 的规范小节定义。

解析器遇到不符合声明编码的 TLV payload 时，属于 TLV payload decode error。非 mandatory payload decode error 必须忽略该 TLV 条目；mandatory payload decode error 必须拒绝对应 owner 参与 projection 或展示路径。

=== 15.2 预定义 TLV tag

#table(
  columns: (0.5fr, 1.7fr, 1fr, 3.4fr),
  inset: 5pt,
  stroke: line-stroke,
  align: (center, left, left, left),
  table.header(
    head[tag],
    head[名称],
    head[编码],
    head[含义],
  ),
  [0], [`TLV_END`], [-], [终止符，仅允许 len == 0。],
  [1], [`BUILD_ID`], [bytes], [构建标识。长度应为 16、20 或 32 字节。],
  [2], [`DISPLAY_NAME`], [utf8], [展示名称。],
  [3], [`PACKAGE_NAME`], [utf8], [软件包或组件名称。],
  [4], [`PACKAGE_VERSION`], [utf8], [软件包或组件版本。],
  [5], [`BUILD_INFO`], [kv], [构建时间、构建模式、目标 triple 等构建信息。],
  [6], [`TOOLCHAIN_INFO`], [kv], [编译器、projector、SOYO 生产工具及其版本。],
  [7], [`SOURCE_INFO`], [kv], [仓库、提交、分支、源码路径等来源信息。],
  [8], [`FEATURE_SUMMARY`], [kv], [展示用功能摘要。],
  [9], [`RUNTIME_HINT`], [kv], [运行环境提示，例如所需配置名称或演示场景。],
  [10], [`SCHED_HINT`], [kv], [调度展示提示，例如 latency=low 或 class=interactive。],
  [11], [`MEMORY_HINT`], [kv], [内存展示提示，例如估计工作集或建议栈大小。],
  [12], [`PERF_HINT`], [kv], [性能展示提示，例如热路径名称或预期计数器。],
  [13], [`TEST_PROFILE`], [kv], [测试配置、测试套件名称或预期检查点。],
  [14], [`DEMO_NOTE`], [utf8], [面向展示工具的说明文本。],
  [15], [`POLICY_NOTE`], [kv], [策略说明；v1 不定义机器可安装策略。],
  [16], [`RESOURCE_BUDGET`], [kv], [CPU、内存、句柄和 I/O 预算说明；v1 不定义机器可安装策略。],
  [17], [`SCHEMA_BUNDLE`], [bytes], [展示或诊断用 schema 包提示；不得作为 Schema Table descriptor 来源。],
  [18], [`PROJECTOR_HINT`], [kv], [projector 选择、路径或调试提示。],
  [19], [`OBJECT_NOTE`], [utf8], [对象说明文本。],
  [20], [`RELATION_NOTE`], [utf8], [关系说明文本。],
)

预定义 TLV tag 的含义不得改变。废弃 tag 的编号必须保留。新增 tag 必须使用未分配编号。

v1 不定义任何 Core 级 mandatory 机器策略。具体执行策略可以由对象 schema 或 projector 规范定义，并通过 metadata binding 或 projection 配置引用。若目标运行时无法表达 required 策略，projector 必须失败。

== 16. 对象、schema、projection、relation 与外部引用校验

解析器、检查器、projector 或导出器在读取对象、投影或导出前必须完成以下校验：

- magic == `"soyo"`。
- version == 1。
- header_size == 192。
- file_size 必须等于实际文件大小，且不得小于 header_size。
- 所有 Header 保留字段为 0。
- 所有 entry_size 符合 v1 规定。
- 所有 `offset + size` 和 `offset + count * entry_size` 计算不得发生整数溢出。
- 任一表 count 为 0 时，offset 必须为 0。
- 任一表 count 非 0 时，`offset + count * entry_size` 必须位于文件范围内。
- String Table 必须存在且 offset 0 为 NUL。
- String Table 范围不得与 Header、任一通用表或 TLV 链范围重叠。
- TLV 链范围不得与 Header、String Table 或任一通用表范围重叠。
- Blob Table、Object Table、Schema Table、Projection Table、Relation Table、Metadata Binding Table 和 External Reference Table 之间的表范围不得重叠。
- FILE_RANGE blob 的 `file_offset + stored_size` 不得发生整数溢出，且文件范围在任何情况下都不得与 Header、String Table、Blob Table、Object Table、Schema Table、Projection Table、Relation Table、Metadata Binding Table、External Reference Table 或 TLV 链范围发生任何字节重叠。
- 每个 blob_id、object_id、schema_id、projection_id、relation_id、binding_id 和 external_ref_id 在对应表内唯一，且不得为 0。
- 每个非 0 字符串偏移必须落在 String Table 范围内并指向 NUL 终止 UTF-8 字符串。
- 每个 Blob、Schema 和 External Reference 的 hash_alg 必须是 v1 已定义值；HASH_NONE 必须对应全 0 hash，非 HASH_NONE 必须对应非全 0 hash，并在可取得逻辑内容时强制校验。
- 每个 Blob source_kind 和 compression 必须是 v1 已定义值；media_type_off 非 0 时必须指向合法 identifier；compression != NONE 时，解码后的逻辑内容长度必须等于 size。
- ZERO blob 的 file_offset、stored_size、external_ref_id 和 compression 必须为 0；EXTERNAL_REF blob 的 file_offset、stored_size 和 compression 必须为 0，且必须引用 kind 为 BLOB 的 External Reference 记录。
- 每个 Schema kind、format 和 flags 必须是 v1 已定义值。
- 每个 Schema 的 format 与 descriptor 字段必须满足第 8 章约束；EMBEDDED_DESCRIPTOR 必须引用本文件 blob，EXTERNAL_DESCRIPTOR 必须通过 descriptor_ref_id 引用 kind 为 SCHEMA 的 External Reference 记录。
- SCHEMA_BUNDLE TLV 不得作为 Schema Table descriptor 来源。
- 每个 SEALED Schema 必须设置 hash_alg != HASH_NONE 且 schema_hash 非全 0；format 为 NONE 时不得设置 SEALED。
- 每个 object.schema_id 必须引用存在的 Schema 记录。
- 每个 object.schema_id 引用的 schema kind 必须为 OBJECT_PAYLOAD。
- 每个 object payload 范围必须位于对应 blob 范围内。
- 每个 schema descriptor 范围必须位于对应 blob 范围内。
- 每个 projection.source_object_id 必须引用存在的 Object 记录。
- 每个 projection.input_schema_id 必须引用存在的 Schema 记录。
- 每个 projection.input_schema_id 必须等于 source object 的 schema_id；v1 Core 不定义根输入 schema 兼容或自动适配规则。
- 每个 projection.config_schema_id 非 0 时必须引用存在的 Schema 记录。
- 每个 projection.config_schema_id 非 0 时引用的 schema kind 必须为 PROJECTION_CONFIG。
- closure_policy 为 DECLARED_RELATIONS 时，projection config 必须来自本文件内的非 EXTERNAL_REF blob。
- 每个 projection 的 target_protocol_id_off 和 projector_id_off 必须非 0。
- projection.projector_source 在 v1 必须为 ENVIRONMENT_ONLY。
- projection flags 中 LOSSLESS 和 LOSSY_ALLOWED 不得同时设置；PRIMARY 不得与 DIAGNOSTIC 或 HIDDEN 同时设置。
- projection 的 REQUIRES_TRUSTED_INPUT flag 必须与 trust_policy == TRUSTED_INPUT_REQUIRED 保持一致。
- TRUSTED_INPUT_REQUIRED projection 不得使用 PROJECTOR_DEFINED closure_policy。
- projection 闭包遍历必须使用确定性遍历顺序和已访问 object_id 集合；遇到已访问对象时不得继续递归展开该边。
- projection 闭包收集必须受实现资源上限约束，超过最大闭包对象数、最大 relation 遍历数或最大遍历深度时，当前 projection 必须失败。
- 同一 source_object_id、target_protocol_id 与 target_protocol_version 组合下最多只能存在一个 PRIMARY projection。
- 每个 relation.source_object_id 必须引用存在的 Object 记录。
- 每个 relation.target_object_id 非 0 时必须引用存在的 Object 记录。
- 每个 relation 的 REQUIRED 和 WEAK flags 不得同时设置。
- 每个 REQUIRED relation 的 target_object_id 必须非 0。
- 每个 relation payload 非空时 schema_id 必须引用存在的 Schema 记录。
- 每个 relation.schema_id 非 0 时引用的 schema kind 必须为 RELATION_PAYLOAD。
- 每个 metadata binding 的 owner_kind 必须在 v1 定义范围内，owner_id 必须引用对应表记录；FILE owner 的 owner_id 必须为 0。
- 每个 metadata binding 的 metadata_schema_id 非 0 时必须引用 kind 为 METADATA 或 VIEW 的 Schema 记录。
- 每个 MANDATORY metadata binding 的 metadata_schema_id 必须非 0，且不得设置 DISPLAY_ONLY 或 DIAGNOSTIC。
- 每个 metadata binding 的 tlv_count 必须非 0。
- 若 TLV 链不存在 structural error，每个 metadata binding 的 tlv_first..tlv_first+tlv_count 必须落在 TLV 链 ordinal 范围内。
- 同一 owner 下的 MANDATORY metadata binding 的 TLV ordinal 范围不得互相重叠；不同 owner 可以共享同一段 TLV。
- 若 TLV 链存在 structural error，所有 metadata binding 视为不可解析，并跳过 metadata binding 的 TLV ordinal 范围校验；不依赖 TLV metadata 的对象图检查可以继续，但相关 owner 不得参与需要 mandatory metadata 的 projection 或展示路径。
- 每个 external reference 的 kind 必须在 v1 定义范围内。
- 每个 external reference 的 locator_off 和 namespace_off 非 0 时必须指向合法 identifier。
- 每个 REQUIRED external reference 必须设置 hash_alg != HASH_NONE 且 content_hash 非全 0。
- EXTERNAL_REF blob 必须引用 kind 为 BLOB 的 External Reference 记录。
- 被用于满足 TRUSTED_INPUT_REQUIRED 的 `soyo.trust.signature.v1` 信任对象 payload 必须满足第 13.3 节布局、算法、coverage、key_id、TRUST_ANCHOR、zero range 和签名大小约束。
- TRUSTED_INPUT_REQUIRED projection 的候选信任对象必须按第 13 章标准发现规则从 Object Table 扫描得到。
- TRUSTED_INPUT_REQUIRED projection 必须至少存在一个验证成功的标准信任对象，且该信任对象 coverage 必须覆盖第 13.1 节最低覆盖集。
- 标准信任对象的 coverage entry 必须能映射为第 13.2 节定义的有效 chunk；部分表 coverage、重复 chunk、指向不存在记录或不存在 TLV 的 coverage、EXTERNAL_REF blob 字节 coverage 均使该信任对象无效。
- 同一 external reference record 不得同时由 External Reference Table 完整表级 chunk 和 kind 12 单记录 chunk 覆盖。
- 标准信任对象的 canonical chunk 必须按 `(kind, id, offset, size)` 升序拼接。
- 标准信任对象的 zero range 只能作用于该信任对象自身 payload 所在 blob 字节 chunk，且必须完整覆盖 signature bytes；signature bytes 所在范围不得与任何其它对象 payload、relation payload、schema descriptor 或 projection config 范围重叠。
- 标准信任对象的 key_id 必须匹配唯一 TRUST_ANCHOR external reference；信任锚无法解析、解析结果不可信或 content_hash 校验失败时，该信任对象无效。
- 每个保留字段和保留 bit 必须为 0。

执行、投影或导出路径还必须完成对应 schema 和 projector 规定的额外校验。SOYO Core 校验通过不表示对象可以执行，只表示容器结构可被安全遍历。

=== 16.1 资源限制

SOYO v1 不规定单一全局最大文件大小，但实现必须配置并公开自己的资源限制。至少应包含最大文件大小、最大 String Table 字节数、最大字符串长度、最大 Blob 数量、最大 Object 数量、最大 Schema 数量、最大 Projection 数量、最大 Relation 数量、最大 Metadata Binding 数量、最大 External Reference 数量、最大 TLV 数量、最大单个 blob 字节数、最大闭包对象数、最大闭包 relation 遍历数和最大闭包遍历深度。

超出实现资源限制时，解析器必须确定性失败，不得返回部分成功结果。资源限制失败不是 schema 语义失败，也不等同于文件格式 malformed；诊断信息必须区分格式错误、未知语义、信任失败和资源耗尽。内核态解析器可以采用比用户态工具更严格的限制。

=== 16.2 验收场景

符合 v1 的检查器必须能对以下场景给出确定结果：合法 embedded schema descriptor 必须通过；合法 external schema descriptor 必须通过；Header file_size 与实际文件大小不一致必须失败；FILE_RANGE blob 使用压缩存储且解压后长度等于 size 必须通过；FILE_RANGE blob 的 stored_size 范围覆盖核心元数据必须失败；SCHEMA_BUNDLE TLV 被用作 descriptor 来源必须失败；SEALED + HASH_NONE 必须失败；SEALED + format NONE 必须失败；object 使用非 OBJECT_PAYLOAD schema 必须失败；relation payload 使用非 RELATION_PAYLOAD schema 必须失败；projection config 使用非 PROJECTION_CONFIG schema 必须失败；DECLARED_RELATIONS 使用 EXTERNAL_REF config blob 必须失败；TRUSTED_INPUT_REQUIRED 使用 PROJECTOR_DEFINED closure 必须失败；LOSSLESS 与 LOSSY_ALLOWED 同时设置必须失败；PRIMARY 与 DIAGNOSTIC 或 HIDDEN 同时设置必须失败；同一 owner 下 MANDATORY metadata 缺少 metadata_schema_id 必须失败；同一 owner 下 MANDATORY metadata 范围重叠必须失败；不同 owner 共享同一段 TLV 必须允许；trust signature 缺少覆盖 signature bytes 的 zero range 必须失败；trust signature 的 signature bytes 与其它 payload 或 descriptor 范围别名化必须失败；TRUSTED_INPUT_REQUIRED coverage 缺少最低覆盖集必须失败；trust signature key_id 无唯一 TRUST_ANCHOR 必须失败；canonical coverage 出现重复 chunk 必须失败；canonical coverage 对通用表做部分覆盖必须失败；同一 external reference record 同时使用完整表级 chunk 和 kind 12 单记录 chunk 覆盖必须失败；REQUIRED external reference 无 content_hash 必须失败；unknown non-mandatory TLV 必须可跳过；TLV structural error 必须禁用 metadata binding 并跳过 ordinal 范围校验，但不得阻断不依赖 TLV metadata 的对象图检查。

== 17. 文件生产工具

SOYO 文件生产工具必须保证输出文件满足本文的 Header、String Table、Blob Table、Object Table、Schema Table、Projection Table、Relation Table、Metadata Binding Table、External Reference Table 和 TLV 规则。生产工具不得把某个具体目标协议的必需语义伪装成 Core 语义。

SOYO 文件生产工具必须满足以下要求：

- 输出任何对象时，必须写入 Object 记录和引用的 Schema 记录。
- 输出对象 payload 时，必须写入对应 Blob 记录，正确设置 size、stored_size、compression 和 content_hash，且 payload 范围不得越界。
- 输出可投影对象时，必须写入 Projection 记录，说明目标协议、projector 和输入 schema。
- 输出对象关系时，必须写入 Relation 记录；relation payload 非空时必须写入 relation schema。
- 输出 metadata 时，必须写入 Metadata Binding 记录，说明 TLV 归属对象和 mandatory 语义。
- 输出外部依赖时，必须写入 External Reference 记录；Core 不得自动取回这些引用。
- 能够提供 schema descriptor 的生产工具应当写入 EMBEDDED_DESCRIPTOR 或 EXTERNAL_DESCRIPTOR；SCHEMA_BUNDLE TLV 只能作为展示或诊断辅助信息，不得替代 Schema Table descriptor。
- 工具可以在对象 payload 中使用私有布局，但不得要求 SOYO Core 识别该私有布局。
- 删除信息的工具不得删除 Header、String Table、Blob Table、Object Table、Schema Table、Projection Table、Relation Table、Metadata Binding Table 或 External Reference Table 中的必需信息。
- 删除信息的工具不得默认删除 required 对象、required schema、PRIMARY projection、required relation、required external reference 或被 mandatory metadata binding 引用的 TLV。

SOYO 检查工具应当能够显示 Header、字符串、blob、对象、schema、projection、relation、metadata binding、external reference、TLV、信任对象和可用 projector。SOYO 删除工具可以删除未被强制引用的信息 TLV、提示 TLV、诊断 TLV 或其它非执行必需数据。

== 18. 版本兼容

SOYO 的兼容边界分为四类：

- Core version：Header、通用表项格式、通用强制规则或安全边界发生不兼容变化时，必须提升 version。
- Schema version：对象 payload、relation payload 或 projection 配置语义发生变化时，由对应 schema identifier 和 schema version 管理。
- Projector ABI：projector 调用边界、输入约束或输出协议绑定方式发生变化时，由 projector_abi 和 projector_version 管理。
- Target protocol version：投影输出目标协议发生变化时，由 target protocol identifier 和 target protocol version 管理。

新增 schema 不影响旧实现的 Core 解析能力；旧实现遇到未知 schema 时，可以展示但不得执行或投影 required 对象。新增 projector 不影响旧实现的 Core 解析能力；旧实现遇到未知 projector 时，可以展示 projection，但不得假装完成投影。新增 TLV tag 不影响旧实现执行能力；旧实现必须能够跳过未知且非 mandatory 的 TLV。新增 mandatory TLV 会影响旧实现执行能力；旧实现不认识 mandatory TLV 时必须拒绝相关对象、schema、projection 或 relation。

执行必需变化必须显式失败；未被强制引用的 TLV 扩展必须稳定回退。

v1 内的兼容扩展应通过 TLV、schema、projector 或目标协议版本完成，并保持 version == 1。任何改变 Header、通用表项布局、通用强制规则或安全边界的扩展必须提升 version。旧解析器遇到 version > 1 时，不得按 v1 结构继续解析对象图、投影或 blob；只可以读取 magic、version、header_size 等最小诊断字段并安全退出。未知版本文件不得降级执行、投影或导出。

== 19. 迁移说明

早期草案中的 `Header.profile`、`SOYO_PROFILE_PROCESS`、`SOYO_PROFILE_ELM_EBI`、profile-local directory、通用 Segment Table 和 Required Directory 已从 SOYO Core 中移除。它们不再属于主标准。

用户态进程映像应迁移为独立的 Process Image Schema 和 Process projector。该 schema 可以定义入口、地址空间、栈、TLS、系统调用策略、绑定槽和生命周期钩子，但这些字段不得进入 SOYO Core。

ELM 单元和 EBI 应迁移为独立的 ELM Unit Schema 和 ELM projector。ELM Unit Schema 可以定义 manifest、target、payload blocks、segments、symbols、relocations、lifecycle、imports、exports、provider ports、extension points 和 extensions；ELM projector 可以把该对象投影成 EBI 协议对象。SOYO Core 不理解 EBI，ELM Core 也不应依赖 SOYO 文件偏移、表偏移或 TLV 偏移。

归档文件、调试对象、设备树 overlay、配置包和其它未来对象也应按同一方式定义 schema 和 projector。SOYO Core 只承载它们，不内建它们。

#warn[
  规范性结论：SOYO 是可自描述、可组合、可投影的对象容器格式。SOYO Core 只定义对象、字节块、schema、投影、关系和扩展元数据的通用承载规则。具体语义由对象 schema 和 projector 定义。
]

== 20. 附录：标准扩展对象与投影实现

本章定义随 SOYO v1 文档发布的标准扩展对象和标准投影实现。它们不是 SOYO Core 的组成部分，不改变 Header、通用表、TLV、信任边界或校验矩阵。实现可以只支持其中一部分；不支持时必须按未知 schema 或未知 projector 处理，不得把对应对象误判为可执行、可授权或可装载。

=== 20.1 标准字段对象

标准字段对象吸收 Field Table 的结构化展示能力，但不把字段系统放入 SOYO Core。字段对象只是一种普通对象 payload，适合给通用工具、schema descriptor、调试器和 projector 提供稳定的键值视图。

标准 identifier 如下：

#table(
  columns: (1.4fr, 3.8fr),
  inset: 6pt,
  stroke: line-stroke,
  align: (left, left),
  table.header(
    head[类别],
    head[identifier],
  ),
  [字段对象 schema], [`soyo.fields.v1`],
  [字段归属关系], [`soyo.has-fields`],
)

字段对象 payload 由固定头部和 field entry 数组组成，所有整数均为 little-endian：

```text
version: u16                 // v1 必须为 1
flags: u16                   // v1 必须为 0
field_count: u32
field_table_offset: u32
reserved: u32                // 必须为 0
```

每个 field entry 使用以下布局：

```text
key_off: u32                 // String Table 字符串偏移，不得为 0
schema_id: u32               // 字段 schema；无独立 schema 时为 0
value_kind: u16
flags: u16
aux: u32
value0: u64
value1: u64
```

value_kind 使用以下值：0 表示 NULL，1 表示 U64，2 表示 I64，3 表示 BOOL，4 表示 STRING_OFFSET，5 表示 BLOB_ID，6 表示 OBJECT_ID，7 表示 SCHEMA_ID，8 表示 RELATION_ID，9 表示 PROJECTION_ID，10 表示 BYTES_IN_BLOB。flags bit 0 表示 REQUIRED_BY_SCHEMA，bit 1 表示 NO_STRIP，bit 2-15 保留且必须为 0。

字段对象规则如下：

- 字段对象必须通过 `soyo.has-fields` relation 绑定到 owner object；该 relation 的 source_object_id 为 owner，target_object_id 为字段对象。
- 字段对象不得改变 owner object 的主语义；owner 的主语义仍由 owner.schema_id 和 payload 定义。
- REQUIRED_BY_SCHEMA 字段缺失、重复键、字段顺序和跨字段约束由 owner schema 或字段 schema 解释。
- value_kind 引用 blob、object、schema、relation 或 projection 时，引用目标必须存在。
- BYTES_IN_BLOB 使用 value0 作为 blob_id，aux 作为 blob 内偏移，value1 作为字节长度；范围不得越过 blob.size。
- 字段对象可以进入 TRUSTED_INPUT_REQUIRED 最低覆盖集；若 projector 声明字段影响投影结果，则必须覆盖字段对象 payload 和 `soyo.has-fields` relation。

=== 20.2 标准策略对象

标准策略对象吸收 Policy Table 的强制策略表达能力，但不把策略系统放入 SOYO Core。策略是否可安装、如何授权、如何审计，仍由目标运行时、schema 和 projector 共同定义。

标准 identifier 如下：

#table(
  columns: (1.4fr, 3.8fr),
  inset: 6pt,
  stroke: line-stroke,
  align: (left, left),
  table.header(
    head[类别],
    head[identifier],
  ),
  [策略集合 schema], [`soyo.policy.set.v1`],
  [策略项 schema], [`soyo.policy.item.v1`],
  [策略归属关系], [`soyo.applies-policy`],
)

策略集合对象 payload 使用以下固定头部：

```text
version: u16                 // v1 必须为 1
flags: u16
policy_count: u32
policy_table_offset: u32
reserved: u32                // 必须为 0
```

策略集合 flags bit 0 表示 HAS_MANDATORY_POLICY，bit 1-15 保留且必须为 0。每个 policy entry 使用以下布局：

```text
policy_schema_id: u32        // 必须引用 kind 为 METADATA 或 OBJECT_PAYLOAD 的 schema
subject_kind: u16            // 0=file, 1=object, 2=blob, 3=projection, 4=relation, 5=external_ref
flags: u16
subject_id: u64              // file subject 时必须为 0
payload_blob_id: u64         // 无 payload 时为 0
payload_offset: u64
payload_size: u64
```

policy entry flags bit 0 表示 MANDATORY，bit 1 表示 AUDIT_ONLY，bit 2-15 保留且必须为 0。MANDATORY 与 AUDIT_ONLY 不得同时设置。payload_blob_id 为 0 时，payload_offset 和 payload_size 必须为 0；payload_blob_id 非 0 时，payload 范围不得越过 blob.size。

策略对象规则如下：

- 策略集合对象必须通过 `soyo.applies-policy` relation 绑定到被约束对象、projection 或 file owner。
- MANDATORY policy 不被目标运行时支持、payload 无法解析或安装失败时，相关 projection 必须失败。
- AUDIT_ONLY policy 只生成诊断或审计输入，不得改变授权结果。
- 策略对象不得替代 SOYO Core 的边界检查、信任校验和 mandatory 规则。
- 删除工具不得默认删除被 required projection、required relation 或 mandatory metadata 引用的策略对象。

=== 20.3 标准投影通用规则

两个投影实现都遵循以下共同规则：

- Projection Table 中的 projector_source 必须为 ENVIRONMENT_ONLY。
- projector 只能读取已经通过 SOYO Core 校验的对象图、blob、schema、relation、metadata binding、external reference 和 TLV。
- 若 projection 设置 TRUSTED_INPUT_REQUIRED，projector 必须先完成第 13 章定义的可信输入校验，再输出目标协议对象。
- 投影输出不得保留对 SOYO 文件偏移、表偏移或 TLV ordinal 的运行时依赖；投影完成后，目标运行时只使用投影结果。
- projector 无法理解 required object、required relation、mandatory metadata、required external reference 或 required schema 时，投影必须失败。

=== 20.4 可执行映像投影

可执行映像投影把 SOYO 对象图投影为一个可由装载器消费的进程映像描述。该投影不等同于 ELF、PE、Mach-O 或任何已有可执行文件格式；它只定义 SOYO 到“可执行映像协议对象”的转换边界。

标准 identifier 如下：

#table(
  columns: (1.4fr, 3.8fr),
  inset: 6pt,
  stroke: line-stroke,
  align: (left, left),
  table.header(
    head[类别],
    head[identifier],
  ),
  [根对象 schema], [`soyo.exec.manifest.v1`],
  [段对象 schema], [`soyo.exec.segment.v1`],
  [投影配置 schema], [`soyo.exec.projector.config.v1`],
  [目标协议], [`soyo.exec.image.v1`],
  [projector], [`soyo.projector.exec-image.v1`],
  [入口关系], [`soyo.exec.entry`],
  [段关系], [`soyo.exec.segment`],
  [依赖关系], [`soyo.exec.depends-on`],
)

根对象必须使用 schema `soyo.exec.manifest.v1`。该对象 payload 使用以下固定布局，所有整数均为 little-endian：

```text
version: u16                 // v1 必须为 1
arch: u16                    // 0=agnostic, 1=riscv64, 2=loongarch64
abi: u16                     // 0=freestanding, 1=posix-like-user
endian: u8                   // 1=little-endian
pointer_width: u8            // 32 或 64
flags: u32
entry_vaddr: u64
default_stack_size: u64
segment_count: u32
segment_table_offset: u32
relocation_count: u32
relocation_table_offset: u32
capability_count: u32
capability_table_offset: u32
reserved: [u8; 32]           // 必须全 0
```

manifest flags 使用以下 bit：

- bit 0：PIE，映像允许被装载器重定位。
- bit 1：STATIC_LINKED，映像不依赖运行时动态链接器。
- bit 2：REQUIRES_SYSCALL_ABI，映像依赖目标运行时系统调用 ABI。
- bit 3：NO_DEFAULT_STACK，装载器不得自动创建默认栈。
- bit 4-31：保留，v1 必须为 0。

段表由 segment_count 个 segment entry 组成，起点为 manifest payload 内的 segment_table_offset。每个 segment entry 使用以下布局：

```text
vaddr: u64
mem_size: u64
file_size: u64
blob_id: u64
blob_offset: u64
flags: u32
align: u32
reserved: u64                // 必须为 0
```

segment flags 使用以下 bit：bit 0 表示 READ，bit 1 表示 WRITE，bit 2 表示 EXECUTE，bit 3 表示 ZERO_FILL，bit 4 表示 SHARED，bit 5-31 保留。READ、WRITE、EXECUTE 至少必须设置一个。ZERO_FILL 设置时，file_size 可以小于 mem_size；未设置 ZERO_FILL 时，file_size 必须等于 mem_size。blob_id 为 0 时，file_size 必须为 0，且该段只能由 ZERO_FILL 表达。blob_id 非 0 时必须引用存在的 blob，且 `blob_offset + file_size` 不得超过 blob.size。align 为 0 表示无附加约束；非 0 时必须为 2 的幂。

relocation table 和 capability table 的具体条目格式由 abi 字段指定。若 relocation_count 或 capability_count 非 0，而 projector 不支持对应 abi 的条目格式，投影必须失败。v1 标准 projector 至少必须支持 relocation_count == 0 且 capability_count == 0 的静态映像。

可执行映像 projector 必须执行以下校验：

- manifest version、arch、abi、endian、pointer_width 和 flags 必须在 v1 定义范围内。
- entry_vaddr 必须落在某个 EXECUTE 段的虚拟地址范围内。
- 任意两个段的虚拟地址范围不得重叠；计算 `vaddr + mem_size` 前必须检查整数溢出。
- WRITE 与 EXECUTE 同时设置的段必须被 projector 标记为安全策略风险；若投影配置禁止 W+X，投影必须失败。
- 若 projection 要求 TRUSTED_INPUT_REQUIRED，最低覆盖集必须包含 manifest payload、所有 segment entry、所有被引用 blob 字节范围和参与投影的 mandatory metadata。
- 输出映像必须按 vaddr 升序排列段；相同 vaddr 或重叠段必须失败。

可执行映像投影的输出是以下抽象对象集合：

- ExecImageHeader：arch、abi、entry_vaddr、pointer_width、flags 和默认栈需求。
- ExecSegmentList：按 vaddr 升序排列的段，每段包含 vaddr、mem_size、权限、align 和已解析字节内容。
- ExecRelocationList：由 abi 解释的重定位请求；v1 最小实现允许为空。
- ExecCapabilityList：由目标运行时解释的能力请求；v1 最小实现允许为空。

装载器消费 ExecImage 时不得再读取 SOYO 文件本体。若运行时需要延迟分页或按需装载，projector 必须把所需字节范围复制到运行时可验证的字节提供者中，并把完整性哈希随输出对象一起传递。

=== 20.5 EBI 投影

EBI 投影把 SOYO 中的 ELM 单元对象图投影为 ELM 运行时可消费的 EBI 协议对象。EBI 是 ELM 的原生二进制接口协议，不是 SOYO Core，也不是 SOYO 文件格式。SOYO 文件只负责承载 ELM 单元对象和投影声明。

标准 identifier 如下：

#table(
  columns: (1.4fr, 3.8fr),
  inset: 6pt,
  stroke: line-stroke,
  align: (left, left),
  table.header(
    head[类别],
    head[identifier],
  ),
  [根对象 schema], [`elm.unit.schema.v1`],
  [段对象 schema], [`elm.unit.segment.v1`],
  [符号对象 schema], [`elm.unit.symbols.v1`],
  [重定位对象 schema], [`elm.unit.relocations.v1`],
  [投影配置 schema], [`elm.ebi.projector.config.v1`],
  [目标协议], [`elm.ebi.v1`],
  [projector], [`elm.projector.soyo-to-ebi.v1`],
  [依赖关系], [`elm.depends-on`],
  [拓展关系], [`elm.extends`],
  [接口导入关系], [`elm.imports`],
  [接口导出关系], [`elm.exports`],
)

根对象 payload 使用 schema `elm.unit.schema.v1`。该 payload 是 ELM 单元 manifest，固定头部如下：

```text
version: u16                 // v1 必须为 1
unit_kind: u16               // 1=manager, 2=module, 3=tool, 4=interface
target_arch: u16             // 0=agnostic, 1=riscv64, 2=loongarch64
rust_abi: u16                // v1 必须为 1，表示 rust-only ELM ABI v1
flags: u32
name_offset: u32
name_size: u32
version_offset: u32
version_size: u32
on_initialize_symbol: u32
on_finalize_symbol: u32
segment_count: u32
symbol_count: u32
relocation_count: u32
import_count: u32
export_count: u32
extension_point_count: u32
reserved: [u8; 32]           // 必须全 0
```

manifest 中的 name_offset、version_offset 指向该 manifest payload 内的 UTF-8 字节范围，不要求 NUL 结尾。on_initialize_symbol 和 on_finalize_symbol 必须引用 EBI symbol table 中存在的符号 id。unit_kind 为 interface 或 tool 时，可以没有可执行段，但仍必须提供 on_initialize 和 on_finalize；这两个 hook 可以是空实现。

manifest flags 使用以下 bit：

- bit 0：HOTPLUG_SAFE，允许在运行时加载和卸载。
- bit 1：STATEFUL，模块持有运行时状态，卸载前必须完成状态迁移或释放。
- bit 2：MANAGER_REQUIRED，模块必须通过 elm-mgr API 初始化。
- bit 3：PROVIDES_API，模块向其它 ELM 导出接口。
- bit 4：CONSUMES_API，模块依赖其它 ELM 导出的接口。
- bit 5：TOOL_ONLY，模块只提供工具函数或类型定义，不直接挂接内核子系统。
- bit 6-31：保留，v1 必须为 0。

EBI projector 的输入闭包规则如下：

- 根对象必须进入闭包。
- REQUIRED 的 `elm.depends-on`、`elm.imports`、`elm.exports` 和 `elm.extends` relation 必须进入闭包。
- WEAK 的 `elm.extends` relation 可以缺失目标对象，但 projector 必须把该缺失作为诊断项写入输出。
- 所有参与输出的 segment、symbol、relocation、import、export 和 extension point 对象都必须有明确 schema。
- 若 projection 要求 TRUSTED_INPUT_REQUIRED，最低覆盖集必须包含 manifest、所有参与输出的 ELM 关系、所有代码和数据 blob、所有符号与重定位描述、所有 required external reference 和 mandatory metadata。

EBI 输出协议对象由以下部分组成：

- EbiHeader：unit name、unit version、unit_kind、target_arch、rust_abi、flags、on_initialize 和 on_finalize。
- EbiSegmentList：代码、只读数据、可写数据、TLS 模板和零填充区。
- EbiSymbolTable：导出符号、内部符号、hook 符号和接口符号。
- EbiRelocationTable：运行时需要应用的重定位项。
- EbiImportSet：该 ELM 依赖的 elm-mgr API、其它 ELM API 和内核子系统 API。
- EbiExportSet：该 ELM 对外开放的 API、extension point 和 service 描述。
- EbiMetadataSet：投影后仍需交给 elm-mgr 的展示信息、诊断信息、权限请求和热插拔策略。

EBI projector 必须执行以下校验：

- rust_abi 必须为 v1 支持值；v1 不接受 C/C++ ABI。
- on_initialize 和 on_finalize 必须存在，签名必须匹配 rust-only ELM ABI v1 的 hook 约定。
- HOTPLUG_SAFE 未设置时，elm-mgr 不得在运行时卸载该 ELM；只能在启动阶段加载或在关机阶段释放。
- PROVIDES_API 设置时，必须至少存在一个 export 或 extension point。
- CONSUMES_API 设置时，必须至少存在一个 import 或 depends-on relation。
- TOOL_ONLY 设置时，不得声明直接接入设备、中断、调度、VFS 或网络子系统；只能通过 elm-mgr 暴露工具 API。
- 所有导入 API 必须能由 elm-mgr、内核子系统适配层或闭包内其它 ELM 满足；否则投影失败。
- 所有导出 API 的名称、版本和符号 id 必须唯一；冲突时投影失败。

EBI projector 不得直接把 SOYO blob 暴露给 ELM 运行时作为长期依赖。投影完成后，elm-mgr 接收的是 EBI 输出协议对象；后续热插拔、依赖解析、API 注册、事件绑定和服务发布均由 elm-mgr 按 EBI 结果执行。

EBI 投影与 EKI、SOYO 的关系如下：

- EKI 是 ELM 原生镜像类型时，应天然实现 EBI。
- SOYO 本身不实现 EBI，但可以通过本节定义的 projector 投影出 EBI。
- 任意未来文件类型只要能被某个 resolver 或 projector 转换为等价 EBI 输出协议对象，就可以被 elm-mgr 作为 ELM 输入候选；elm-mgr 不应只识别 SOYO 或 EKI 文件扩展名。
