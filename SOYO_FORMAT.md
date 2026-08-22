# SOYO 文件格式标准

版本：**SOYO Core v1**，文档修订版：2026-08

本文是 Hitoshizuku OS 的 SOYO 文件格式规范。它恢复了原
`soyo-object-container-standard.typ` 的规范内容，并以 Markdown 作为长期维护格式。
本文中的表项大小、字段偏移、枚举值和校验规则属于线格式契约；修改它们必须同时
更新实现、测试和版本号。

SOYO 有两个需要明确区分的层次：

1. **SOYO Core 对象容器**：本文第 1 至第 20 节定义的通用对象图、字节块、schema、
   projection、relation、metadata 和外部引用容器。Core 不理解 EBI、ELF、tar 或
   进程映像的业务字段。
2. **SOYO Wire 可执行映像 profile**：`libs/soyo/wire-abi.registry` 和
   `libs/soyo/src` 当前实现的固定线格式，用于 `soyo-ld` 产生可装载映像。该 profile
   是 Core 的一个具体扩展，不得把 profile 字段反向加入 Core Header。

本文使用以下规范词：**必须**和**不得**是硬性要求；**应当**是推荐要求；**可以**
表示可选行为。实现即使只支持一部分 schema 或 projector，也必须遵守 Core 的边界、
溢出、范围和未知数据处理规则。

## 1. 适用范围

SOYO Core v1 规定：

- Header、String Table、Blob Table、Object Table、Schema Table、Projection Table、
  Relation Table、Metadata Binding Table、External Reference Table 和 TLV 扩展层；
- 对象类型、schema、projector、目标协议和 relation identifier 的命名规则；
- schema 描述符、projection 输入闭包、关系遍历、metadata 强制性和外部引用边界；
- 解析器、检查器、projector、导出器及生产工具的安全校验要求；
- v1 的信任对象、规范化签名覆盖范围和未知扩展的兼容行为。

SOYO Core 不规定以下内容：

- 用户态进程的入口、栈、TLS、系统调用策略或地址空间布局；
- ELM、EBI、EKI、LKM 等运行时的业务字段；
- ELF、tar、cpio、调试文件或其它目标格式的布局；
- 任意对象 payload 的业务语义。

这些语义必须由独立 schema 和 projector 定义，并通过本文的对象、schema、projection
和 relation 机制承载。

## 2. 文件分层与通用编码

SOYO 文件包含十类区域：

| 顺序 | 区域 | 职责 |
| ---: | --- | --- |
| 1 | Header | 固定 192 字节，声明版本、表位置、表项数量和 TLV 范围。 |
| 2 | String Table | NUL 终止 UTF-8 字符串池。 |
| 3 | Blob Table | 描述文件内、全零或外部来源的字节块。 |
| 4 | Object Table | 对象节点及其 payload 引用。 |
| 5 | Schema Table | payload、关系和配置的 schema 身份与描述符。 |
| 6 | Projection Table | 对象图可投影到的目标协议和 projector。 |
| 7 | Relation Table | 对象间有向关系。 |
| 8 | Metadata Binding Table | 将 TLV 绑定到文件或记录。 |
| 9 | External Reference Table | 外部 schema、blob、projector 或信任锚声明。 |
| 10 | TLV | 可扩展展示、诊断和策略元数据。 |

所有多字节整数均为 little-endian。解析器不得把不可信字节直接转换为宿主机结构体；
必须按字段偏移读取，并在每次加法、乘法和切片前检查溢出与边界。

### 2.1 字符串、identifier 和哈希

除字段另有说明外，所有 `*_off` 都指向 String Table 内的 NUL 终止 UTF-8 字符串。
offset `0` 表示空字符串或缺省值。字符串按字节比较，不执行 Unicode 归一化。

identifier 使用点分层命名，允许的字符为小写字母、数字、`.`、`-` 和 `_`；不得含
有 `:`、`/`、`?`、`#`、`@` 或空白。未特别限制时最大长度为 128 字节。契约使用
`name@version` 形式，name 遵守同样的字符限制。

SOYO 区分两类哈希：

- **匹配哈希**：8 字节 `*_hash`，使用 BLAKE3-256 前 8 字节按 little-endian 解释，
  只用于索引、缓存和诊断，不得用于认证或安全策略；
- **完整性哈希**：32 字节 `content_hash` 或 `schema_hash`，由字段中的
  `hash_alg` 指定，使用完整的 BLAKE3-256 或 SHA-256 digest。

匹配哈希不一致时仍必须使用字符串或完整性哈希确认身份；不得仅因匹配哈希不一致而
拒绝投影。`HASH_NONE` 时对应哈希字段必须全零，非 `HASH_NONE` 时应写入非零完整
digest。

## 3. Header

Header 固定为 192 字节，`version == 1` 时 `header_size == 192`。字段如下：

| 偏移 | 大小 | 字段 | 说明 |
| ---: | ---: | --- | --- |
| `0x00` | 4 | `magic` | `soyo`，即 `73 6f 79 6f`。 |
| `0x04` | 2 | `version` | 格式版本，v1 为 `1`。 |
| `0x06` | 2 | `header_size` | v1 为 `192`。 |
| `0x08` | 4 | `flags` | v1 必须为 `0`。 |
| `0x0c` | 4 | `reserved0` | 必须为 `0`。 |
| `0x10` | 8 | `string_offset` | String Table 文件偏移。 |
| `0x18` | 8 | `string_size` | String Table 字节数。 |
| `0x20` | 8 | `blob_offset` | Blob Table 文件偏移；无记录时为 `0`。 |
| `0x28` | 4 | `blob_count` | Blob 数量。 |
| `0x2c` | 2 | `blob_entry_size` | v1 为 `96`。 |
| `0x2e` | 2 | `reserved1` | 必须为 `0`。 |
| `0x30` | 8 | `object_offset` | Object Table 文件偏移。 |
| `0x38` | 4 | `object_count` | 对象数量。 |
| `0x3c` | 2 | `object_entry_size` | v1 为 `80`。 |
| `0x3e` | 2 | `reserved2` | 必须为 `0`。 |
| `0x40` | 8 | `schema_offset` | Schema Table 文件偏移。 |
| `0x48` | 4 | `schema_count` | schema 数量。 |
| `0x4c` | 2 | `schema_entry_size` | v1 为 `96`。 |
| `0x4e` | 2 | `reserved3` | 必须为 `0`。 |
| `0x50` | 8 | `projection_offset` | Projection Table 文件偏移。 |
| `0x58` | 4 | `projection_count` | projection 数量。 |
| `0x5c` | 2 | `projection_entry_size` | v1 为 `80`。 |
| `0x5e` | 2 | `reserved4` | 必须为 `0`。 |
| `0x60` | 8 | `relation_offset` | Relation Table 文件偏移。 |
| `0x68` | 4 | `relation_count` | relation 数量。 |
| `0x6c` | 2 | `relation_entry_size` | v1 为 `80`。 |
| `0x6e` | 2 | `reserved5` | 必须为 `0`。 |
| `0x70` | 8 | `metadata_offset` | Metadata Binding Table 偏移。 |
| `0x78` | 4 | `metadata_count` | binding 数量。 |
| `0x7c` | 2 | `metadata_entry_size` | v1 为 `32`。 |
| `0x7e` | 2 | `reserved6` | 必须为 `0`。 |
| `0x80` | 8 | `external_ref_offset` | External Reference Table 偏移。 |
| `0x88` | 4 | `external_ref_count` | 外部引用数量。 |
| `0x8c` | 2 | `external_ref_entry_size` | v1 为 `80`。 |
| `0x8e` | 2 | `reserved7` | 必须为 `0`。 |
| `0x90` | 8 | `tlv_offset` | TLV 链偏移；无 TLV 时为 `0`。 |
| `0x98` | 8 | `tlv_size` | TLV 链字节数。 |
| `0xa0` | 8 | `file_size` | 必须等于实际文件大小。 |
| `0xa8` | 24 | `reserved8` | 必须全为 `0`。 |

Header 不包含 profile、入口、目标架构、生命周期、权限或 EBI 字段；这些字段属于
对象 schema 或 projector。任意表的 count 为 `0` 时 offset 和 entry size 必须为 `0`；
count 非零时，`offset + count * entry_size` 必须位于文件范围内。所有核心区域必须
两两不重叠，`file_size` 不得小于 Header。

## 4. String Table

String Table 是一段字节串，偏移 `0` 必须为 NUL。所有非零偏移都必须位于范围内、指向
合法 UTF-8 字符串，并在表末尾前遇到 NUL。需要 identifier、名称或 schema 的字段不得
使用 offset `0`。文件范围外的尾部字节不得被当作字符串池内容。

## 5. 对象类型和 schema 注册

Core 不注册任何执行 profile。未知 identifier 可以被检查器展示，但装载器和 projector
不得根据字符串前缀猜测其语义。常见 identifier 示例：

| 类别 | 示例 |
| --- | --- |
| 对象类型 | `mygo.elm.unit` |
| schema | `mygo.elm.unit.schema.v1` |
| 目标协议 | `mygo.elm.ebi` |
| projector | `mygo.elm.projector.soyo-to-ebi` |
| relation type | `mygo.elm.depends-on` |
| 归档目标 | `posix.tar.archive` |

如果工具需要解释未知 schema，应在 Schema Table 中提供 descriptor。`SCHEMA_BUNDLE`
TLV 只用于展示和诊断，不得替代 Schema Table descriptor。

## 6. Blob Table

每条 Blob 记录为 96 字节：

| 偏移 | 大小 | 字段 | 说明 |
| ---: | ---: | --- | --- |
| `0x00` | 8 | `blob_id` | 唯一 id，`0` 保留。 |
| `0x08` | 4 | `flags` | v1 必须为 `0`。 |
| `0x0c` | 2 | `source_kind` | `1=FILE_RANGE`，`2=ZERO`，`3=EXTERNAL_REF`。 |
| `0x0e` | 2 | `hash_alg` | `0=HASH_NONE`，`1=BLAKE3_256`，`2=SHA256`。 |
| `0x10` | 8 | `file_offset` | 文件来源字节偏移。 |
| `0x18` | 8 | `size` | 逻辑字节数。 |
| `0x20` | 8 | `stored_size` | 存储字节数；未压缩时等于 `size`。 |
| `0x28` | 4 | `align` | `0` 或 2 的幂。 |
| `0x2c` | 4 | `compression` | `0=NONE`，`1=DEFLATE`，`2=ZSTD`。 |
| `0x30` | 4 | `media_type_off` | 内容类型 identifier；无则为 `0`。 |
| `0x34` | 4 | `reserved0` | 必须为 `0`。 |
| `0x38` | 32 | `content_hash` | 逻辑内容完整性哈希。 |
| `0x58` | 8 | `external_ref_id` | 仅 `EXTERNAL_REF` 使用。 |

`FILE_RANGE` 的 `file_offset..file_offset+stored_size` 必须在文件内，且不得与 Header、
任意核心表或 TLV 范围重叠。`ZERO` 的 offset、stored_size、external_ref_id 和 compression
必须全为 `0`，逻辑内容是 size 个零字节。`EXTERNAL_REF` 不携带本地字节，必须引用
kind 为 `BLOB` 的外部记录，Core 不自动下载或取回。

压缩 blob 解码后长度必须等于 `size`，未知 compression 必须拒绝。哈希按解压后的逻辑
字节计算。`media_type_off` 只提供提示，不授予可执行或可投影语义。

## 7. Object Table

每条 Object 记录为 80 字节：

| 偏移 | 大小 | 字段 | 说明 |
| ---: | ---: | --- | --- |
| `0x00` | 8 | `object_id` | 唯一 id，`0` 保留。 |
| `0x08` | 4 | `type_id_off` | 对象类型 identifier，非零字符串偏移。 |
| `0x0c` | 4 | `schema_id` | 必须引用 Schema Table。 |
| `0x10` | 4 | `flags` | `REQUIRED`、`PRIVATE`、`DISPLAY_ONLY`。 |
| `0x14` | 4 | `reserved0` | 必须为 `0`。 |
| `0x18` | 8 | `payload_blob_id` | payload 所在 blob；无 payload 为 `0`。 |
| `0x20` | 8 | `payload_offset` | blob 内偏移。 |
| `0x28` | 8 | `payload_size` | payload 大小。 |
| `0x30` | 4 | `name_off` | 展示名；无则为 `0`。 |
| `0x34` | 4 | `namespace_off` | 命名空间；无则为 `0`。 |
| `0x38` | 8 | `reserved1` | 必须为 `0`。 |
| `0x40` | 16 | `reserved2` | 必须全为 `0`。 |

对象 flags 的 bit 0、1、2 分别为 `REQUIRED`、`PRIVATE`、`DISPLAY_ONLY`，其余 bit
在 v1 必须为零。`payload_blob_id == 0` 时 offset 和 size 必须为零；否则范围不得越过
blob.size。对象是否可执行只由 projection 和显式 projector 决定。

## 8. Schema Table

每条 Schema 记录为 96 字节：

| 偏移 | 大小 | 字段 | 说明 |
| ---: | ---: | --- | --- |
| `0x00` | 4 | `schema_id` | 唯一 id，`0` 保留。 |
| `0x04` | 2 | `kind` | `OBJECT_PAYLOAD=1`、`RELATION_PAYLOAD=2`、`PROJECTION_CONFIG=3`、`METADATA=4`、`VIEW=5`。 |
| `0x06` | 2 | `format` | `NONE=0`、`EMBEDDED_DESCRIPTOR=1`、`EXTERNAL_DESCRIPTOR=2`。 |
| `0x08` | 4 | `flags` | 见下文。 |
| `0x0c` | 4 | `schema_name_off` | schema identifier，非零。 |
| `0x10` | 4 | `schema_version_off` | 版本字符串，非零。 |
| `0x14` | 4 | `display_name_off` | 展示名；无则为 `0`。 |
| `0x18` | 8 | `descriptor_blob_id` | 内嵌 descriptor 所在 blob。 |
| `0x20` | 8 | `descriptor_offset` | descriptor 在 blob 内偏移。 |
| `0x28` | 8 | `descriptor_size` | descriptor 大小。 |
| `0x30` | 2 | `hash_alg` | schema_hash 算法。 |
| `0x32` | 2 | `reserved0` | 必须为 `0`。 |
| `0x34` | 4 | `reserved1` | 必须为 `0`。 |
| `0x38` | 32 | `schema_hash` | descriptor 或外部 schema 完整性哈希。 |
| `0x58` | 8 | `descriptor_ref_id` | `EXTERNAL_DESCRIPTOR` 的 SCHEMA 引用。 |

Schema flags：bit 0 `REQUIRED`、bit 1 `EMBEDDED_DESCRIPTOR`、bit 2 `EXTERNAL_ALLOWED`、
bit 3 `DISPLAY_ONLY`、bit 4 `SEALED`、bit 5 `DEPRECATED`，其余 bit 必须为零。

- `NONE`：所有 descriptor 字段和 descriptor 相关 flags 必须为零；
- `EMBEDDED_DESCRIPTOR`：必须设置 bit 1，引用本文件中的非外部 blob；
- `EXTERNAL_DESCRIPTOR`：必须设置 bit 2，通过 kind 为 `SCHEMA` 的外部引用提供 descriptor；
- `SEALED`：必须使用完整性哈希，不能与 `NONE` 同时使用。

Schema kind 必须与引用位置匹配：对象只能引用 `OBJECT_PAYLOAD`，relation 只能引用
`RELATION_PAYLOAD`，projection 配置只能引用 `PROJECTION_CONFIG`，mandatory metadata
只能引用 `METADATA` 或 `VIEW`。

## 9. Projection Table

每条 Projection 记录为 80 字节：

| 偏移 | 大小 | 字段 | 说明 |
| ---: | ---: | --- | --- |
| `0x00` | 8 | `projection_id` | 唯一 id，`0` 保留。 |
| `0x08` | 8 | `source_object_id` | 源对象。 |
| `0x10` | 4 | `input_schema_id` | 必须等于源对象 schema。 |
| `0x14` | 4 | `config_schema_id` | 配置 schema；无配置为 `0`。 |
| `0x18` | 4 | `target_protocol_id_off` | 目标协议 identifier，非零。 |
| `0x1c` | 4 | `target_protocol_version_off` | 目标协议版本；可为 `0`。 |
| `0x20` | 4 | `projector_id_off` | projector identifier，非零。 |
| `0x24` | 4 | `projector_version_off` | projector 版本；可为 `0`。 |
| `0x28` | 2 | `projector_abi` | `0` 表示由 identifier 定义。 |
| `0x2a` | 2 | `closure_policy` | 输入闭包策略。 |
| `0x2c` | 2 | `projector_source` | v1 只允许 `ENVIRONMENT_ONLY=0`。 |
| `0x2e` | 2 | `trust_policy` | `UNSPECIFIED=0` 或 `TRUSTED_INPUT_REQUIRED=1`。 |
| `0x30` | 4 | `flags` | projection flags。 |
| `0x34` | 4 | `priority` | 数值小者优先。 |
| `0x38` | 8 | `config_blob_id` | 配置 blob；无配置为 `0`。 |
| `0x40` | 8 | `config_offset` | 配置偏移。 |
| `0x48` | 8 | `config_size` | 配置大小。 |

projection flags：bit 0 `PRIMARY`、1 `HIDDEN`、2 `DIAGNOSTIC`、3 `LOSSLESS`、4
`LOSSY_ALLOWED`、5 `REQUIRES_TRUSTED_INPUT`。`LOSSLESS` 与 `LOSSY_ALLOWED` 不得同时
设置；`PRIMARY` 不得与 `HIDDEN` 或 `DIAGNOSTIC` 同时设置；信任 flag 必须与
`trust_policy` 一致。

closure policy：`SELF_ONLY=0`、`REQUIRED_RELATIONS=1`、`DECLARED_RELATIONS=2`、
`PROJECTOR_DEFINED=3`。需要可信输入的 projection 不得使用 `PROJECTOR_DEFINED`。
`DECLARED_RELATIONS` 的配置必须来自本文件内的非 `EXTERNAL_REF` blob，以便在验签前
确定输入闭包。

同一源对象、目标协议和目标版本最多有一个 `PRIMARY` projection。没有 projection 的
对象图只可检查和展示；没有可用 PRIMARY projector 时，默认操作必须失败，不得假装成功。

## 10. Relation Table

每条 Relation 记录为 80 字节：

| 偏移 | 大小 | 字段 | 说明 |
| ---: | ---: | --- | --- |
| `0x00` | 8 | `relation_id` | 唯一 id，`0` 保留。 |
| `0x08` | 8 | `source_object_id` | 源对象。 |
| `0x10` | 8 | `target_object_id` | 目标对象；无目标为 `0`。 |
| `0x18` | 4 | `schema_id` | relation payload schema；无 payload 可为 `0`。 |
| `0x1c` | 4 | `flags` | `REQUIRED`、`ORDERED`、`WEAK`、`PRIVATE`。 |
| `0x20` | 4 | `relation_type_id_off` | relation identifier，非零。 |
| `0x24` | 4 | `name_off` | 展示名；可为 `0`。 |
| `0x28` | 8 | `payload_blob_id` | payload blob；无 payload 为 `0`。 |
| `0x30` | 8 | `payload_offset` | blob 内偏移。 |
| `0x38` | 8 | `payload_size` | payload 大小。 |
| `0x40` | 16 | `reserved0` | 必须全为 `0`。 |

`REQUIRED` 与 `WEAK` 不得同时设置；required relation 必须有非零目标。Core 只沿
`source_object_id -> target_object_id` 方向遍历。每个源对象的 relation 按 relation_id
升序，最终闭包按 object_id 升序输出。实现必须维护已访问集合，遇到已访问对象时停止
展开该边。schema 若要求 DAG，检测到环时 projection 必须失败。

实现必须为闭包设置最大对象数、relation 数和遍历深度。超过限制时当前 projection
确定性失败，不能使用不完整闭包继续投影。

## 11. Metadata Binding Table

每条 binding 记录为 32 字节：

| 偏移 | 大小 | 字段 | 说明 |
| ---: | ---: | --- | --- |
| `0x00` | 8 | `binding_id` | 唯一 id，`0` 保留。 |
| `0x08` | 2 | `owner_kind` | `FILE=1`、`BLOB=2`、`OBJECT=3`、`SCHEMA=4`、`PROJECTION=5`、`RELATION=6`、`EXTERNAL_REF=7`。 |
| `0x0a` | 2 | `flags` | `MANDATORY=1`、`DISPLAY_ONLY=2`、`DIAGNOSTIC=4`。 |
| `0x0c` | 4 | `metadata_schema_id` | mandatory 时非零。 |
| `0x10` | 8 | `owner_id` | 归属记录 id；FILE 必须为 `0`。 |
| `0x18` | 4 | `tlv_first` | TLV ordinal。 |
| `0x1c` | 4 | `tlv_count` | 覆盖的 TLV 数，必须非零。 |

`MANDATORY` 不得与展示或诊断 flag 同时设置；mandatory binding 的 schema kind 必须
为 `METADATA` 或 `VIEW`。同一 owner 下 mandatory 的 TLV 范围不得重叠，不同 owner
可以共享同一段 TLV。TLV 链结构错误时，所有 binding 视为不可解析；不依赖 mandatory
metadata 的对象图检查仍可以继续。

## 12. External Reference Table

每条外部引用记录为 80 字节：

| 偏移 | 大小 | 字段 | 说明 |
| ---: | ---: | --- | --- |
| `0x00` | 8 | `external_ref_id` | 唯一 id，`0` 保留。 |
| `0x08` | 2 | `kind` | `SCHEMA=1`、`BLOB=2`、`OBJECT=3`、`SOYO_FILE=4`、`PROJECTOR=5`、`TRUST_ANCHOR=6`。 |
| `0x0a` | 2 | `flags` | `REQUIRED=1` 或 `DISPLAY_ONLY=2`。 |
| `0x0c` | 4 | `reserved0` | 必须为 `0`。 |
| `0x10` | 4 | `identifier_off` | 外部 identifier；可为 `0`。 |
| `0x14` | 4 | `version_off` | 版本；可为 `0`。 |
| `0x18` | 4 | `locator_off` | resolver 私有定位 identifier。 |
| `0x1c` | 4 | `namespace_off` | resolver 命名空间 identifier。 |
| `0x20` | 2 | `hash_alg` | 外部内容哈希算法。 |
| `0x22` | 2 | `reserved1` | 必须为 `0`。 |
| `0x24` | 4 | `reserved2` | 必须为 `0`。 |
| `0x28` | 32 | `content_hash` | 外部内容完整性哈希。 |
| `0x48` | 8 | `reserved3` | 必须为 `0`。 |

`REQUIRED` 外部引用必须提供非零完整性哈希；解析失败或哈希不匹配时，引用它的
projection 必须失败。`locator` 和 `namespace` 不定义网络、文件系统或包管理语义，
Core 不得自动下载、执行或解析外部内容。v1 的 PROJECTOR 外部引用只能用于诊断。

## 13. 信任边界

需要可信输入的 projection（设置 `REQUIRES_TRUSTED_INPUT` 或 trust policy）必须先
收集输入闭包，再发现并验证 `soyo.trust.signature` / `soyo.trust.signature.v1`
对象。至少一个信任对象必须完整覆盖当前 projection 的最低覆盖集并验证成功。

最低覆盖集包括：完整 Header 和 String Table；相关核心表记录；当前 projection、源对象、
闭包对象及其 payload；实际遍历的 required/declared relation 及其 payload；相关 schema
记录和 descriptor；projection 配置；参与投影的 mandatory metadata 与 TLV；required
external reference 记录；信任锚记录；以及信任对象自身记录和 payload（签名字节在规范化
时置零）。外部内容本身不进入规范化流，只能由 resolver 依据 content_hash 校验。

### 13.1 规范化字节流

规范化流名称为 `SOYO_CANONICAL_STREAM_V1`。每个 chunk 使用 little-endian 头部：

```text
kind:   u16
flags:  u16       // v1 必须为 0
id:     u64
offset: u64
size:   u64
payload: [u8; size]
```

kind `1..12` 分别表示 Header、String Table、Blob Table、Object Table、Schema Table、
Projection Table、Relation Table、Metadata Binding Table、External Reference Table、TLV
条目、blob 字节范围和单条 external reference 记录。chunk 按 `(kind, id, offset, size)`
升序拼接，不得重复、越界或部分覆盖通用表。ZERO blob 生成等长零字节，EXTERNAL_REF
blob 不生成本地 blob chunk。

### 13.2 标准签名 payload

`soyo.trust.signature.v1` payload 使用以下固定头部：

```text
version:          u16       // 1
algorithm:        u16       // 1 = ED25519
digest_algorithm: u16       // 1 = BLAKE3_256
reserved0:        u16
coverage_count:   u32
zero_range_count: u32
coverage_offset:  u32
zero_range_offset:u32
key_id_offset:    u32
key_id_size:      u32
signature_offset: u32
signature_size:   u32       // ED25519 为 64
reserved1:        [u8; 16]
```

coverage entry 为 `kind:u16, flags:u16, id:u64, offset:u64, size:u64`；zero range entry
为 `offset:u32, size:u32`。所有范围必须位于 trust payload 内。`key_id` 必须是合法
identifier，并逐字节匹配唯一 TRUST_ANCHOR 引用。zero range 必须完整覆盖签名字节，且
签名字节范围不得与其它对象 payload、relation payload、descriptor 或 projection config
重叠。Core 不规定证书格式、吊销策略和本地公钥存储。

## 14. 强制性、未知值和能力边界

- 当前 projection 闭包外的未知 optional object、schema、relation 或 TLV 可以跳过；
- 闭包内的 unknown required object、schema、relation 或 external reference 必须失败；
- 未知 projection 可以展示但不得假装已完成投影；PRIMARY 不可用时默认操作失败；
- 未知 non-mandatory TLV 可以跳过；unknown mandatory metadata 必须拒绝对应 owner；
- SOYO Core 不定义系统调用、设备、VFS、网络或 ELM 权限；这些能力必须由 schema 和
  projector 转换为目标运行时理解的对象，无法表达 required 能力时必须失败。

## 15. TLV 扩展元数据

TLV 由 8 字节头部和 payload 组成：

```text
tag:     u32
len:     u32
payload: [u8; len]
```

链范围只由 Header 的 `tlv_offset` 和 `tlv_size` 定义。`tag == 0 && len == 0` 是可选
终止符，终止符后的字节必须全零。tag 为零但 len 非零、头部或 payload 截断、终止符后
出现非零字节均为结构错误。bytes、utf8、kv（每行 `key=value`）和 tag-specific-binary
是预定义编码；mandatory payload 解码失败时对应 owner 必须失败。

预定义 tag：

| tag | 名称 | 编码 | 用途 |
| ---: | --- | --- | --- |
| 0 | `TLV_END` | - | 终止符，len 必须为 0。 |
| 1 | `BUILD_ID` | bytes | 构建标识。 |
| 2 | `DISPLAY_NAME` | utf8 | 展示名称。 |
| 3 | `PACKAGE_NAME` | utf8 | 软件包或组件名称。 |
| 4 | `PACKAGE_VERSION` | utf8 | 软件包或组件版本。 |
| 5 | `BUILD_INFO` | kv | 构建时间、模式和目标信息。 |
| 6 | `TOOLCHAIN_INFO` | kv | 编译器、projector 和生产工具信息。 |
| 7 | `SOURCE_INFO` | kv | 仓库、提交和源码来源。 |
| 8 | `FEATURE_SUMMARY` | kv | 展示用功能摘要。 |
| 9 | `RUNTIME_HINT` | kv | 运行环境提示。 |
| 10 | `SCHED_HINT` | kv | 调度展示提示。 |
| 11 | `MEMORY_HINT` | kv | 内存和栈提示。 |
| 12 | `PERF_HINT` | kv | 性能展示提示。 |
| 13 | `TEST_PROFILE` | kv | 测试配置或检查点。 |
| 14 | `DEMO_NOTE` | utf8 | 展示说明。 |
| 15 | `POLICY_NOTE` | kv | 策略说明；v1 不定义机器策略。 |
| 16 | `RESOURCE_BUDGET` | kv | CPU、内存、句柄和 I/O 预算。 |
| 17 | `SCHEMA_BUNDLE` | bytes | 仅展示或诊断，不能替代 descriptor。 |
| 18 | `PROJECTOR_HINT` | kv | projector 选择或调试提示。 |
| 19 | `OBJECT_NOTE` | utf8 | 对象说明。 |
| 20 | `RELATION_NOTE` | utf8 | 关系说明。 |

已分配 tag 的含义不得改变；废弃 tag 的编号保留；新增 tag 使用未分配编号。v1 不定义
Core 级 mandatory 机器策略。

## 16. 统一校验清单

解析、检查、投影和导出前至少必须完成以下检查：

1. magic、version、header_size、file_size、所有保留字段和 entry size 正确；
2. 所有表范围、TLV 范围、blob 范围和乘加运算无溢出且互不越界；
3. count 为零的表 offset 为零，字符串池存在且 offset 0 为 NUL；
4. 所有 id 在各自表内唯一且非零，所有非零字符串偏移指向合法 UTF-8；
5. hash、source_kind、compression、schema kind/format/flags 均为 v1 已知值；
6. blob、descriptor、object payload、relation payload 和 projection config 的范围有效；
7. object、projection、relation、binding 和 external reference 的交叉引用有效；
8. REQUIRED/WEAK、LOSSLESS/LOSSY、PRIMARY/HIDDEN/DIAGNOSTIC 等互斥约束成立；
9. projection 闭包按确定顺序收集并受资源上限约束；
10. mandatory metadata、required 外部引用和 trusted input 满足相应校验；
11. 仅在显式支持 schema 和 projector 时执行、投影或导出。

校验通过只表示容器可以安全遍历，不表示对象具有可执行语义。

### 16.1 资源限制

实现必须配置并公开最大文件、字符串池、字符串长度、blob、对象、schema、projection、
relation、metadata、外部引用、TLV、单 blob、闭包对象、闭包 relation 和遍历深度。当前
`libs/soyo` 的 Wire profile 另有独立限制（见第 21 节）。超过限制必须确定性失败，并
区分 malformed、unknown、trust failure 和 resource exhausted。

### 16.2 验收场景

v1 检查器至少应覆盖以下固定场景：

- 合法的 embedded schema descriptor 和 external schema descriptor 通过；
- Header 的 `file_size` 与实际大小不一致失败；
- 压缩 FILE_RANGE blob 解压后长度正确时通过，覆盖核心元数据范围时失败；
- `SCHEMA_BUNDLE` 被当作 Schema Table descriptor 时失败；
- SEALED + HASH_NONE、SEALED + `format=NONE` 时失败；
- object 引用非 `OBJECT_PAYLOAD` schema 时失败；
- projection 的 input schema 与 source object schema 不一致时失败；
- 同一目标协议出现两个 PRIMARY projection 时失败；
- REQUIRED relation 无目标、或 REQUIRED 与 WEAK 同时设置时失败；
- mandatory metadata 的 schema 非 METADATA/VIEW，或 binding 范围越界时失败；
- required external reference 缺失哈希、EXTERNAL_REF blob 引用错误 kind 时失败；
- TLV 链截断、终止符后存在非零字节时报告结构错误，并阻止依赖 mandatory metadata 的 owner；
- trusted input coverage 缺失、重复、部分覆盖表或签名 zero range 不完整时，可信 projection 失败；
- 闭包深度、对象数或 relation 数超过实现限制时，以资源耗尽失败而不是使用部分闭包；
- 未知 optional 数据可展示或跳过，未知 required 数据必须阻止相关 projection。

## 17. 标准扩展对象和 projector

以下扩展属于普通对象，不是 Core 的内建语义；不支持时必须按未知 schema/projector
处理。

### 17.1 字段对象

标准 identifier：`soyo.fields.v1` 和关系 `soyo.has-fields`。payload 头部为：

```text
version:u16, flags:u16, field_count:u32,
field_table_offset:u32, reserved:u32
```

field entry 为 `key_off:u32, schema_id:u32, value_kind:u16, flags:u16, aux:u32,
value0:u64, value1:u64`。value_kind `0..10` 依次表示 NULL、U64、I64、BOOL、字符串偏移、
blob id、object id、schema id、relation id、projection id 和 blob 中字节范围。bit 0/1/2
分别表示 REQUIRED_BY_SCHEMA、NO_STRIP 和保留策略。字段对象必须通过
`soyo.has-fields` 绑定到 owner，引用的对象、blob、schema 和关系必须存在。

### 17.2 策略对象

标准 identifier：`soyo.policy.set.v1`、`soyo.policy.item.v1` 和关系
`soyo.applies-policy`。策略集合头部为 `version:u16, flags:u16, policy_count:u32,
policy_table_offset:u32, reserved:u32`。策略项包含 policy schema、subject kind/id、
payload blob/range 和 mandatory/audit-only flags。MANDATORY 与 AUDIT_ONLY 不得同时设置；
mandatory 策略不支持、解析失败或安装失败时相关 projection 必须失败。

### 17.3 可执行映像 projector

推荐 identifier：`soyo.exec.manifest.v1`、`soyo.exec.segment.v1`、
`soyo.exec.projector.config.v1`、目标 `soyo.exec.image.v1` 和 projector
`soyo.projector.exec-image.v1`。manifest 至少包含 version、arch、abi、endian、pointer
width、flags、entry_vaddr、默认栈、段/重定位/能力表位置。段包含 vaddr、mem_size、
file_size、blob_id、blob_offset、权限和 align。

projector 必须检查入口位于 EXECUTE 段、段不重叠、整数不溢出、W+X 策略、blob 范围和
可信覆盖。输出应为不再依赖 SOYO 文件偏移的 ExecImageHeader、ExecSegmentList、
ExecRelocationList 和 ExecCapabilityList。

### 17.4 EBI projector

推荐 identifier：`elm.unit.schema.v1`、`elm.unit.segment.v1`、`elm.unit.symbols.v1`、
`elm.unit.relocations.v1`、`elm.ebi.v1` 和 projector `elm.projector.soyo-to-ebi.v1`。
ELM manifest 描述 unit kind、target arch、rust ABI、初始化/终止 hook、段、符号、重定位、
导入、导出和拓展点。`HOTPLUG_SAFE`、`STATEFUL`、`MANAGER_REQUIRED`、`PROVIDES_API`、
`CONSUMES_API` 和 `TOOL_ONLY` 等 flag 必须由 projector 检查。

EBI projector 的输入闭包必须包含 required 的 `elm.depends-on`、`elm.imports`、
`elm.exports` 和 `elm.extends` 关系；weak extends 缺失只能作为诊断。输出为 EBI header、
段表、符号表、重定位表、导入/导出集合和 metadata 集合。投影完成后运行时只消费 EBI
对象，不得长期依赖 SOYO blob 或 SOYO 文件偏移。

## 18. 版本兼容

兼容性分为四层：

- Core version：Header、通用表项、通用强制规则或安全边界不兼容时提升 `version`；
- Schema version：由 schema identifier、版本字符串和 schema_hash 管理；
- Projector ABI：由 projector identifier、version 和 `projector_abi` 管理；
- Target protocol version：由目标协议 identifier 和版本管理。

新增 schema、projector 或非 mandatory TLV 不影响旧 Core；旧实现可以展示但不得执行未知
对象。旧实现遇到 `version > 1` 时，只能读取 magic、version、header_size 等诊断字段，
不得按 v1 继续解析、投影或导出，也不得降级执行。

## 19. 生产工具与实现约束

生产工具必须生成完整核心表、schema descriptor、projection、relation、metadata binding
和 external reference；不得把业务主语义塞进 TLV。输出外部依赖时必须写入 External
Reference 记录，Core 不负责取回。检查工具应能显示所有核心表、TLV、信任对象和可用
projector；删除工具不得默认删除 required 对象/schema/projection/relation、mandatory
metadata 或 required 外部引用。

本仓库的实现对应关系：

| 组件 | 责任 |
| --- | --- |
| `libs/soyo` | no_std 解析、解码、结构校验、信任和映射规划。 |
| `libs/soyo/wire-abi.registry` | 当前 Wire profile 的唯一机器可读线格式来源。 |
| [`hitoshizuku-soyo-linker`](https://github.com/redstone6835/hitoshizuku-soyo-linker) | 主机端 ELF 到 SOYO Wire profile 的链接和检查工具。 |
| `kernel` | 在显式启用相应 projector/profile 后装载对象，不根据文件扩展名猜测语义。 |

## 20. 迁移说明

早期草案中的 `Header.profile`、`SOYO_PROFILE_PROCESS`、`SOYO_PROFILE_ELM_EBI`、通用
Segment Table 和 Required Directory 不属于 SOYO Core。进程映像、ELM/EBI、归档、调试
对象和设备树 overlay 都应迁移为独立 schema 与 projector。Core 只承载它们，不内建其
业务字段。

## 21. 当前实现：SOYO Wire 可执行 profile v1

本节记录 `libs/soyo/src` 当前实际实现的另一种固定 wire 格式。它由
`libs/soyo/wire-abi.registry` 生成 Rust 常量，修改 registry 必须同步修改本节、测试和
`WIRE_ABI_GENERATION`。它不是第 3 节的对象容器 Header，二者不能混读。

### 21.1 Wire Header

Wire Header 同样为 192 字节，但字段完全不同：

| 偏移 | 字段 | 说明 |
| ---: | --- | --- |
| `0x00` | `magic[4]` | `soyo`。 |
| `0x04` | `format_version:u16` | v1 为 `1`。 |
| `0x06` | `header_size:u16` | `192`。 |
| `0x08` | `artifact_kind:u16` | `1=Executable`，`2=SharedComponent`。 |
| `0x0a` | `target_arch:u16` | `riscv64` 或 `loongarch64`。 |
| `0x0c` | `endian:u8` | `1=little-endian`。 |
| `0x0d` | `pointer_width:u8` | 当前必须为 `64`。 |
| `0x0e` | `abi_family:u16` | 非零 ABI 家族。 |
| `0x10` | `abi_epoch:u16` | ABI 世代。 |
| `0x12` | `hash_algorithm:u16` | 当前 `1=SHA256`。 |
| `0x14` | `flags:u32` | v1 必须为 `0`。 |
| `0x18` | `required_features:u64` | `STATIC_TLS=1`、`INIT_FINI_ARRAY=2`、`DYNAMIC_COMPONENTS=4`。 |
| `0x20` | `optional_features:u64` | 不得与 required 重叠。 |
| `0x28` | `entry_offset:u64` | Executable 的代码入口。 |
| `0x30` | `table_offset:u64` | Directory 起点，必须为 `192`。 |
| `0x38` | `table_count:u32` | Directory 数量。 |
| `0x3c` | `table_entry_size:u16` | `48`。 |
| `0x40` | `file_size:u64` | 实际文件大小。 |
| `0x48` | `image_virtual_size:u64` | 非零、页对齐且不超过实现上限。 |
| `0x50` | `build_id[32]` | 构建标识。 |
| `0x70` | `content_hash[32]` | 文件内容哈希。 |
| `0x90` | `reserved[112]` | 必须全零。 |

Wire Directory 每项 48 字节，按 table type 严格递增：

```text
table_type:u16, flags:u16, entry_size:u32, entry_count:u32,
reserved0:u32, file_offset:u64, file_size:u64, alignment:u64,
reserved1:u64
```

Directory flags 只有 `REQUIRED=1`。每个 table 的 `file_size` 必须等于
`entry_size * entry_count`，范围必须在文件内且按 alignment 对齐。当前 table type 为：

| 值 | 名称 | entry 大小 |
| ---: | --- | ---: |
| 1 | String | 1 |
| 2 | ImageSegment | 64 |
| 3 | AbiImport | 64 |
| 4 | CapabilityRequirement | 64 |
| 5 | Relocation | 48 |
| 6 | RuntimeInfo | 96 |
| 7 | ComponentInfo | 128 |
| 8 | ComponentDependency | 96 |
| 9 | SymbolImport | 96 |
| 10 | SymbolExport | 96 |
| 11 | DynamicRelocation | 48 |
| 12 | Signature | 128 |

### 21.2 Wire ImageSegment 与重定位

ImageSegment 的 64 字节布局为：

```text
kind:u16, permissions:u16, flags:u32,
virtual_offset:u64, file_offset:u64, file_size:u64,
memory_size:u64, alignment:u64, reserved0:u64, reserved1:u64
```

kind：`1=Code`、`2=Rodata`、`3=Data`、`4=Bss`、`5=TlsTemplate`。权限 bit 为
READ=1、WRITE=2、EXECUTE=4。普通段按页对齐、互不重叠，入口必须落在 Code 的文件范围；
TLS 模板只能出现一次，大小不超过实现上限。重定位 kind 为 `ImageBase64=1`、
`SegmentBase64=2`；目标必须是可写数据类段中的 8 字节对齐位置，所有 addend、segment
索引和范围均需检查。

其余 Wire 表的字段由 registry 生成的常量定义，不能依赖 Rust 结构体布局。ABI import、
capability、runtime、component、symbol 和 dynamic relocation 的保留字段必须为零；
未知 required feature、table type、hash algorithm、段类型或 relocation 必须明确报告
unsupported，而不是静默跳过。

Wire 表项的完整字段布局如下（所有整数均为 little-endian；未列出的字节为 reserved，
必须为零）：

**AbiImport（64 字节）**

```text
0x00 slot:u32                 0x04 operation_id:u32
0x08 flags:u32                0x0c diagnostic_name_offset:u32
0x10 signature_hash:[u8;32]   0x30 reserved:[u8;16]
```

`flags` 的 bit 0 为 REQUIRED，bit 1 为 OPTIONAL，二者不得同时设置。signature_hash
来自 Native ABI registry；diagnostic_name_offset 指向 String Table。

**CapabilityRequirement（64 字节）**

```text
0x00 requirement_id:u32       0x04 object_interface:u16
0x06 flags:u16                0x08 required_rights:u64
0x10 diagnostic_name_offset:u32
0x14 reserved0:u32            0x18 reserved1:[u8;40]
```

flags bit 0/1 分别为 REQUIRED/OPTIONAL。`object_interface` 和 rights 的业务解释由
Native ABI 与目标运行时定义，Core 只检查范围和保留位。

**Relocation（48 字节）**

```text
0x00 kind:u16                 0x02 flags:u16
0x04 target_segment_index:u32
0x08 target_offset:u64        0x10 source_segment_index:u32
0x14 reserved0:u32            0x18 addend:i64
0x20 reserved1:u64             0x28 reserved2:u64
```

kind `1=ImageBase64`、`2=SegmentBase64`。目标段必须是 Rodata、Data 或 Bss，目标偏移
8 字节对齐且能容纳一个 u64；ImageBase64 的 source index 为 `u32::MAX`，SegmentBase64
必须引用非 TLS 源段。

**RuntimeInfo（96 字节）**

```text
0x00 stack_size:u64            0x08 stack_guard_size:u64
0x10 runtime_flags:u64         0x18 init_array_offset:u64
0x20 init_array_count:u32      0x24 init_array_entry_size:u16
0x26 reserved0:u16             0x28 fini_array_offset:u64
0x30 fini_array_count:u32      0x34 fini_array_entry_size:u16
0x36 reserved1:u16             0x38 stack_alignment:u32
0x3c start_info_max_size:u32   0x40 reserved2:[u8;32]
```

Runtime flags bit 0/1 为 RUN_INIT_ARRAY/RUN_FINI_ARRAY。数组非空时 entry size 必须为
8、偏移须位于 Rodata，数组为空时 offset、count 和 entry size 必须同时为零。

**ComponentInfo（128 字节）**

```text
0x00 component_id:[u8;16]      0x10 abi_id:[u8;16]
0x20 flags:u64                 0x28 init_offset:u64
0x30 fini_offset:u64           0x38 interface_count:u32
0x3c reserved0:u32             0x40 call_state_size:u64
0x48 reserved1:[u8;56]
```

**ComponentDependency（96 字节）**

```text
0x00 component_id:[u8;16]      0x10 abi_id:[u8;16]
0x20 content_hash:[u8;32]      0x40 flags:u32
0x44 diagnostic_name_offset:u32
0x48 reserved:[u8;24]
```

**SymbolImport（96 字节）**

```text
0x00 dependency_index:u32      0x04 flags:u32
0x08 interface_id:[u8;16]      0x18 symbol_id:[u8;16]
0x28 signature_hash:[u8;32]    0x48 diagnostic_name_offset:u32
0x4c reserved0:u32             0x50 reserved1:[u8;16]
```

**SymbolExport（96 字节）**

```text
0x00 interface_id:[u8;16]      0x10 symbol_id:[u8;16]
0x20 signature_hash:[u8;32]    0x40 entry_offset:u64
0x48 flags:u32                 0x4c diagnostic_name_offset:u32
0x50 reserved:[u8;16]
```

**DynamicRelocation（48 字节）**

```text
0x00 kind:u16                 0x02 flags:u16
0x04 target_segment_index:u32
0x08 target_offset:u64        0x10 source_index:u32
0x14 reserved0:u32             0x18 addend:i64
0x20 reserved1:u64             0x28 reserved2:u64
```

kind `1=AbiSlot32`、`2=AbiSlot64`、`3=InterfaceGate`、`4=TlsOffset64`。其余约束由
Native ABI、组件信息和目标架构 projector 定义；Core 仍必须检查索引、对齐、范围和
保留字段。

**Signature（128 字节）**

```text
0x00 key_id:[u8;32]            0x20 signature:[u8;64]
0x60 flags:u32                0x64 reserved:[u8;28]
```

Wire Signature 的密钥解析和信任锚由运行环境提供。flags 的未定义 bit 必须为零；Wire
格式的 content_hash 使用 Header 声明的 SHA-256，不得把 Wire Signature 与第 13 节
SOYO Core 对象签名 payload 混用。

### 21.3 Wire 签名与资源上限

Wire signature entry 为 `key_id[32]`、`signature[64]`、`flags:u32` 和 28 字节保留区，
当前哈希算法为 SHA-256。Wire 解析器必须执行 `libs/soyo` 中的文件大小、映像大小、表
数量、段数量、导入、能力、重定位、组件依赖和符号数量限制；内核可以使用更严格的
`SoyoReadLimits`。资源耗尽、格式错误和不支持版本必须区分返回。

当前实现的默认上限如下；这些数值不是 Core 对象容器的全局上限：

| 项目 | 上限 |
| --- | ---: |
| 文件大小 | 256 MiB |
| 映像虚拟大小 | 1 GiB |
| Directory 项数 | 64 |
| String Table | 1 MiB |
| ImageSegment | 32 |
| ABI import | 256 |
| capability requirement | 64 |
| 静态 relocation | 65,536 |
| TLS 模板 | 16 MiB |
| component dependency | 256 |
| symbol import/export | 各 4,096 |
| dynamic relocation | 65,536 |

## 22. 参考实现和变更流程

修改本标准时按以下顺序进行：

1. 先修改本文件中对应的线布局和兼容性说明；
2. 修改 `libs/soyo/wire-abi.registry` 或 Core 编解码器；
3. 更新 `libs/soyo` 的拒绝测试、编码测试、信任测试和 linker 集成测试；
4. 更新 `hitoshizuku-soyo-linker` README、`ELM.md` 及相关 schema/projector 文档；
5. 对改变 on-disk 语义的修改提升 Core、schema、projector ABI 或 target protocol 版本。

SOYO 文件不得通过文件扩展名决定执行权限。解析器先验证结构，调用方再选择显式支持
的 schema 和 projector；任何无法理解的必需语义都必须失败。
