# 安全报告

## 1. 审查范围

| 维度 | 覆盖区域 |
|------|---------|
| unsafe 代码与内存安全 | `arch/`、`libs/allocator/`、`libs/mm/`、`general/` |
| 并发模型与锁正确性 | `libs/sched/`、`kernel/src/syscalls/`、`libs/vfs/` |
| 系统调用参数校验 | `kernel/src/syscalls/`、`general/src/mm/` |
| 文件系统与网络输入解析 | `libs/extfs/`、`libs/fatfs/`、`libs/net/` |
| 架构层硬件交互 | `arch/src/loongarch64/`、`arch/src/riscv64/` |

## 2. 总览

| 层 | 安全边界 | 已识别风险数 | 关键风险类型 |
|-----|---------|:---------:|-----------|
| arch | 汇编入口、CSR 操作、页表激活、上下文切换 | 3 | TLB 刷新屏障缺失、大页标志处理、静态变量别名 |
| libs (分配器) | 裸指针→引用转换、全局分配器状态 | 2 | slab/buddy 元数据解引用无校验 |
| libs (调度器) | WaitQueue/Mutex 协议、原子操作、per-CPU 状态 | 2 | static mut 并发读、状态覆盖窗口 |
| libs (文件系统) | 外部磁盘镜像解析 | 4 | extent 树/FAT 链深度与循环无界、描述符大小误判 |
| general | 能力注入、固件解析、用户态内存访问 | 1 | 路径字符串 TOCTOU |
| kernel | syscall 分发、权限检查、资源管理 | 3 | futex 语义偏差、返回值错误、iovec 竞态 |

## 3. 详细发现

### 3.1 文件系统

#### extent 树递归遍历无深度限制

- **文件**：`libs/extfs/src/extent.rs:70-208`、`libs/extfs/src/extent_wr.rs:19-119`
- **根因**：`map_block`、`collect_extents`、`free_tree`、`count_tree_blocks` 四个函数均使用递归下降遍历 extent 索引树。`parse_header` 仅校验魔数 `0xf30a`，不限制 `eh_depth` 字段。ext4 规范定义 depth 为 u16（最大 65535），而内核栈仅 16-64 KiB，depth 约 200 即可导致栈溢出。
- **触发条件**：挂载恶意构造的 ext4 镜像，其中某个 inode 的 extent 树深度超过约 200。任何读取该文件的操作（`read`、`execve`、`mmap`）或元数据操作（`stat`、`unlink`）均可触发。

#### extent 树索引节点循环导致死循环

- **文件**：`libs/extfs/src/extent.rs:106-133`
- **根因**：`map_block` 沿 extent 索引节点下降的过程使用 `loop { ... current = next; }` 模式，不追踪已访问节点的块号。若某索引节点的子块指针（`ei_leaf_lo`/`ei_leaf_hi`）指向自身或其祖先节点，循环永不终止。
- **触发条件**：挂载恶意 ext4 镜像，其中某个 inode 的 extent 树包含环。`read` 系统调用进入 `map_block` 即可触发，内核永久挂死。

#### FAT 目录簇链遍历无环检测

- **文件**：`libs/fatfs/src/dir.rs:178-208`
- **根因**：`scan_dir_sectors_with_scratch` 在 `ChainFromCluster` 分支中通过 `state.fat.next_cluster()` 逐簇沿 FAT 链表推进。循环内无步数上限、无环检测（如龟兔赛跑或已访问集合）、无基于 `total_clusters` 的 bound 检查。`cluster_index` 使用 `saturating_add` 在 `u32::MAX` 饱和，但不中断循环。
- **触发条件**：挂载恶意 FAT 镜像，其中某目录的 FAT 簇链形成环（如簇 5→10→5）。任何访问该目录的操作（`open`、`stat`、`getdents64`、`chdir`）均可触发。

#### INCOMPAT_64BIT 置位且 s_desc_size=0 时描述符大小误判

- **文件**：`libs/extfs/src/sb.rs:117-119`
- **根因**：超级块解析逻辑中，当 `INCOMPAT_64BIT` 特性位置位且 `s_desc_size` 字段（偏移 254）为零时，代码使用 `desc_size = 32`。但 ext4 规范规定：若 `INCOMPAT_64BIT` 存在且 `s_desc_size` 为零，应默认使用 64 字节描述符。32 字节描述符下，`block_bitmap_hi`、`inode_bitmap_hi`、`inode_table_hi` 等所有高 32 位字段从错误偏移量读取，后续块/inode 分配操作使用垃圾物理地址。
- **触发条件**：挂载设置了 `INCOMPAT_64BIT` 但 `s_desc_size` 为零的 ext4 镜像（可通过手工修改超级块构造）。任何分配/释放块或 inode 的操作均受影响。

### 3.2 内存管理

#### slab 与 buddy 分配器元数据解引用无校验

- **文件**：`libs/allocator/src/slab.rs:1483-1485`、`libs/allocator/src/buddy.rs:2160`
- **根因**：`slab_node_mut(addr: usize) -> &'static mut SlabNode` 和 `node_mut(addr: usize) -> &'static mut BlockNode` 将任意 `usize` 值直接转换为可变引用，不对地址是否在分配器管理的物理内存范围内做任何校验。这些函数被内部链表遍历路径调用——若链表中相邻节点的指针因堆溢出被破坏，伪造的地址将直接解引用。
- **触发条件**：分配器管理的某个对象发生堆越界写入，覆盖相邻 slab 节点的 `header`/`avail_list` 或 buddy 节点的链表指针。后续分配/释放操作遍历破坏后的链表时触发。

### 3.3 并发

#### static mut POWER_CONTROLS 无锁读-改-写竞争

- **文件**：`general/src/firmware/power.rs:119-209`
- **根因**：`install_one` 函数执行「读取 `static mut POWER_CONTROLS` → 修改字段 → 写回」序列，无互斥锁保护。在 SMP 环境下，两个 CPU 同时调用 `install_one`（分别来自 ACPI 和 DTB 初始化路径）会产生经典的 TOCTOU 竞争——后完成的写入会静默覆盖先完成的安装。
- **触发条件**：多核启动，ACPI 与 DTB 电源控制初始化路径并行执行。

#### static mut INIT_TASK / ROOT_PID_NS 并发读取

- **文件**：`libs/sched/src/scheduler.rs:167-169`
- **根因**：`static mut INIT_TASK: Option<Arc<Task>>` 和 `static mut ROOT_PID_NS` 由 BSP 在 `INIT_READY` 标志 Release 之前初始化写入，之后所有 CPU 通过 `root_task()` 等函数读取。然而 Rust 内存模型规定 `static mut` 在任何时刻最多被单一线程访问——多核同时读取即构成未定义行为（即使读取的是不可变引用）。当前实现依赖 `INIT_READY` 的 Release/Acquire 同步写侧，但读侧缺乏相应的原子保护。
- **触发条件**：SMP 环境下 AP 启动后调用 `root_task()`。

### 3.4 系统调用

#### FUTEX_CMP_REQUEUE 锁外值比较违反 Linux 语义

- **状态**：已于 2026-07-20 修复。传统 futex 与 futex2 入口都会先在锁外完成 fault-in，再在 `FUTEX_TABLE` 锁内通过 nofault 原子读取重新比较；只有比较成功才在同一临界区迁移等待者。2026-07-21 又补齐了 WAIT/WAITV 的比较-登记事务，以及 WAKE_OP/PI 用户字的 CAS 更新。
- **文件**：`kernel/src/syscalls/process.rs:2581-2669`、`kernel/src/syscalls/process.rs:2891-2916`、`kernel/src/syscalls/process.rs:3105-3129`、`general/src/mm/vm_space.rs:1131-1162`
- **根因**：`FUTEX_CMP_REQUEUE` 的实现分两步：(1) 在获取 futex bucket 锁之前，通过 `copy_from_user` 读取 `uaddr` 处的用户值并与 `val3` 比较；(2) 匹配后获取 bucket 锁，调用 `futex_requeue_key` 将等待者从源 futex 转移到目标 futex。但第二步不再重新检查 `uaddr` 处的值。Linux 规范要求条件比较必须在锁内进行——线程 A 可在第一步和第二步之间修改 `uaddr`，使唤醒条件不再成立，导致等待者被错误转移，在 pthread 条件变量语义下可能永久阻塞。
- **触发条件**：多线程程序使用 `pthread_cond_broadcast`（内部依赖 `FUTEX_CMP_REQUEUE`），同时另一线程修改条件变量关联的 futex 字。

#### copy_cstr_from_user 的 strnlen 与 copy 间 TOCTOU 窗口

- **文件**：`general/src/mm/user_access.rs:101-113`
- **根因**：`copy_path_from_user` 调用的 `copy_cstr_from_user` 先通过 `strnlen_user` 在用户态页面中定位 NUL 终止符确定长度，再通过 `copy_from_user` 将等长数据拷贝到内核缓冲区。两次用户态内存访问之间存在窗口——另一线程可在此期间修改路径缓冲区内容（如将路径中间的一个字符改为 NUL，或扩展路径长度超出已确认的范围）。此问题属于 POSIX API 的固有缺陷（Linux 同样存在），但本实现中两个操作之间无任何锁定或重确认机制。
- **触发条件**：多线程程序中的线程 A 调用 `openat` 等路径类系统调用，线程 B 并发修改同一路径缓冲区。

#### sys_getcwd 返回值与 Linux ABI 不一致

- **文件**：`kernel/src/syscalls/fs.rs:380`
- **根因**：Linux `getcwd` 系统调用成功时返回写入用户缓冲区的字符串长度（含 NUL 终止符），而此实现返回 `Ok(user)`——即用户缓冲区指针的值。glibc 等 libc 实现会检查返回值：若为负则视为 errno 取反，若为正则用作长度。返回一个高地址指针值（如 `0x0000004000800000`）将被 libc 误判。
- **触发条件**：任何 `getcwd(buf, size)` 调用。

#### sendmsg/recvmsg iovec 数组两轮扫描间的竞态

- **文件**：`kernel/src/syscalls/fs.rs:3867-3916`
- **根因**：`sendmsg` 处理流程中，`iov_total_len_capped` 先遍历 `iovec` 数组计算总数据长度并据此预分配内核缓冲区，随后 `copy_send_iovecs` 再次遍历 `iovec` 数组逐一执行 `copy_from_user` 拷贝数据。两轮遍历之间，用户态另一线程可修改 `iovec` 数组内容（`iov_base`/`iov_len`），导致分配的内核缓冲区大小与实际拷贝数据量不匹配。`copy_from_user` 自身的缺页修复机制限制了越界写风险，但可导致发送数据内容与调用者预期不一致。
- **触发条件**：多线程程序中的线程 A 调用 `sendmsg`/`writev`，线程 B 并发修改同一 `iovec` 数组。

### 3.5 架构层

#### RISC-V sfence.vma 前缺少页表写屏障

- **文件**：`arch/src/riscv64/paging.rs:137-160`
- **根因**：`activate_with_asid` 函数在写入 `satp` CSR 后执行 `sfence.vma` 刷新 TLB。但在写入 `satp` 之前，未执行 `fence w,w` 来保证此前对页表内存的存储操作已全局可见。RISC-V 规范不保证 `csrw satp` 隐含任何存储屏障——在弱排序实现上，页表写入可能被推迟到 `satp` 切换之后，此时 TLB 硬件遍历可能读取到旧（无效）页表项。LoongArch64 对应代码（`paging.rs:170`）有 `dbar 0` 屏障。
- **触发条件**：弱内存排序的 RISC-V 硬件实现上频繁切换地址空间（如 `execve` 或 `fork` 后的 COW 页表更新）。

#### LoongArch64 大页 protect 路径 global 标志丢失

- **文件**：`arch/src/loongarch64/mm/user_pgd.rs:248-257`
- **根因**：`protect` 函数在重建 PTE 时调用 `flags_global(old_flags)` 判断是否需要保留 global 属性。该函数检查 `PTE_G`（bit 6），但 2MiB/1GiB 大页的 global 位位于 `PTE_HGLOBAL`（bit 12）。因此通过 `mprotect` 修改大页的权限时，global 标志会被清零，大页降级为非 global 映射，每次上下文切换后 TLB 失效需重新填充。
- **触发条件**：对通过 `mmap` 以 2MiB 或 1GiB 大页映射的区域调用 `mprotect` 修改权限位。
