# 架构设计文档

## 1. 当前架构总览

本文描述当前代码和构建系统已经落实的依赖关系，不把尚未完成的分层目标写成既成事实。`Cargo.toml` 是编译期依赖的权威来源；若本文与清单不一致，应先修正文档或代码，再合入变更。

当前工程由六类单元组成：

1. `kernel` 负责引导后的全局编排、系统调用、进程入口、文件系统挂载和 ELM 运行时接入。
2. `arch` 保存 LoongArch64 与 RISC-V 64 的引导、异常、中断、页表和指令级机制。
3. `hal` 对常用架构能力提供统一入口，但当前不是 `kernel` 到 `arch` 的唯一通道。
4. `general` 保存架构无关的共享内核基础设施，其中设备部分只保留发现、PnP、资源、字符设备、块设备、开放设备功能和用户态投影等抽象。
5. `libs` 保存可独立复用的内核子系统与数据结构，例如 allocator、VFS、调度、AF_UNIX、IP 网络、ELM、ELF 和文件系统实现。
6. `drivers` 保存具体硬件驱动。每个驱动都是独立 ELM 工程，由统一模块清单决定集成、受管装载或禁用。

协议执行状态机位于 `drivers/net-stack` 的 `net.stack` ELM；`libs/net` 提供 host 与 ELM
共享的缓冲区、队列、flow shard、单写者执行和协议契约，kernel 负责设备队列与 worker
调度。INET 套接字数据路径由 `libs/vfs` 接入；AF_UNIX 本地 IPC 继续由 `libs/socket` 和
`libs/vfs` 提供。loopback 位于 `drivers/loopback`，VirtIO block/net 位于
`drivers/virtio-blk` 和 `drivers/virtio-net`，均通过 `drivers/Modules.toml` 选择集成方式。

## 2. 编译期层级

```text
                              ┌──────────────┐
                              │    kernel    │
                              └──────┬───────┘
                      ┌──────────────┼──────────────┐
                      ▼              ▼              ▼
                 ┌────────┐     ┌─────────┐    ┌────────┐
                 │  arch  │◄────│   hal   │    │general │
                 └───┬────┘     └────┬────┘    └───┬────┘
                     │               │             │
                     └───────────────┴──────┬──────┘
                                            ▼
                                      ┌──────────┐
                                      │   libs   │
                                      └──────────┘

drivers/ELM --编译时使用 Kernel API Profile--> kernel 导出的 Rust 接口
drivers/ELM --配置为 y 时以集成归档链接-------> kernel
drivers/ELM --配置为 m 时输出 EKI------------> build/<arch>/modules
```

箭头表示编译期或链接期依赖。`kernel -> arch` 是当前真实关系：引导上下文、异常恢复、VDSO 和部分架构专属集成仍由 `kernel` 直接使用。新的通用调用应优先进入 `hal`，但在代码完成迁移前，不得把“禁止直接依赖”写成已落实规则。

### 2.1 `kernel`

`kernel` 是最终镜像的集成层，职责包括：

- 接收架构加载器传入的启动上下文；
- 初始化内存、调度、VFS、系统调用和 ELM；
- 把 DTB、ACPI、PCI 等发现结果交给通用设备模型；
- 装入配置为 `y` 的集成 ELM，登记配置为 `m` 的 BuildBound 元数据；
- 选择裸内核或显式指定的 initramfs。

`kernel` 可以依赖 `arch`、`hal`、`general` 和所需 `libs`。它不得反向成为这些 crate 的依赖，也不得重新容纳已经迁出的具体设备驱动。

### 2.2 `arch`

`arch` 只保存 ISA 或平台机制，包括启动汇编、寄存器访问、异常入口、页表切换、中断控制、用户态上下文和 VDSO。它依赖 `general` 中的通用契约以及必要的 `libs`，不依赖 `kernel`、`hal` 或具体驱动 ELM。

ISA 专属汇编和 CSR/寄存器操作必须留在 `arch`。通用状态机、设备策略和文件系统逻辑不得通过目标架构条件编译塞入 `general`。

### 2.3 `hal`

`hal` 依赖 `arch` 和 `general`，为时间、中断、用户地址访问、CPU 控制等常用机制提供统一入口。它用于减少上层的 ISA 分支，但当前不承担完整的依赖隔离职责。

新增跨架构能力时，应先判断该能力是否能够形成稳定的通用语义。能够统一的进入 `hal`；只能由启动或异常路径表达的机制可以暂时由 `kernel` 直接调用 `arch`，并在代码中保持边界明确。

### 2.4 `general`

`general` 提供共享内核基础设施和设备抽象，不包含具体硬件型号驱动。设备相关职责包括：

- 固件与总线发现结果的通用表示；
- PnP 设备、驱动匹配、资源归属和移除状态机；
- IRQ、DMA、MMIO、PCI、platform 等驱动所需的通用机制与契约；
- 字符设备和块设备两条基础 I/O 轨道；
- `DeviceFunction` 开放类别、动态类别注册和设备功能生命周期；
- devtmpfs、sysfs、procfs 等用户态投影视图。

`general` 可以依赖 `libs`，但不得依赖 `arch`、`hal`、`kernel` 或 `drivers`。具体 UART、RTC、IRQ 控制器、固件总线、flash、random 和 VirtIO 实现均位于 `drivers`。

### 2.5 `libs`

`libs` 中的 crate 按实际子系统关系互相依赖，但不得依赖 `kernel`。其中：

- `allocator`、`sched`、`vfs`、`socket`、`net` 等提供可直接由内核和 ELM 使用的 Rust API；
- `kernel-symbols` 为审核后的 API 生成稳定导出描述符和 Mixin 站点；
- `elm` 与 `elm-loader` 定义运行时模型、EBI 和装载协议；
- `socket` 提供 AF_UNIX；`net` 提供网络设备契约、packet/flow 类型、队列和单写者执行原语；
- `drivers/net-stack` 实现 Ethernet、ARP、IPv4/IPv6、ICMP、TCP 与 UDP 的协议 shard turn，
  通过直接固定端点与 kernel host 协作。

跨 crate 依赖必须保持无环。全局能力需要后端时，优先使用明确的注册接口、trait 或函数指针，不允许通过依赖 `kernel` 获取实现。

### 2.6 `drivers`

`drivers/Modules.toml` 是驱动集合的配置与依赖图。每个条目声明模块名、工程路径、配置键、适用目标和 ELM 依赖：

- `y`：编译为集成归档并链接进内核镜像；按集成组件 initcall 初始化，不创建 ELM cell，
  也不具有 generation、动态暂停或热替换语义。
- `m`：编译为受管 EKI，写入 `build/<arch>/modules`，由 `elm-mgr` 在运行时校验和装载。
- `n`：不构建，也不进入最终依赖图。

默认部署策略只把基础且通用的固件总线、UART、随机服务、协议栈与回环设备设为 `y`；
架构或板级中断控制器以及其它可选硬件驱动设为 `m`。这里的选择表达部署边界，不改变
`general` 中的 PnP、资源所有权或 `DeviceFunction` 抽象。

模块之间的功能依赖必须同时写入模块集合清单和 ELM 清单，不能用隐藏的 Cargo path
dependency 代替。例如 `virtio.block` 依赖 `virtio.framework`，构建工具不仅会在依赖被
禁用时拒绝配置，也要求硬依赖两端具有相同的 `y` 或 `m` 模式。

驱动源码通过 Kernel API Profile 使用与内核相同路径的 Rust API。配置为 `y` 和 `m` 的源码保持一致，差异只存在于构建、链接、装载和生命周期管理阶段。

## 3. 依赖规则

| 来源 | 允许依赖 | 禁止依赖 |
|---|---|---|
| `kernel` | `arch`、`hal`、`general`、`libs`、配置为 `y` 的集成归档 | 被 `arch`、`general`、`libs` 或驱动反向依赖 |
| `hal` | `arch`、`general`、必要 `libs` | `kernel`、`drivers` |
| `arch` | `general`、必要 `libs` | `kernel`、`hal`、`drivers` |
| `general` | `libs` | `arch`、`hal`、`kernel`、`drivers` |
| `libs` | 其他无环 `libs` crate | `general`、`hal`、`arch`、`kernel`、`drivers` |
| `drivers` | ELM 框架、Kernel API Profile、显式 ELM 依赖 | 内核源码 path dependency、未声明的其他驱动 |

所有层都必须遵守以下硬约束：

1. 禁止循环依赖。
2. `general` 中不得使用架构条件编译承载通用功能分叉。
3. 具体硬件匹配表、寄存器布局和探测逻辑不得回流到 `general`。
4. 网络设备实现必须位于 `drivers` 并通过 ELM 清单管理，不得作为 `general` 或 `libs/net` 的隐藏具体驱动。
5. 动态 ELM 只能解析 Kernel API Profile 中审核过的符号；存在源码或 metadata 不等于允许链接。

## 4. ELM 与内核接口

Kernel API Profile（内核 API 配置）同时包含目标专属 Rust metadata、源码投影、导入库、支持归档和符号清单。各部分职责不同：

- metadata 负责类型检查和 Rust 单态化所需的编译信息；
- 源码投影负责 LSP 跳转、诊断和补全；
- 符号清单规定动态 ELM 真正允许解析的内核入口；
- 导入库把稳定 API 名称映射到当前内核实现；
- Profile 哈希绑定上述内容，防止模块把相似但不一致的接口误认为兼容。

一个 crate 出现在 metadata 列表中，并不意味着它的全部公有函数自动成为 ELM API。可调用入口必须使用 `kernel_symbols::export` 明确登记，并声明契约、能力、状态修改、所有权返回和长期保留参数等属性。

具体驱动不通过通用 ELM 消息调用 MMIO、DMA、IRQ 或 VFS 热路径。装载器完成权限和符号解析后，驱动调用的是直接 Rust API；ELM 抽象层负责镜像、依赖、生命周期、资源、策略、故障隔离和热替换，而不是充当第二套内核 API。

## 5. 构建与产物

默认通过 Cargo 构建指定架构的裸内核，不打包 initramfs：

```text
 cargo xtask build --target loongarch64-unknown-none
 cargo xtask build --target riscv64gc-unknown-none-elf
```

模块配置和检查入口：

- `cargo xtask config`：交互式选择驱动和 ELM 模式；
- `cargo xtask oldconfig`：保留现有选择并补齐新增项；
- `cargo xtask defconfig`：恢复默认配置；
- `cargo xtask modules --target <triple>`：构建用于接口导出的 kernel、导出该目标的
  Kernel API Profile，再构建当前配置选择的模块集合；
- `cargo xtask build --target <triple>`：消费模块清单与集成归档完成最终 kernel 链接；
  对应清单不存在时先补跑 `modules`；
- `cargo xtask build --target <triple> --initramfs <cpio>`：在调用方提供镜像时启用
  `embedded-initramfs` 并嵌入该 CPIO。

`modules` 与 `build` 默认分别复用 `target/<arch>` 和 `build/<arch>/modules`。切换目标、
配置或接口后应显式重新运行 `modules`，不要依赖旧的 `modules.manifest` 自动失效。

initramfs 生成、用户态 rootfs 和镜像装配属于独立工程，不是内核构建的隐式步骤。

## 6. 不安全代码与并发

所有 `unsafe` 块必须按 `STYLES.md` 给出 `// Safety:` 说明。职责边界如下：

- `arch` 可以使用汇编、寄存器和上下文切换所需的不安全操作；
- `general` 和 `libs` 只能在已验证的底层原语、设备资源、裸指针或 FFI 边界使用不安全操作；
- 驱动访问 MMIO、DMA 和中断资源时必须遵守资源所有权与失效顺序；
- `kernel` 只在无法下沉的集成边界使用不安全操作，不把策略代码建立在未经验证的裸指针上。

SMP 下的共享状态必须使用原子、per-CPU 数据或明确锁保护。设备移除遵循“停止发现新对象、标记失效、唤醒等待者、排空 I/O、注销投影、释放资源”的顺序。ELM 卸载还必须等待直接调用、租约和保留模块代码的对象全部收束。
