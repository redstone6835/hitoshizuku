# CHANGELOG

## 2026-08-15
### feat/fs-time-full
- `feat(vfs)`：inotify 全套——fsnotify 事件核心（libs/vfs/src/fsnotify.rs：inode 监视注册表 `(fs_id,ino)`→Weak<Watch>、全局原子门控零开销、ONESHOT/EXCL_UNLINK/IGNORED 语义、DELETE_SELF 移除、rename cookie 配对、unmount 全量 IN_UNMOUNT）+ inotify 实例（libs/vfs/src/inotify.rs：wd 分配/复用、IN_MASK_ADD 合并、16384 队列上限 + Q_OVERFLOW、read EINVAL/EAGAIN、PollSource 等待、fdinfo watch 列表）+ 3 syscall（init1/add_watch/rm_watch）+ 全部注入点（open/close/read/write/pread/pwrite/truncate/chmod/chown/utimens/unlink/rmdir/rename/mkdir/mknod/symlink/link/umount）+ `/proc/self/fdinfo/<fd>` 基础设施（FileOps::show_fdinfo 默认方法 + procfs fdinfo 目录）
- `test(vfs)`：宿主单测 10 个（掩码过滤/MASK_ADD/ONESHOT/队列溢出/read 语义/DELETE_SELF/EXCL_UNLINK/cookie 配对/ONLYDIR）+ QEMU 运行时自测 ALL PASS（userland/tests/inotify_test.c：12 事件全序列断言 + fdinfo + rm_watch + ONESHOT）
- `feat(vfs)`：扩展属性（xattr）全套与 POSIX ACL——16 个 syscall（set/get/list/removexattr × path/l/fd + set/get/list/removexattrat 带 AT_EMPTY_PATH/NOFOLLOW）；VFS 语义层（user./trusted./security./system.posix_acl_* 命名空间、权限模型、目录 sticky 规则、XATTR_CREATE/REPLACE、ERANGE/E2BIG/ENODATA）；POSIX ACL 二进制格式（version 2 + tag/perm/id 条目）、校验、`posix_acl_permission` 强制（能力绕过优先）与 `posix_acl_create`（default ACL 派生 + mode 调整）；extfs 用 ext4 兼容 xattr 块（magic 0xea020000、name_index 1/2/3/4/6、值区块尾分配、i_file_acl 读写）；tmpfs 用内存表；chmod↔ACL mask 双向同步；创建路径继承 default ACL
- `test(vfs)`：宿主单测 + QEMU 运行时自测（userland/tests/xattr_test.c，tmpfs 后端 ALL PASS，覆盖权限模型/ACL/默认 ACL/双向同步）

- `feat(time)`：时间子系统完整化——`sysinfo(2)` 真实统计（1/5/15 分钟负载 avenrun 定点衰减 + 真实内存/进程数，mem_unit=1 字节单位）；POSIX timer 全套（`timer_create/settime/gettime/getoverrun/delete` + time64，SIGEV_NONE/SIGNAL/THREAD_ID，overrun 追赶计数，CLOCK_REALTIME/MONOTONIC/BOOTTIME + PROCESS/THREAD_CPUTIME_ID，DeadlineObserver 复用）；`adjtimex/clock_adjtime` NTP 状态机（ADJ_OFFSET/FREQUENCY/STATUS/TIMECONST/TICK/NANO 等，频率误差按 tick 折叠进 REALTIME 偏移，vDSO 与内核路径一致）；`ITIMER_VIRTUAL/ITIMER_PROF`（tick 级 user/system CPU 记账，两架构 timer ISR 传 from_user）；timerfd `TFD_TIMER_CANCEL_ON_SET`（Linux 语义：settime 登记、时钟设置取消、read/settime 返回 ECANCELED）
- `chore(scripts)`：新增 `wrap-kernel-elf.py`（裸内核镜像包装为最小 ELF64，使 QEMU loongarch virt 走 `-kernel` 直启路径）


## 2026-06-16
### feat/test -> dev
- 修复 `Mutex` 释放后 `dequeue_and_wake` 将 waiter 切为 `Runnable` 入队，但 `push_back -> continue` 循环不检查唤醒后是否仍在等待队列，导致无限循环；修复为 `wake_futex_waiters` 与 `Mutex` 解锁后统一执行 `restore_current_after_wait` 恢复状态再重新竞争锁
- 修复大部分 libctest 的 fail：涉及 `tmpfs` 目录遍历补齐、`net_socket` bind 错误码修正、`sys_read`/`sys_write` 返回值处理、`sched::exec` 参数传递修复
- 修复 LoongArch64 `decode_clone_register_args` 的 `a3↔a4` 交换（musl `clone.s` 按 `flags,stack,parent_tid,child_tid,tls` 传参）；完善 `UserTrapFrame::ret()` 读取 `a0`；实现 `write_linux_mcontext()` 按 Linux sigcontext 布局写入 ucontext

## 2026-06-15
### feat/testsuit -> dev
- 参照 Linux old clone ABI 调整 LoongArch64 参数解析：musl `clone.s` 按 `flags,stack,parent_tid,child_tid,tls` 传参，修正 `decode_clone_register_args` 中 `a3↔a4` 映射避免 TLS 错乱
- 修复 `alloc_mmap_range` 搜索顺序颠倒：原逻辑只搜尾部导致前半部空洞永不回收，改为先搜 `[base,cursor]` 再搜 `[cursor,limit]`
- 实现基于 `WaitQueue` 的可睡眠互斥锁（`libs/sched/src/mutex.rs`），冲突时 task 进入 `Sleeping`；`ExtRegFileOps.io_mu` 从 `Spinlock<()>` 改为 `Mutex<()>` 使 ext4 I/O 临界区可执行阻塞操作

### feat/syscall -> dev
- 修复 futex lost-wakeup 竞态：引入 `FutexWaitState`（`AtomicBool sleeping` 标记），`wake_futex_waiters` 直接 `cas_state(Sleeping→Runnable)` 和 `cas_state(Running→Runnable)` 覆盖刚发布但未睡眠的 waiter；`futex_table_contains` 二次确认避免虚假唤醒
- 修复 virtio 驱动 bio 请求下发后设备无响应时直接失败的问题；完善 block 请求生命周期管理、sysfs/devtmpfs 设备节点注册
- 优化 virtio 设备速度：提取 `VirtioBlkDmaQueue` trait 统一 MMIO/PCI 传输层；typed `VirtioBlkQueueId`；预分配 `DmaBufferPool` 请求池；`schedule_once` 改传真实时间戳取代零记账 yield

## 2026-06-14
### feat/riscv64 -> dev
- 实现 RISC-V 64 架构层完整支持：Sv48 四级页表（4KiB/2MiB/1GiB 页，PTE_V/A/D/R/W/X/U 位域）、`__riscv_exception_entry` 汇编入口通过 CSR_STVEC direct 模式注册，sscratch 栈交换区分内核/用户态 trap；SCAUSE 解码中断类型（IPI/Timer/Hardware）后分发到异常/中断/系统调用三条路径；内核上下文切换保存/恢复 callee-saved 寄存器 ra/sp/s0-s11，新线程经 `__kthread_trampoline` 首次切入
- 实现 PLIC 中断控制器驱动：DTB 匹配 `sifive,plic-1.0.0`/`riscv,plic0`，MMIO 操作 priority/enable/claim/complete 寄存器组（`PLIC_PRIORITY_BASE`/`PLIC_CLAIM_BASE`），通过 `IrqLine::Hardware(0)` 级联至通用中断框架
- 支持 VirtIO MMIO Legacy 传输层：`VirtioMmioTransport` trait 封装 legacy（QueuePFN+QueueAlign+GuestPageSize）与 modern（QueueDescLow/High+QueueReady）寄存器布局差异；`DmaBuffer::sub_view`/`from_allocation` 方法支撑 legacy virtqueue 手动内存管理
- 实现 user_copy 与 copy_from_user 汇编路径：利用 `SSTATUS.SUM` 位使 S-mode 直接访问 U 标记页；逐字节拷贝 + 每指令单条 `__ex_table` 保护，fault 时 `fault_decode` 改写 sepc 至 fixup label
- 添加 initramfs 构建逻辑：`build.rs` 为 LA/RV 分别打包 `initramfs-la.cpio`/`initramfs-rv.cpio`；Makefile `all` 目标同时构建 `kernel-la` 与 `kernel-rv`

- 修复子进程 clone 时 `UserTrapFrame.kstack_top` 被父进程值继承，`schedule_once` 通过 `set_kernel_trap_stack` 覆盖 `CSR_SSCRATCH` 为父栈顶，导致子进程中断处理写入父栈静默崩溃
- 修复 `map_range_with_policy` 在 `PreferLarge` 模式 `map_2m` 失败后直接返回 OK，未降级到 `BaseOnly` 4KiB 逐页映射，导致 PTE 未建立
- 修复 VirtIO Legacy MMIO 未写入 `GuestPageSize`（0x028，默认 0）导致 `QueuePFN = desc_dma >> 12` 计算无效
- 修复 `leaf_flags` 无条件设置 `PTE_D` 导致只读页被标记 D 位，违反 Sv48 规范（`PTE_D=1 ∧ PTE_W=0` 硬件视作非法）；扩展 boot 页表 PUD[3] 为 1GiB leaf 并补全 MMIO 映射 PUD[0]+PUD[1]

## 2026-06-13
### feat/syscall -> dev
包含此前合入 feat/syscall 的 fix/driver、fix/net、fix/sched 及新增系统调用：

fix/driver:
- extfs indirect block/bitmap/FAT 扇区/目录项缓冲复用，virtio 设置 `GuestPageSize=4096` 修复 QueuePFN 路径
- 修复 `map_range_with_policy` PreferLarge 模式 2MB 大页映射失败后返回 OK 导致错误被忽略

fix/net:
- rtmsg 扩展 AF_INET6 支持补齐 IPv6 静态/默认/运行期路由和地址配置
- Auto 接口自动启动 DHCP 状态机，校验 Raw 包头 IP 版本和出站目的地址

fix/sched:
- load_avg 周期采样缓存，均衡失败回退到 parent domain 避免无效迁移

新增系统调用:
- 补充 pthread 所需 `sys_set_robust_list`/`get_robust_list` 和 `sys_futex`
- 修复 shm vma page fault handler 缺失写权限映射导致 shm 不可写
- 通过 IP_RECVTTL/IP_PKTINFO cmsg ancilliary data 接入 UDP TTL 与 pktinfo 控制消息
- 实现 `IP_MULTICAST_IF`/`ADD_MEMBERSHIP`/`DROP_MEMBERSHIP` 组播 sockopt
- 通过 SIOCINQ ioctl 接通 raw socket 接收队列字节数查询和 FIONREAD
- 修复 mkdir 路径尾部 `/` 分隔符未 strip 导致 VFS lookup 失败
- 添加 `/dev/cpu_dma_latency` 只读 PM QoS 接口，返回 int 0 满足 busybox 功耗管理
- 扩展 sched_getattr 返回调度域放置拓扑查询

修复:
- 修复 `CLONE_CHILD_SETTID` 用 8-byte usize 写入 32-bit `pid_t` 覆盖 pthread 结构导致的 libctest 卡死
- 修复 `do_exit` 中 vma 注销 + allocator free 未调用导致进程退出时内存不回收
- 修复 memblock 释放后物理页未回归 buddy 导致 boot 分配器空间浪费
- 修复信号处理后 tty 回显与 termios 状态未恢复导致 CTRL+C 退出后无法输入

## 2026-06-11
### feat/syscall -> dev
- 通过 `Allocator` trait 封装 buddy/slab 暴露 allocator crate API，引入 TLSF 算法优化实时分配
- 补全 sched/fs/net lib 层 syscall 桥接实现，新增 `copy_from_user` 快路径
- 接入 fs/process/signal/mm 子系统完整实现的 Linux 兼容 syscall 粘合层
- `kernel/src/syscalls/nr.rs` 补齐约 300 个 Linux asm-generic syscall 编号及 stub
- 实现 `execveat`/`pidfd_open`/`memfd_create`/`timerfd`/`signalfd`，修复 LTP 因 `CLONE_CHILD_SETTID` 位宽卡死

## 2026-06-10
### fix/sched -> dev
- 实现 `sys_nice`/`setpriority`/`sched_setscheduler` 和 SMP wake_affine 远端唤醒均衡
- 添加 SMT/MC/NUMA 层级调度域拓扑和 cpumask 层级包含关系
- CFS 迁移 fair 任务时修正 vruntime 跨队列保持公平性
- 通过 `cpumask_allowed` 校验和 `task_rq_lock` 确保按目标队列更新调度实体
- 修正 `sched_getattr`/`setattr` 空指针返回 EFAULT，收拢实时优先级 [0,99] FIFO/RR 边界
- 校验 `capable(CAP_SYS_NICE)` 权限和用户/组 ID 匹配

### fix/alloc -> dev
- 注册表引入哈希桶链取代线性扫描，slab 热路径 O(n)→O(1) 优化

### fix/net -> dev
- 通过 `NetTime` trait 解耦 wall clock 抽象网络层时间接口，路由表与接口配置分离
- socket fd 引用计数 + fdtable RCU 模式释放加固句柄生命周期校验
- Raw/ICMP socket recv 结果传输到用户态 `msghdr`
- 调用 smoltcp route 在 connect/bind 前校验 TCP/UDP 出站路由下一跳可达性
- 修复 DHCP Option 解析顺序和路由表先精确匹配再 LPM 的查询顺序

### feat/timer -> dev
- 通过 `DeviceProjection` trait 完成设备注册自动映射到 devtmpfs 节点的投影机制
- 重排设备抽象代码，`device_numbers` → `posix_compat` 责任分离
- 修复 SIGINT 递送后 tty 挂起未恢复输入，完善信号 handler 重置和终端回显恢复

## 2026-06-09
### feat/timer -> dev
设备与驱动:
- 基于 PnP 机制的静态驱动表实现内建驱动程序注册和设备自动匹配
- 将 `/dev/null`/`zero` 纳入 `dev::function::DeviceFunction` trait，PCI vendor/device ID 泛型类型安全
- 通过 `pci_host_bridge` 注册表扫描 PCI 配置空间枚举设备
- 通过 `of_msi_parse` 从设备树解析 `msi-parent`/`phandle` DT 属性关联 MSI 控制器
- 实现基于文件映射的 loop 块设备驱动，支持 `losetup` 模拟挂载
- 通过 `DevNodeSet` 树形节点将设备抽象接入 devtmpfs/sysfs/procfs

VFS 与进程:
- 将标准设备号表和 posix 兼容策略拆分到 `posix_compat.rs` 独立模块
- 实现 `sys_futex` FUTEX_WAIT/WAKE/REQUEUE 基于 waitqueue 的内核同步
- 重构 `do_clone` 分离 CLONE_VM/CLONE_VFORK/CLONE_THREAD 三种路径

其他:
- 通过静态驱动 + PnP 设备注册接入 `/dev/random` 和 `/dev/urandom`，适配字符设备接口
- loongarch64 用 `#[naked]` 函数嵌入 VDSO 汇编，避免 Rust prologue 污染 vsyscall 页布局

### feat/net -> dev
- 修复 ext4 extent 树拆分误将 leaf extent 降级导致映射失效和块覆盖写
- 修复 Ext2Inode lock 获取时不让出 CPU 导致优先级反转的死锁，加入 `schedule_yield` 重试

## 2026-06-08
### feat/net -> dev
- 调整黑名单启用 libcbench/libctest/lmbench，移除已修复测试的屏蔽
- 调整 smoltcp UDP socket 元数据队列深度与 yield 频率，修复缓冲区不足丢包
- 修复 `do_exit` 未清理 fdtable 中 socket fd 导致 `SO_REUSEADDR` 端口复用失败
- 修复 bind EADDRINUSE 时错误路径 panic 导致段错误的问题

### fix/alloc -> dev
- 修复 buddy 初始化时未释放 boot allocator 页面，仍使用其过时元数据的问题
- 通过 `RtcOps` trait 封装 ls7a MMIO 寄存器访问 CMOS 时间，细化 ls7a 控制器驱动

## 2026-06-07
### feat/net -> dev
网络协议:
- 实现 DHCP DISCOVER→OFFER→REQUEST→ACK 四次握手状态机，解析 Option 子网掩码/网关/DNS/租期
- 实现 `resolve_iface_for_remote` 最长前缀匹配路由选择和 TCP `pending_accepted` 限长 backlog 队列
- 实现 netlink RTM_NEWADDR/NEWROUTE 写操作，`parse_nlattr_ipv4` 解析属性操作内核路由表
- 修复 smoltcp socket 锁持有时调度让步不足导致的 CPU 空转和 UDP lookback 丢包

Socket:
- 实现 TCP_INFO 填充 state/RTT/MSS/cwnd，MSG_PEEK 调用 `tcp_peek`/`udp_peek_from` 零拷贝窥探
- 实现 SO_LINGER close 阻塞发送超时和 nonblock connect 返回 EINPROGRESS

VFS 与设备:
- 实现 sysfs ClassNet/ClassNetIface 目录节点和 16 属性槽位渲染 `/sys/class/net/`
- 实现 procfs 遍历 socket 快照渲染 `/proc/net/tcp`、`udp`、`unix`、`arp`、`sockstat`
- 通过 DevNodeSet+devtmpfs 树形节点和 blockfs 自动探测完善 VFS 设备挂载
- 修复 devtmpfs 挂载时序，确保 `pnp_bind_cb` 完成设备节点初始化前 `/dev` 不为空
- 接入 shm 引用计数 + vma 生命周期 hook 管理共享内存释放

### feat/testsuit -> dev
- 列出所有可用测试组
- 支持动态库设置

## 2026-06-06
### feat/net -> dev
- 基于 smoltcp Loopback 注册回环网卡，IP 127.0.0.1 收发不经过物理链路
- 通过 vvar page 映射实现 VDSO 框架，loongarch64 用 `naked_asm` 嵌入 `__vdso_clock_gettime` 避免 syscall
- 修复 `accept()` 从 `pending_accepted` 队列取错 socket 导致返回错误连接
- 修复 smoltcp `SocketHandle` 类型转移和 `Rc<RefCell<>>` 锁粒度过大导致的死锁
- 添加 CMOS RTC 驱动并通过 statfs 返回 `EXT4_SUPER_MAGIC` 使 busybox df 可识别块设备

### feat/testsuit -> dev
- 修复 rcS 黑名单 shell pattern 误匹配子串，改用前缀精确匹配
- 修复 pipe 读端在 buffer 空且写端关闭时未立即返回 0 导致的 EOF 自陷死锁
- 增强 rcS 引入 `SKIP_BLACKLIST` 环境变量，支持逐项跳过黑名单测试
- 将 iperf（网络吞吐）和 iozone（文件系统基准）加入测评黑名单
- 将内核链接脚本和设备树编译集成到 Cargo build.rs 中替代 Makefile

### bugfix -> dev
- 修复 execve 后 sigaction 继承未清空 caught 标志导致的 SIGSEGV，新增 `signal_reset_on_exec` 恢复 SIG_DFL

## 2026-06-05
### dev -> main
- 合并 dev（net + measure + testsuit）到 main：feat/net（virtio 网卡 + AF_INET/NETLINK + 网络协议栈）、feat/measure（构建系统 + lua + sched/arch 修复）、feat/testsuit（信号 restorer 校验 + 工具链升级）

### feat/net -> dev
网络:
- 添加 AF_INET 和 AF_NETLINK 协议分流：`libs/vfs/src/netlink_socket.rs` 实现 `AF_NETLINK` 协议族，`SocketOps` trait 分发 `NETLINK_ROUTE` 消息（`RTM_GETLINK`/`NEWADDR`/`NEWROUTE`/`DELLINK`/`DELADDR`/`DELROUTE`）；`libs/vfs/src/addr.rs` 实现 `sockaddr_in`/`sockaddr_in6` 与 `net::Endpoint` 的二进制互转
- 添加 virtio 网卡驱动：通过 PCI capability chain 定位 `common_cfg`/`notify`/`device_cfg` MMIO 区域，两个 virtqueue（RX queue 0 + TX queue 1）实现以太网帧收发；实现 `SIOCGIFINDEX`/`SIOCGIFNAME`/`SIOCGIFFLAGS`/`SIOCSIFADDR`/`SIOCSIFNETMASK`/`SIOCADDRT`/`SIOCDELRT` 等网络 ioctl

系统调用:
- 添加 `SYS_GETRLIMIT`：在 `libs/sched/src/rlimit.rs` 中按 `RLIMIT_NOFILE`/`RLIMIT_STACK`/`RLIMIT_DATA`/`RLIMIT_AS` 返回硬软限
- 添加 `SYS_SIGTIMEDWAIT`：在 `libs/sched/src/signal.rs` 中实现带超时的挂起信号等待队列
- 新增随机数支持：`general/src/dev/drivers/random.rs` 实现 virtio-rng 驱动和 Pollard 回退生成器；`arch/src/loongarch64/random_source.rs` 封装 LoongArch CSR 架构随机数寄存器（`CPUCFG` 检测 + `CSR_RNG` 读取）；`hal/src/random.rs` 抽象层暴露 `hal::random::fill_random()`

构建:
- 修改 smoltcp 来源并标记未实现功能：fork smoltcp 为 `libs/mygo-smoltcp` 本地 vendor 依赖，在未实现的函数体添加 `todo!()` 标记（IGMP/MDNS/IPv6 扩展头等），同时在 `Cargo.toml` 中关闭这些 feature gate
- 修复更换工具链后 arch 编译问题：补全 `naked_asm` 的 `options(nostack)` 约束，适配新工具链对 naked function 的严格检查

### feat/measure -> dev
- 修复 execve 后未激活新 VmSpace 导致子进程 SIGSEGV：根因为 `process_execve` 将新 `VmSpace` 写入 task ext 表后未调用 `vm.activate()` 切换 `PGDL` CSR，子进程返回用户态时仍使用父进程页表，返回地址为垃圾值跳转至 `0xfffffffe7ffffffc` 触发 PIF SIGSEGV；修复为 execve 末尾加 `vm.activate()`
- 修复 TrapFrame.euen=0 导致异常返回后内核态静默 HALT：根因为 `init_kernel_trap_frame`/`init_user_trap_frame` 将 `TrapFrame.euen` 初始化为 0，异常返回路径无条件从 TrapFrame 恢复 `CSR_EUEN`，覆盖 boot 阶段 `_start` 使能的 `EUEN_FPE|EUEN_SXE`；内核态执行 LSX 向量指令触发 `EECSSXD` → 机器错误 HALT；修复为 `tf.euen = EUEN_FPE | EUEN_SXE`
- 修复内核在 qemu 10 上崩溃的问题：LLVM release 模式将大于 16 字节的 `memset`/`memcpy` 优化为 LSX/ASX 向量指令，内核未使能 `EUEN.SXE` 导致 `SXD` 异常 → 静默 halt；`_start` 中显式 `csrwr CSR_EUEN` 置位 SXE 使能
- 修复 clone 写 child_tid 误用父进程 VmSpace 导致缺页死循环：`sys_clone` 在 `CLONE_CHILD_SETTID` 路径写入 `child_tid` 时仍处于父进程 `VmSpace`，访问子进程地址空间触发缺页 → 缺页处理尝试写同一 `child_tid` → 死循环；修复为写入前 `vm.activate()` 切换到子进程 VmSpace，写入后恢复
- 添加 initramfs 构建支持和 lua 解释器依赖，Makefile 集成 `scripts/build-lua.sh` 编译 Lua 5.4 至 rootfs
- 改为 vendor 离线构建：`cargo-config/config.toml` 通过 `[source.crates-io] replace-with = "vendored-sources"` 指向 vendor 目录，消除构建时网络依赖

### feat/testsuit -> dev
工具链与构建:
- 使用测评镜像中较新工具链：更新 `rust-toolchain.toml` 至评测环境 `nightly-2025-05-20`，确保 `llvm-target` 与 QEMU 兼容
- 压制 nightly 上 `named_asm_labels` 编译问题：`cargo-config/config.toml` 添加 `-Zallow-named-asm-labels` rustc flag
- 修正 `.gitignore` 导致编译失败和排除不完全的问题：修复 `vendor/*` 误匹配 vendor 子目录下的 `Cargo.lock`
- 加入 bench 门控规避缺少测试盘导致的编译失败：`kernel/Cargo.toml` 添加 `#[cfg(feature = "bench")]` 门控

测评适配:
- 修复 sa_restorer 未填充导致段错误：glibc 静态链接测试程序将 `sa_restorer` 作为 sigreturn 入口，内核 `sys_sigaction` 未保存 restorer 地址；修复为 `sa_restorer` 无效时退化为 `SIG_DFL` 处理
- 修复 rcS 测评脚本搜索位置：测试用例搜索路径修正为测评容器实际挂载位置 `/testbin`，替换旧硬编码路径

## 2026-06-04
### feat/net -> dev
- 重构设备抽象层：引入 `DeviceFunction` trait + `FunctionRegistry`（基于 `BTreeMap<PnpId, Vec<DeviceFunction>>`）；`DevNodeSpec` 枚举统一 char/block/net 三类设备节点；BIO 异步 I/O 结构（`BioOp::Read/Write`、`BioReqError`）；`Completion::wait()` + `notify()` 同步原语替代忙等轮询
- 添加网络设备抽象及协议栈：创建 `libs/net` crate（13 文件），`NetStack` 全局调度器采用 `RwLock`（读锁 I/O 路径 + 写锁 attach/detach）+ `per-interface Mutex` 双层锁提升多核并发；`NetDriver` trait 对齐 Linux `net_device_ops`：`poll_rx`/`alloc_tx`/`commit_tx` 收发分离；`NetAdapter` 实现 smoltcp `Device` trait 桥接驱动与协议栈
- 修复 devtmpfs 无法挂载的问题：`block_driver_init` 初始化顺序修正，确保 `DevTmpsFs` 在块设备注册前挂载，使 `/dev/vda` 等设备节点在 probe 完成后立即可见

## 2026-06-03
### feat/net -> dev
unix 套接字:
- 初步实现 unix 套接字及网络框架：创建 `libs/socket` crate（`connection.rs`/`io.rs`/`state.rs`/`types.rs`），支持 `SOCK_STREAM`/`SOCK_DGRAM`/`SOCK_SEQPACKET`；VFS 层 `FileOps` trait 通过 dentry 路径绑定（`socketpair` 和 `bind`+`listen` 两种模式）；`poll`/`epoll` 通过 `WaitQueue` 实现阻塞唤醒
- 添加读端关闭处理和句柄身份管理：`Handle` 引入权限位（readable/writable/read_closed），读端关闭后写端返回 `EPIPE`；处理 `EOF` 状态转换
- 完善 unix 套接字及 VFS 的单元测试：覆盖 datagram/stream/seqpacket 的 send/recv/close 路径及注册表生命周期

DTB:
- 修复 DTB 无法被完整解析的问题：`general/src/firmware/dtb.rs` 新增独立 DTB 解析器，校验 FDT 魔数 `0xd00dfeed`，按 `fdt_header` → `memory reservation block` → `structure block`（`FDT_BEGIN_NODE`/`PROPERTY`/`END_NODE`）→ `strings block` 递归遍历树节点

### bugfix -> dev
- 修复 busybox 安装错误：`scripts/build-busybox.sh` 修正 install 路径前缀和配置文件复制顺序
- 排除 userland 下的构建产物：新增 `userland/.gitignore` 忽略 `rootfs-la`/`rootfs-rv` 下的 ELF 二进制和 `.o` 文件

## 2026-06-02
### feat/procfs -> dev
- 初步实现 procfs 和 sysfs 文件系统：procfs（`general/src/vfs/procfs.rs`）通过静态 ino 编号方案（PID_BASE/SLOTS）暴露 `/proc/[pid]/status/maps/fd/cmdline`；sysfs（`general/src/vfs/sysfs.rs`）基于 `DevNodeSpec` 枚举和 `FunctionRegistry` 注册表生成 `/sys/block/devices/class/kernel` 目录树，固定 ino 布局避免冲突；底层依赖 VFS 的 `Superblock`/`InodeOps`/`FileOps`/`Dentry` trait
- 修复 extfs 顺序读写及 4K 块大小下读取过慢的问题：优化 extent 遍历路径，将逐块 BIO 改为批量 `BioBuilder::range()` 一次提交多块；修复块大小 ≠ 4K 时的 `i_block` 索引计算（`ext4_extent_header` 偏移对齐）
- 修复 loongarch64 汇编错误：`ertn` 前寄存器恢复顺序错误导致返回值错乱

### feat/power -> dev
- 新增 `sys_reboot` 系统调用：校验四个 magic 值（`LINUX_REBOOT_MAGIC1/MAGIC2/MAGIC2A/MAGIC2B/MAGIC2C`）后分发 `LINUX_REBOOT_CMD_RESTART`/`CMD_HALT`/`CMD_POWER_OFF`；HALT 路径调用 `platform::halt()` 发送 QEMU 的 shutdown ACPI 事件

### bugfix -> dev
extfs:
- 修复 extfs 无法多块读取的问题：读路径中 `read_at` 未正确循环处理 `extent_wr::read_extents` 返回的多个 extent 块，改为 `while remaining > 0` 逐 extent 读取
- 修复 extfs 批量读取超过 virtio 上限的问题：`BioBuilder` 构建时将单次 BIO 请求的 `block_range()` 限制在 `virtio_blk` queue size 以内，超出时分片提交

内存与进程:
- 调整内核栈大小至 64 KiB 防止栈溢出：`DEFAULT_KERNEL_STACK_SIZE` 改为 `64 * 1024`，`KernelStack::new()` 通过 `Layout::from_size_align` 分配
- clone 时 CLONE_VM 场景跳过冗余页表切换：`CLONE_VM` 标志下子进程共享父进程 `VmSpace`，跳过 `vm.activate()` 写入 `PGDL` CSR 的页表切换操作

其他:
- 为 `LINUX_REBOOT_CMD_HALT` 添加 shutdown 调用：HALT 路径追加 `shutdown()` 函数调用，通过 ACPI `_S5` 或 QEMU `isa-debug-exit` 通知 QEMU 退出

## 2026-06-01
### feat/test -> dev
测试框架:
- 实现主机端与内核端统一测试框架及 ktest 内核测试器：创建 `libs/ktest` crate 和 `ktest-macro` proc-macro，通过 `#[ktest]` 属性宏统一标记测试函数；`runner.rs` 在 `cfg(kernel)` 下链接进内核，主机端通过 `cargo test` 驱动
- 修复 proc-macro 编译时 `cfg!(feature = "kernel")` 求值问题

单元测试:
- 添加内存分配器集成测试：基于 `KERNEL_ALLOCATOR` 全局分配器，测试 buddy/slab 的分配、释放、realloc 路径
- 添加 VFS、调度器、内存管理等模块单元测试：libs/vfs 测试 dentry/path 操作；libs/sched 测试任务队列与调度实体；libs/mm 测试 VmSpace 映射与页表操作
- 构建 FatFs 与 ExtFs 的 mock 测试及 Memdisk 适配：`MemDisk` 结构体实现 `BlockDevice` trait 模拟块设备；覆盖 extfs CRC/Inode/Superblock 和 fatfs BPB/FAT/dir entry 解析
- 为日志、ELF 解析、错误码等基础模块添加单元测试：libs/log 环形缓冲区与时间戳；libs/elf 解析魔数与程序头；libs/errno 错误码转换

### fix/loongarch64 -> dev
- 修复 FPU 懒启用导致 EUEN/FPU 寄存器损坏：根因为 context_switch 时，若旧进程使能了 FPU 但新进程的 `TrapFrame.euen = 0`，异常返回路径的 `csrwr CSR_EUEN` 会清除已使能的 `EUEN_FPE`；修复为在切换路径中根据旧进程 `CSR_EUEN` 正确保存 `fpu_state`，恢复时按需写回

## 2026-05-26
### bugfix -> main
- 修复 fatfs 和 extfs 读写过慢的问题：extfs 优化 extent_wr 间接块缓冲和 alloc_mod 块分配路径，引入 `MAP_CACHE_MAX_BLOCKS` 缓存；fatfs 重构 `FatTable` 为 LRU 扇区缓存 + dirty 位回写 + `with_slots_lock` 消除 FAT12 跨扇区 TOCTOU 窗口；块设备层引入 BIO 异步提交框架（`Completion`/`queue_bio`/`submit_bio_wait`），virtio_pci 利用 `Completion::wait()` 消除忙等待
