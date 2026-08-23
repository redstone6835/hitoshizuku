# 安全审查记录

审查基线：2026-08-23，`main` 分支。本文记录源码审计中已经确认的问题和修复状态，
不是形式化安全证明。行号会随实现变化，因此以函数名和路径为准。

## 1. 审查范围

| 维度 | 覆盖区域 |
| --- | --- |
| 不安全代码与内存安全 | `arch/`、`libs/allocator/`、`libs/mm/`、`general/` |
| 并发与内存序 | `libs/sched/`、`general/`、`kernel/src/syscalls/` |
| 用户指针与系统调用 ABI | `general/src/mm/`、`kernel/src/syscalls/` |
| 不可信输入解析 | `libs/extfs/`、`libs/fatfs/`、`libs/net/` |

状态含义：**未修复**表示当前源码仍存在所述路径；**已修复**表示实现已经改变，但仍应保留
回归测试；**设计约束**表示当前实现依赖更窄的内部不变式，后续重构不得放宽该前提。

## 2. 当前状态

| 区域 | 发现 | 状态 |
| --- | --- | --- |
| extfs | extent 深度和索引环缺少统一上限 | 未修复 |
| fatfs | 目录簇链缺少环检测和遍历上限 | 未修复 |
| extfs | 64-bit 文件系统的零 `s_desc_size` 被当作 32 | 未修复 |
| allocator | buddy/slab 元数据地址依赖内部来源不变式 | 设计约束 |
| firmware | `POWER_CONTROLS` 存在无锁读改写 | 未修复 |
| scheduler | `INIT_TASK` / `ROOT_PID_NS` 的 `static mut` 并发读取 | 已修复 |
| futex | `FUTEX_CMP_REQUEUE` 比较与迁移不在同一临界区 | 已修复 |
| user access | C 字符串两次用户内存读取之间可被修改 | 未修复 |
| syscall | `getcwd` 返回用户缓冲区地址 | 已修复 |
| syscall | iovec 预扫描与复制之间可被修改 | 未修复 |
| RISC-V | 修改页表后的切换缺少 PTE store 排序 | 已修复 |
| LoongArch64 | huge leaf 权限更新不保留 `PTE_HGLOBAL` | 未修复 |

## 3. 未修复项

### 3.1 extfs extent 树边界

`libs/extfs/src/extent.rs` 的 `map_block` 迭代下降但不记录已访问块，`collect_extents` 仍
递归访问子树；`libs/extfs/src/extent_wr.rs` 的 `free_tree` 和 `count_tree_blocks` 也仍为
递归实现。恶意 extent 索引可以构造过深树或祖先环，造成栈耗尽、无限遍历或重复释放。

修复要求：统一验证 header 中的 depth、entries 和节点容量；下降路径设置与文件系统块数
一致的硬上限，并记录当前路径上的块号。释放路径还必须在执行副作用前完成完整预检，避免
发现环时只释放了半棵树。

### 3.2 FAT 目录簇链环

`libs/fatfs/src/dir.rs::scan_dir_sectors_with_scratch` 沿 `next_cluster` 遍历目录链，目前仅在
遇到 EOC 时结束，`cluster_index` 饱和也不会停止。循环 FAT 链可使目录查询永久占用 CPU。

修复要求：以数据区总簇数为最大步数，并加入环检测；所有沿 FAT 链读取目录的入口必须复用
同一验证器。

### 3.3 ext4 group descriptor 默认大小

`libs/extfs/src/sb.rs` 在 `INCOMPAT_64BIT` 置位且 `s_desc_size == 0` 时仍选择 32 字节。
兼容 ext4 的读取规则应采用 64 字节或直接拒绝该组合，不能按 32 字节继续解析高位字段。

### 3.4 固件电源控制并发发布

`general/src/firmware/power.rs` 使用 `AtomicBool` 发布 `static mut POWER_CONTROLS`，但
`install_one` 的读取、合并和写回没有互斥。并发安装 shutdown/reboot 后端会覆盖另一侧更新；
读取与写入同一非原子对象也不满足 Rust 并发访问规则。

修复要求：将完整结构放入锁或使用不可变快照原子替换；`clear`、`install`、`install_one`
和 `load_controls` 必须遵循同一同步协议。

### 3.5 用户字符串 TOCTOU

`general/src/mm/user_access.rs::copy_cstr_from_user` 先执行 `strnlen_user`，再按所得长度复制。
另一线程可以在两次读取之间修改字节。架构 fixup 能处理缺页和非法地址，但不能保证两次读取
看到同一内容。

对路径类系统调用应把用户内存视为不稳定输入：一次有界复制到内核缓冲区，再在内核副本中
查找 NUL、校验 UTF-8 和长度。不要通过锁定任意用户页来建立隐式 ABI。

### 3.6 iovec 两轮读取

`kernel/src/syscalls/fs.rs::copy_send_iovecs` 先调用 `iov_total_len_capped` 计算容量，随后再次
读取每个 iovec。用户线程可在两轮之间修改 base 或 len，使实际发送内容与预扫描不同。
当前复制长度仍受已分配缓冲区限制，因此主要风险是调用语义不一致，而不是直接越界写。

修复要求：先一次性复制并验证 iovec 描述符数组，再依据内核副本计算长度和复制 payload；
接收路径也应使用同一快照规则。

### 3.7 LoongArch64 huge leaf global 位

`arch/src/loongarch64/paging.rs::flags_global` 只检查普通 leaf 的 `PTE_G`，而目录级 huge leaf
使用 `PTE_HGLOBAL`。`mprotect` 重建 huge leaf 时可能丢失 global 属性，造成不必要的 TLB
失效和语义偏差。

修复要求：权限抽象必须携带 leaf level，或在重建 PTE 时直接根据原 PTE 类型同时保留
`PTE_G` / `PTE_HGLOBAL`；补充 2 MiB 和 1 GiB 映射回归测试。

## 4. 设计约束

`libs/allocator` 的 buddy `node_mut` 和 slab `slab_node` 会把内部保存的地址还原为引用。
当前安全性依赖节点地址只来自永不提前释放、满足布局与对齐要求的 metadata allocator，且
链表操作始终在相应 allocator 锁内。它们不是接受任意地址的公共 API。

后续若允许从磁盘、用户态、ELM 或未受同一锁保护的结构恢复节点地址，必须先加入管理范围、
对齐、节点状态和所属 allocator 校验；不能把当前内部来源不变式扩展成外部信任边界。

## 5. 已修复项与回归要求

- 调度器全局锚点已改为持有永久 `Arc` 强引用的 `AtomicPtr`，读取时使用 Acquire 并克隆
  `Arc`；不得退回并发读取 `static mut Option<Arc<_>>`。
- futex compare-requeue 已在 fault-in 后于 futex 表临界区内重新读取并比较用户字；WAIT、
  WAITV、WAKE_OP 和 PI 路径需要继续保持比较/登记或更新的事务边界。
- `sys_getcwd` 已返回包含结尾 NUL 的字节数 `needed`，不再返回用户缓冲区地址。
- RISC-V 地址空间切换通过 `needs_page_table_fence` 跟踪未排序的页表修改，并在需要时执行
  ASID 定向 `sfence.vma`；ASID generation 复用和远程 shootdown 测试必须覆盖该标志。

安全修复应同时增加最小拒绝测试或并发回归测试，并更新本文件状态。涉及 ELM 镜像、直接
内核符号或设备资源的变更还应检查 [ELM.md](ELM.md) 和
[DEVICE_ABSTRACTION.md](DEVICE_ABSTRACTION.md) 中的生命周期约束。
