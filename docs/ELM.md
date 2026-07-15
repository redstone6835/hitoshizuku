# ELM 最终设计目标：可拓展内核单元系统

## 1. 定位

ELM，全称 **Extensible Loadable Module**，中文统一称为 **可拓展内核单元**。

ELM 不是 Linux 模块 ABI 的兼容层，也不是传统动态内核模块机制。ELM 的目标是在当前内核中提供一种面向 Rust 的运行时拓展体系：内核提供可信执行底座、资源所有权、状态机、能力边界和可观测拓扑；`elm-mgr` 作为 ELM 外界接口核心和运行时管理器，负责把策略、菜单、依赖选择、事件订阅、运行时 API 和外部管理入口统一收口。外部 ELM 使用目标内核发布的精确 `allocator`、`general` Rust metadata，并按与内核源码相同的路径编写普通 Rust 代码；编译器产生的真实函数、固有方法和静态对象引用在打包期映射到由 `#[kernel_symbols::export]` 登记的稳定目录项。名称、契约、版本、规范 Rust ABI、接口源码摘要、能力策略和镜像信任全部通过后，调用就是普通 Rust 调用，不经过 `elm-mgr`、provider、函数表、grant 或 token。只有需要运行时发现、绑定、审计、异步流控或跨单元服务语义的能力才通过 provider 端口进入枢纽连接层。

ELM 的基本原则：

- 不兼容 Linux `init_module`、`finit_module`、`delete_module` ABI。
- 不采用 `ko`、`modprobe`、`export_symbol`、GPL namespace 等传统模型。
- 不把 ELM 简化为“装入一段代码并调用 init/exit”。
- 不允许 ELM 猜测或直接链接未登记的内核 trait、裸指针和私有符号。
- ELM 面向 Rust 框架开发；C/C++ 兼容和许可证策略暂不进入目标。
- 开机时内核创建根管理单元 `elm-mgr`，它是永久内建 ELM，元数据来源显示为 `<builtin>`，不使用 EKI、soyo、ELF 或其他镜像自举。
- 开机时内核同时创建 `elm-mgr` 的内建子单元 `eki`，由它提供 EKI 到 EBI 的本地投影能力。
- 后续所有 ELM 都是 `elm-mgr` 管理树下的子单元。
- 每个动态 ELM 都必须实现统一的 `ElmModule` trait，并通过 `#[elm::module]` 注册；`create`、`initialize` 和 `finalize` 是必需方法。
- 一个 ELM 可以拓展另一个 ELM，也可以被另一个 ELM 拓展。
- ELM 之间可以同时存在父子、依赖、提供、拓展和能力绑定关系。
- ELM 支持热插拔、热替换、暂停、恢复、故障隔离和回滚。

当前设计走向是：ELM 不再被视为“内核模块加载器”，而是收敛为 **可管理执行单元运行时**。跨单元服务默认通过带版本的流契约连接到枢纽端口；对延迟敏感且 ABI 可以严格固定的调用可以使用 `direct-pinned` 导入导出。资源访问通过租约完成，生命周期变更通过预检和提交完成，运行时状态通过快照、事件和审计可观测。

## 2. 统一中文术语

| 英文名 | 中文名 | 含义 |
| --- | --- | --- |
| ELM Cell | 内核单元 | 一个可管理、可绑定、可拓展的 ELM 实例 |
| elm-mgr | 单元管理器 | 启动期根管理单元，负责策略和用户可见管理 |
| elm-mgr Runtime API | 单元运行时接口 | `elm-mgr` 对外公开管理 API、事件 API、策略 API 和运行时服务 API 的统一入口 |
| Nexus | 枢纽连接层 | ELM 之间和内核能力之间的运行时连接网络 |
| Nexus Port | 枢纽端口 | 枢纽连接层中可绑定的能力入口或出口 |
| Port Provider | 端口提供者 | 向枢纽连接层注册端口并执行端口语义的内核或 ELM 实体 |
| Flow Contract | 流契约 | 描述一次能力流的输入、输出、错误、并发和背压语义 |
| Intent | 能力意图 | 内核单元声明自己想消费、提供、拓展或观察的能力 |
| Offer | 能力提供 | 内核单元声明自己能提供的能力 |
| Binding | 能力绑定 | 一个能力意图与一个枢纽端口或能力提供之间的实际连接 |
| Binding Graph | 绑定图 | 父子、依赖、提供、拓展和能力绑定组成的运行图 |
| Extension Point | 拓展点 | 内核单元显式开放的可拓展位置 |
| Extension | 拓展项 | 挂接到某个拓展点的能力实现 |
| Lease | 资源租约 | ELM 对内核资源的受控引用 |
| Capability | 能力权限 | 内核授予内核单元可使用的权限集合 |
| Reactor | 事件反应器 | 处理流事件、管理事件和热插拔事件的 ELM 入口 |
| Generation | 切换代 | 热替换时区分新旧绑定和租约的版本号 |
| Quiesce | 静默化 | 停止新流进入并等待活跃流排空 |
| Detach | 脱离 | 从绑定图、拓展点和资源表中摘除实例 |
| Retire | 退役 | 完成资源回收后的最终移除 |
| Faulted | 故障态 | 单元运行时发生不可继续的错误 |
| Quarantined | 隔离态 | 故障单元被禁止接收普通流，只允许诊断和退役 |
| Topology | 运行拓扑 | 当前所有单元、关系、端口、绑定和租约的可观测快照 |
| Manifest | 单元清单 | ELM 的自描述元数据 |
| EBI | ELM 二进制装载接口 | ELM Core 消费的稳定装载协议对象，不是文件格式 |
| EBI Source | EBI 来源 | 已在本地解析完成并能产出 EBI 协议对象的镜像、投影、内建对象或测试对象 |
| EKI | ELM 内核镜像 | ELM 原生镜像格式，天然贴合 EBI；当前由内建子单元 `eki` 提供装载投影能力 |
| soyo | soyo 可拓展文件类型 | 未来通用文件类型，本身不等于 EBI，但具备通过 profile 或对象实现出 EBI 的能力 |

## 3. 总体架构

```text
内核 ELM Core
    |
    +-- elm-mgr：根管理单元
          |
          +-- eki：内建 EKI 投影单元
          +-- 普通内核单元
          +-- 拓展单元
          +-- 服务单元
          +-- 驱动单元
          +-- 子管理单元
```

内核 ELM Core 负责硬约束：

- 创建启动期内建 `elm-mgr` 和 `eki` 拓扑。
- 维护真实状态机、运行拓扑、资源租约、事件序列、审计环和绑定图。
- 校验单元清单、目标架构、ABI 版本、能力权限、依赖合法性和端口契约。
- 执行静默化、脱离、退役、故障隔离等安全流程。
- 提供私有系统调用 `sys_elm_ctl`。
- 为 `elm-mgr` 提供可信执行底座和可验证提交路径；安装并校验常驻内核符号目录，在执行模块代码前完成直接符号解析、能力裁决和地址重定位。
- 提供稳定的 EBI Source 输入 ABI，并把具体镜像格式隔离在 Projection Source 之后。

`elm-mgr` 负责策略：

- 管理所有后续 ELM。
- 作为所有 ELM 通向运行时管理能力的统一通道。
- 维护模组菜单和用户可见管理界面。
- 决定加载、卸载、启用、禁用、替换和配置策略。
- 编排普通 ELM 之间的依赖、拓展和能力绑定。
- 处理外部工具通过 `sys_elm_ctl` 发送的管理请求。
- 公开 `elm::runtime::*` 普通运行时接口和受授权 Manager 专用的 `elm::management::*` 管理接口；事件订阅、生命周期管理、provider 绑定等管理能力只通过 `elm::management::Client` 进入，普通日志、上下文和终止操作只通过 `elm::runtime` 进入。
- 通过统一的 provider Ops 接纳需要运行时发现、绑定、审计、异步队列或跨单元调用的能力；VFS、设备、调度、网络、IRQ、DMA、MMIO 等子系统的稳定 Rust API 仍可以按工程依赖顺序直接暴露给 ELM。
- 将策略结果转化为内核可验证的预检和提交操作。

普通 ELM 负责能力：

- 声明能力意图、能力提供、依赖和拓展项。
- 通过枢纽端口参与能力流。
- 通过资源租约访问内核资源。
- 遵守状态机、热插拔和故障隔离规则。
- 不持有裸内核对象指针，不直接调用未声明、不稳定或绕过策略快照的内核内部接口。

## 4. 核心运行模型

ELM 的运行模型由五个对象构成：

- 内核单元：运行时可管理的最小实体，具有 `ElmId`、名称、类型、状态、切换代和 EBI 装载状态。
- 枢纽端口：枢纽连接层中的稳定连接点，具有 `PortId`、流契约、方向、模式和实现状态。
- 能力绑定：把一个内核单元连接到一个枢纽端口，具有 `BindingId`、契约、切换代、活动状态和可选租约。
- 资源租约：把绑定后的资源引用收敛到可撤销对象，具有 `LeaseId`、所有者、类型、权限、状态、活跃引用数和切换代。
- 事件记录：把单元、端口、绑定和租约变化变成固定布局事件，供管理工具和运行时端口读取。

典型控制路径：

```text
外部工具
    -> sys_elm_ctl(MGR_CALL)
        -> elm-mgr 管理通道
            -> ELM Core 预检
                -> ELM Core 提交
                    -> 绑定图、租约表、事件环、审计环更新
```

典型运行路径：

```text
内核单元
    -> 声明能力意图
        -> elm-mgr 选择端口
            -> ELM Core 创建能力绑定
                -> ELM Core 创建资源租约
                    -> 端口提供者执行真实语义
```

当前已经形成闭环的运行路径：

- `core.log@1`：单元通过运行时绑定提交固定长度日志，内核写入日志系统，并累计提交次数。
- `core.event@1`：单元通过运行时绑定按游标读取 ELM 事件，确认事件游标，并统计投递和丢弃事件数。
- `mgr.menu.item@1`：单元通过菜单绑定向 `elm-mgr` 注册菜单项，菜单项与绑定、租约处于同一撤销链路。
- `mgr.action.invoke@1`：`elm-mgr` 通过 provider 调用帧执行内建管理动作，当前已支持 Core 健康检查动作。
- 动态端口提供者：内核单元可以注册带访问策略的 provider 端口，端口会进入快照、审计和统计路径。
- 同步调用帧：ABI 已稳定为 `ElmCallFrame` / `ElmReplyFrame`；已接入 kernel-backed 管理动作 provider，ELM 原生 provider 可通过 EBI provider port 的 `handler_symbol` 接入原生 handler。
- 异步 provider 队列：`elm-mgr` 可以提交 provider 调用、轮询结果、取消排队任务、对运行中调用提交取消意图并查询队列统计；队列会持有 provider 租约直到结果被领取、TTL 过期或结果环淘汰。
- `elm.mgr.api.registry@1`：`elm-mgr` 公开 API 注册表，描述当前可用的管理 API、事件 API、provider API 和未来子系统 provider API。
- `elm.mgr.event.*@1`：`elm-mgr` 提供事件订阅、订阅查询、订阅读取和退订能力，每个订阅都有独立租约和游标。

第一版同步调用帧固定内联载荷为 256 字节。调用帧只表达 `binding_id`、`call_id`、`opcode`、`flags` 和 payload，不携带指针，也不绑定文件格式。`mgr.action.invoke@1` 使用 `ElmActionInvokeRequest` 和 `ElmActionInvokeReply` 作为 payload ABI；带原生 handler 的动态 provider 会通过 `ElmNativeProviderCallV1` 进入 ELM 原生执行器；未声明 handler 的动态 provider 保持不可调用，不再作为运行时 TODO 后端暴露给调用路径。

异步 provider 队列复用同一个调用帧，不创造第二套 provider ABI。`SubmitProviderCall` 把 `ElmCallFrame` 包进 `ElmProviderAsyncSubmitRequest`，只额外描述超时、结果保留 TTL 和保留 flags。同步 `InvokeProvider` 仍保留，用于低延迟管理动作和兼容现有外部工具；异步路径用于需要背压、取消、超时和结果保留的 provider 调用。

单元级资源预算已经进入运行时模型。启动期 `elm-mgr` 使用 Root 预算；动态装载请求显式给出父单元和初始预算，省略部署侧定制时由构造器使用默认预算。预算是分层委派的：父单元的总预算必须同时覆盖父单元当前用量和所有仍存活直接子单元的保留预算，子单元不能自行扩大预算，父单元也不能把所有子预算之和扩大到自身总预算之外。退役子单元会释放其保留预算；缩减某个单元预算时，还必须覆盖该单元当前用量和仍存活子单元。provider 端口数量、异步 provider 队列占用、事件订阅、pending load、原生镜像、native fault 和审计记录都会进入 `ElmResourceUsage`。超过预算的操作统一返回 `RESOURCE_QUOTA` blocker，并按管理通道策略映射为 `BUSY`，避免单个 ELM 或整棵子树耗尽管理面资源。

原生故障隔离当前已经完成软边界、protected-call 保护域和架构级同步 fault 恢复边界：生命周期 hook 失败、原生 provider fault、异步原生 provider 超时等会记录 native fault，把目标单元标记为 isolated，并阻断后续 provider 注册、provider 调用、绑定预检和 native import 解析。所有 hook、迁移入口、entry、provider handler 和 provider snapshot 都统一经过双架构原生调用门；调用门在进入 ELM 前保存内核整数 ABI 边界帧，并把固定恢复 PC 与边界 SP 登记到当前 guard。同步 kernel fault 命中 active ELM guard 后，trap 会消费该恢复上下文，同时重写 trap frame 的 PC、SP 和返回值，再由固定退出路径恢复固定寄存器与 callee-saved 寄存器。恢复过程不读取故障现场 `ra`，因此 fault 位于 ELM 深层嵌套函数时也不会跳回镜像内部。`panic!` 会进入专用 panic 恢复出口；原生调用期间临时开放中断，timer trap 在超过执行期限时强制重定向到受控退出路径，因此无限循环不再只能依赖软超时。fault dump 同时公开 fault PC、地址、异常码、恢复 PC 和恢复 SP。当前原生 ABI 指纹拒绝 float、vector 和 SIMD target feature，避免允许镜像引入尚未纳入调用门保存范围的扩展寄存器状态；内核 provider 回调不属于原生 ELM 调用门，仍由其所属子系统承担故障边界。

## 5. 枢纽连接层

ELM 不允许扫描未登记符号或按地址猜测内核布局。运行时交互分为三条边界：ELM 自有能力使用 `elm::runtime`/`elm::management`；需要发现、绑定和审计的服务使用枢纽连接层；`direct-pinned` 的 ELM 间导入导出和 `kernel-symbol` 内核导入使用类型化固定槽。`direct-pinned` 会固定目标 ELM generation；`kernel-symbol` 只解析常驻目录中的真实内核实现，不参与 ELM export 选择。二者都在执行模块代码前完成完整 Rust ABI 校验，调用时不再经过 provider 调用帧。

```text
内核单元
    声明能力意图
        绑定枢纽端口
            获得资源租约
                参与流契约
                    由端口提供者或事件反应器处理事件
```

枢纽端口代表一种可组合能力。它可以表示设备事件、文件系统操作、网络包流、IRQ 事件、菜单项注册、配置变更或诊断输出。枢纽端口是需要发现、授权、审计、代际路由或异步流控时的稳定边界；它不强制承载所有本地同步调用。

流契约格式：

- 使用 `name@version` 形式。
- 名称只允许小写字母、数字、`.`、`-` 和 `_`。
- 版本只允许数字和 `.`。
- 契约字符串是兼容性边界，不能用 Rust 类型名或内核内部符号替代。

流方向：

- `Source`：端口产生事件或数据。
- `Sink`：端口消费事件或数据。
- `Duplex`：端口双向交互。
- `Control`：端口提供控制面请求响应语义。

流模式：

- `Exclusive`：同一时刻只允许一个有效消费者或提供者。
- `Shared`：允许多个绑定共享。
- `Ordered`：要求按绑定或提交顺序处理。
- `Pipeline`：面向流式数据处理。
- `Broadcast`：向多个绑定广播事件。

并发与背压模型已经在模型层预留：

- 并发：`Single`、`Parallel`、`Reentrant`。
- 背压：`Drop`、`Queue`、`Stall`、`Reject`。

当前实现先把方向和模式纳入端口描述，后续端口执行器必须把并发和背压纳入真实调度策略。

## 6. 内建枢纽端口

| 端口 | 方向 | 模式 | 当前状态 | 设计语义 |
| --- | --- | --- | --- | --- |
| `core.log@1` | Sink | Shared | 已实现 | 单元向内核日志提交固定长度运行时日志 |
| `core.event@1` | Source | Broadcast | 已实现 | 单元按游标读取 ELM 拓扑和管理事件 |
| `mgr.menu.item@1` | Sink | Ordered | 已实现 | 单元向 `elm-mgr` 注册菜单项 |
| `mgr.action.invoke@1` | Control | Shared | 已实现第一版 | `elm-mgr` 内建管理动作调用入口，当前支持健康检查动作 |
这些端口是 ELM 自有端口，不包含 VFS、设备、网络等子系统语义。后续完整 ELM 设计中，端口提供者也可以来自 ELM 本身：一个 ELM 可以声明新的能力提供，经过 `elm-mgr` 策略和 ELM Core 校验后，把新端口注册进枢纽连接层。这样设备类型、VFS 扩展点、网络处理链、诊断能力和子管理能力都不需要写死在核心中。

### 子系统自注册 provider 规格

子系统接口不再写入 `libs/elm::ports`、`kernel/src/elm/core.rs` 或 ELM 启动路径。每个子系统可以在自己的 crate 内导出 `providers()` 候选规格表，但这些规格表不能由 `elm-mgr` 启动期硬编码汇聚；必须由子系统自身初始化完成后显式注册，或由未来真实 ELM provider 接管。ELM Core 只消费通用 `ElmKernelProviderSpec`，不主动依赖 VFS、设备、网络、IRQ、DMA、MMIO 等子系统。

| 子系统位置 | 端口 | 方向 | 模式 | 当前状态 | 设计语义 |
| --- | --- | --- | --- | --- | --- |
| `general::dev::elm` | `device.discovered@1` | Source | Broadcast | 已实现 snapshot/query | 设备发现只读快照与查询 |
| `general::dev::elm` | `device.claim@1` | Control | Exclusive | 已实现 acquire/release/query/snapshot | 设备声明、释放、查询和绑定撤销清理 |
| `general::dev::elm` | `irq.event@1` | Source | Shared | TODO(provider) | IRQ 事件分发 |
| `general::dev::elm` | `dma.buffer@1` | Duplex | Shared | TODO(provider) | DMA 缓冲区申请、映射、同步和释放 |
| `general::dev::elm` | `mmio.window@1` | Duplex | Shared | TODO(provider) | MMIO 窗口映射和访问租约 |
| `general::dev::elm` | `io.block.submit@1` | Sink | Shared | TODO(provider) | 块 I/O 请求提交 |
| `net::elm` | `io.packet.rx@1` | Source | Pipeline | TODO(provider) | 网络包接收流 |
| `net::elm` | `io.packet.tx@1` | Sink | Pipeline | TODO(provider) | 网络包发送流 |
| `vfs::elm` | `vfs.lookup@1` | Control | Shared | 已实现 query | VFS 路径查找控制面 |
| `vfs::elm` | `vfs.read@1` | Control | Shared | TODO(provider) | VFS 读控制面 |
| `vfs::elm` | `vfs.write@1` | Control | Shared | TODO(provider) | VFS 写控制面 |

显式注册后的 provider specs 会进入 API 注册表、provider 端口快照、sysfs、绑定预检、同步/异步调用、统计、审计和撤销路径。`device.discovered@1`、`device.claim@1` 和 `vfs.lookup@1` 当前只在测试或子系统显式注册后可用，不再属于启动期内建能力。其余尚未接入真实语义的子系统回调返回 `UNSUPPORTED`，用于稳定完整链路。真实协议必须在对应子系统的 `elm.rs` 内补齐，不能回到 ELM Core 写特殊分支。

### `general::dev` 直接符号接口

设备能力不再压缩成单一版本化函数表，也不再维护第二套设备对象模型。外部工程中的同名 `general` crate 投影常驻 `general::dev` 的公开路径、数据类型和 trait；薄包装只把方法调用转成已经绑定的真实 Rust 函数指针。ELM 因而可以按与内核代码相同的方式实现 PnP 身份、发现源、驱动 factory、`match/probe/remove`、设备 function、固件总线、platform、PCI、virtio、DMA、IRQ 和 MSI 逻辑，同时继续遵守当前内核特有的 `PnP -> DeviceFunction -> 可选投影` 抽象，而不是套用 Unix 设备号或 `file_operations`。

设备直接符号按职责拆分能力组：`DEVICE_DISCOVERY`、`DEVICE_DRIVER`、`DEVICE_RESOURCE`、`DEVICE_DMA`、`DEVICE_INTERRUPT`、`DEVICE_BUS` 和 `DEVICE_ADMIN`。符号描述符同时声明是否修改状态、是否为 `unsafe fn`、是否返回长期对象以及是否只用于诊断。装载器只保留最终 ELF 确实引用的符号槽，并在执行模块代码前检查父策略上限；设备能力属于特权组，外部镜像还必须通过签名信任验证并由 Kernel、UserAdmin 或受权 Manager 明确批准。

接口 crate 不包含设备注册表、总线状态或硬件后端。`PNP_DEVICES`、`PNP_DRIVERS` 及其它全局对象始终位于常驻 `general` 实现中；外部门面中的同名方法只操作这些真实对象。接口源码由 `kernel-symbols` 构建脚本生成规范摘要，并与目标架构、rustc、target spec、panic 策略和 `elmapi` 一并写入镜像 ABI 指纹。接口路径、类型布局或 feature 图变化会使旧镜像在执行前被拒绝，而不会以相同函数签名误绑定到不同 Rust 类型。

通过直接符号创建的长期设备对象会自动登记到当前 cell generation。常驻设备层为 function class、driver、PnP device、device function、事件订阅、firmware bus、IRQ handler/domain、MSI controller/vector、PCI host bridge 以及可替换全局后端保存真实撤销操作。ELM 主动注销时同步解除归属；detach 时运行时按资源登记逆序执行 `quiesce/cancel/drain/release`，保证模块代码释放前不再存在指向镜像的回调。当前设备回调尚未具备可逆 shadow registration，因此持有此类资源的 `PauseCell` 会明确返回不支持，不能假装暂停成功后留下悬空入口；完整 detach 仍执行不可逆安全回收。

`device.discovered@1` 的 provider payload 由 `general::dev::elm` 定义：

- `ElmDeviceDiscoveryHeader`：`abi_version`、`record_entry_size`、`record_count`、`total_count`、`flags`、`generation`。
- `ElmDeviceDiscoveryRecord`：`ordinal`、`class_len`、`name_len`、`flags`、固定长度 `class_name[16]` 和 `dev_name[64]`。
- `ELM_DEV_DISCOVERY_OPCODE_QUERY = 1`：绑定该 provider 后可通过 `InvokeProvider` 读取同一类快照；当 256 字节调用帧载荷不足以容纳全部设备时，设备层在内部 header 中设置 `TRUNCATED`。
- provider snapshot 路径使用管理通道单页 payload 上限；`ElmProviderSnapshotRequest.flags/reserved` 表达分页请求和 cursor，`ElmProviderSnapshotHeader.flags/reserved` 表达是否存在下一页和 next cursor。同步 query 路径使用 `ElmReplyFrame` 的 256 字节 payload 上限。

`device.claim@1` 的 provider payload 同样由 `general::dev::elm` 定义：

- `ElmDeviceClaimRequest`：`abi_version`、`flags`、`class_len`、`name_len`、固定长度 `class_name[16]` 和 `dev_name[64]`。
- `ElmDeviceClaimReply`：返回当前声明持有者的 binding id、class/name 和声明 flags；当前 provider invoke 无法获得 Core 内部 lease id，因此 `owner_lease_id` 暂为 0。
- `ElmDeviceClaimSnapshotHeader` / `ElmDeviceClaimRecord`：返回当前所有设备声明，包含记录数、总数、generation、binding id、class/name 和 held flag。
- `ELM_DEV_CLAIM_OPCODE_ACQUIRE = 1`：声明一个已存在的设备 function；同一 binding 重复声明同一设备是幂等成功，不同 binding 声明同一设备返回 `BUSY`。
- `ELM_DEV_CLAIM_OPCODE_RELEASE = 2`：释放当前 binding 持有的设备声明；释放不存在的声明返回 `NOT_FOUND`，释放其他 binding 的声明返回 `BUSY`。
- `ELM_DEV_CLAIM_OPCODE_QUERY = 3`：查询设备声明持有者。
- 绑定撤销时，ELM Core 调用 provider `on_revoke`，设备层会清理该 binding 持有的所有声明。

`vfs.lookup@1` 的 provider payload 由 `vfs::elm` 定义：

- `ELM_VFS_LOOKUP_OPCODE_QUERY = 1`：绑定该 provider 后可查询当前任务 VFS 上下文中的路径。
- `ElmVfsLookupRequest`：`abi_version`、`flags`、`dirfd_kind`、`lookup_flags`、`path_len` 和固定上限路径缓冲区。当前只支持 `ELM_VFS_LOOKUP_DIRFD_CWD`，非 cwd dirfd 在文件句柄租约模型完成前返回 `UNSUPPORTED`。
- `lookup_flags` 当前支持 `NO_FOLLOW`、`DIRECTORY`、`NO_SYMLINKS` 和 `NO_MOUNT_LAST`；创建类 `ALLOW_MISSING_LAST` 不暴露给 ELM lookup query，避免把“不存在”误报为查询成功。
- `ElmVfsLookupReply`：返回 POSIX errno、文件类型、mode、ino、size、nlink、uid、gid、设备号、块统计和时间戳，不返回 `Dentry`、`Inode`、`File` 或任何内核地址。
- 当前 provider 需要调用线程已经装载 `VfsContext`；在纯 Core 测试或启动早期无 VFS 上下文时返回 `NOT_FOUND`，reply 内 errno 为 `EBADF`。
- 规范路径序列化字段已经预留，但 VFS 内部可见路径导出入口尚未稳定，因此当前 `resolved_path_len` 为 0。后续应在 VFS 子系统内补齐路径序列化，而不是让 ELM Core 理解 mount/dentry 细节。

动态端口访问策略：

- `Public`：任意合法 cell 可绑定。
- `ExtensionOnly`：只有 provider owner 自身或挂接到 owner 的拓展单元可绑定。
- `Internal`：只允许 owner 自身绑定，保留给内核内部 provider。

动态端口注销规则：

- 内建 provider 端口不可注销。
- 动态 provider 端口仍有活跃 binding 时返回 busy。
- 注销成功会移除 provider runtime 和端口描述，但不会影响已经撤销的历史审计记录。

## 7. 关系模型

ELM 关系分为五类：

- 父子关系：描述管理归属。
- 依赖关系：描述启动和运行前置条件。
- 提供关系：描述当前单元对外提供的能力。
- 拓展关系：描述当前单元挂接到另一个单元开放的拓展点。
- 能力绑定关系：描述当前单元连接到某个枢纽端口。

规则：

- 除 `elm-mgr` 外，所有 ELM 必须有父单元。
- 父子关系必须无环。
- 依赖关系必须无环。
- 拓展关系默认无环，除非流契约显式允许可重入。
- 被拓展单元必须声明对应拓展点。
- 拓展项必须匹配拓展点的流契约。
- 能力提供必须带版本号。
- 能力绑定不能重复绑定同一个活动的 `(cell, port, contract)`。
- 能力绑定由 `elm-mgr` 决定策略，内核负责合法性校验。

当前内核中的 `elm-mgr` 已开放 `menu.item` 拓展点。启动拓扑不再注入演示单元，动态绑定路径已经可以为 `mgr.menu.item@1`、`core.log@1` 和 `core.event@1` 创建真实绑定和租约。

## 8. 生命周期

```text
Discovered -> Verified -> Loaded -> Linked -> Ready -> Active
Loaded -> Detached -> Retired
Active -> Quiescing -> Paused -> Active
Paused -> Detached -> Retired
Active -> Quiescing -> Detached -> Retired
Active -> Faulted -> Quarantined -> Detached -> Retired
```

中文状态：

- `Discovered`：已发现
- `Verified`：已验证
- `Loaded`：已装入
- `Linked`：已绑定
- `Ready`：已就绪
- `Active`：运行中
- `Quiescing`：静默中
- `Paused`：已暂停
- `Detached`：已脱离
- `Retired`：已退役
- `Faulted`：已故障
- `Quarantined`：已隔离

状态约束：

- 未验证的 ELM 不能装入。
- 未绑定依赖的 ELM 不能就绪。
- 未就绪的 ELM 不能运行。
- 静默中禁止新流进入。
- 活跃流未清零不能脱离。
- 尚未激活的 `Loaded` 单元可以直接脱离并退役。
- 已暂停的单元可以直接脱离并退役。
- 资源租约未撤销不能退役。
- 故障单元不能继续接收普通流，只允许诊断、隔离和退役。

当前生命周期实现：

- `PauseCell` 支持动态单元从 `Active` 进入 `Quiescing` 再进入 `Paused`；原生单元会先调用 `ElmModule::quiesce`，再调用 `ElmModule::pause`。
- `ResumeCell` 支持动态单元从 `Paused` 回到 `Active`；原生单元会先调用 `ElmModule::resume`。
- `DetachCell` 支持动态单元撤销租约、移除菜单项、摘除绑定图并退役。
- `PreflightLifecycle` 会返回阻断位、最终状态和受影响的子单元、依赖者、拓展项数量。
- 内建单元受保护，默认不能被暂停、脱离或替换。
- 含原生代码的已激活单元支持 pause/resume/detach 生命周期执行器；`ReplaceCell` 已支持 EKI Projection Source 和其他已注册 Projection Source 的迁移式热替换事务。

子系统受托资源使用与 cell 生命周期同一提交边界。每项资源必须向常驻内核登记完整的 `suspend/resume/quiesce/cancel/drain/release` 操作表；暂停按登记逆序执行 `suspend`，恢复按登记正序执行 `resume`，从而先停用依赖者、后停用基础资源，并以相反顺序重建。`suspend` 和 `resume` 都是失败即保持调用前状态的事务回调；中途失败时，注册表会逆向调用已经成功步骤的对偶操作，并在全部回滚成功后恢复 owner 接纳门。子系统内部若无法恢复调用前状态，必须返回 `ELM_OWNED_RESOURCE_STATUS_ROLLBACK_FAILED`；运行时会把资源标记为 `Failed`、保持接纳门关闭并隔离 cell，不能伪装成普通失败或成功暂停。进入 detach 后不再执行可逆恢复，而是允许从 `Active` 或 `Suspended` 状态进入不可逆的四阶段退役。

直接设备符号创建的每一项长期对象都独立登记到所属 cell generation，而不是收拢成函数表级聚合资源。当前这些对象具备完整不可逆退役链路，但尚未具备可逆 shadow registration；因此持有设备回调资源时，暂停预检或提交会明确失败，detach 则先停止并注销对象、排空内核持有引用，再允许释放原生镜像。

## 9. 热插拔与热替换

热插拔是能力图重排，而不是简单释放代码。

热插拔流程：

```text
发现单元
验证清单
装入镜像
构建候选绑定图
试运行依赖解析
静默受影响流
提交绑定
启动单元
发布拓扑事件
```

热替换流程：

```text
装入新单元
校验契约兼容性
创建影子绑定
迁移状态
切换代
排空旧代流
撤销旧租约
退役旧单元
```

关键机制：

- 每个绑定有切换代。
- 每个资源租约有切换代。
- 旧流使用旧代完成。
- 新流进入新代。
- 切换失败可以回滚旧绑定。
- 状态迁移必须显式声明状态版本和迁移函数。
- 热替换不能绕过父子、依赖、拓展和租约阻断检查。

当前 `ReplaceCell` 已具备双路径热替换事务：声明式 EBI image 在同名、同类型、surface 兼容且无 native imports/exports 时直接提交 generation、菜单元数据、provider 元数据、绑定和租约代际更新；原生目标必须是 `Active` 或 `Paused` 的动态原生单元，新旧单元的 manifest name 和 kind 必须一致，声明式拓扑、provider surface、imports/exports 和统一模块描述符必须通过兼容性校验。原生替换过程会影子装载新 image，使用新逻辑 generation 暂存其受管 import，在执行新 `ElmModule::initialize` 后静默旧单元，调用旧 `migrate_export` 导出最多 64 KiB 状态，再调用新 `migrate_import` 导入状态；提交点才会提升暂存 import、原子切换 Projection Source generation、provider backend、菜单元数据、绑定和租约代际，随后执行旧 `ElmModule::finalize` 并退役旧镜像。失败时会调用新模块的 `migrate_abort` 与 `finalize`、丢弃尚未公开的暂存 import、撤销新代际 source 并恢复旧代际 source。运行时显式区分旧代际未触碰、已静默、已恢复和状态受损四种结果；新代初始化/迁移失败或调用方授权在事务末尾失效时，只要旧代恢复成功就继续保持 `Active`，只有旧代钩子或恢复本身失败时才进入隔离诊断状态。

当前仍然主动阻断的场景包括：内建单元、Builtin/Memory 外部替换请求、未知或未注册 Projection Source、目标存在子单元/依赖者/拓展项、忙碌租约、provider 队列、运行中调用或保留结果仍未排空、缺少原生 code segment、缺少迁移钩子、native 装载器无法安全重定位，或外部 importer 无法安全 patch 到新 export。跨单元 native import 自动重绑定只支持落在可写 Data/Bss 段内的 import slot/GOT 形态；若 import relocation 落在已 seal 的 Code 段内，会被视为不可安全重绑定。v1 采用“先排空、后原子切换”的强一致语义，不承诺让同一调用跨两个 generation 继续执行；绑定对象保持原位，provider backend 与受管 import 在提交点切换。

## 10. 资源租约

ELM 不保存裸内核对象指针。所有资源都必须通过资源租约访问。

资源租约类型：

- `Device`：设备对象租约。
- `Irq`：中断事件或中断线租约。
- `Dma`：DMA 缓冲区和映射租约。
- `Mmio`：MMIO 窗口租约。
- `VfsNode`：VFS 节点租约。
- `Network`：网络接口或队列租约。
- `Block`：块设备或请求队列租约。
- `MenuItem`：菜单项租约。
- `Provider`：能力提供者引用租约。
- `RuntimePort`：当前运行时端口绑定租约。
- `Other`：暂未细分的资源租约。

租约权限：

- `READ`：只读能力。
- `WRITE`：写入能力。
- `CONTROL`：读、写和控制能力。

租约状态：

- `Active`：可使用。
- `Revoking`：正在撤销。
- `Revoked`：已撤销。

规则：

- 租约归属到具体内核单元实例。
- 租约可以绑定到具体 `BindingId`，用于形成撤销链路。
- 静默化后不能创建新租约。
- 脱离前必须撤销所有可撤销租约。
- 仍有活跃引用时退役失败。
- 租约撤销必须触发对应端口提供者清理。
- 内核维护最终资源所有权，不能完全交给 `elm-mgr`。

当前实现已经支持按 owner 撤销、按 binding 查询、busy 检查、撤销并移除单个租约、撤销并移除某个单元拥有的所有租约。`core.log@1` 和 `core.event@1` 使用 `RuntimePort` 租约，`mgr.menu.item@1` 使用菜单项租约。

## 11. 私有系统调用

ELM 不使用 Linux 模块系统调用。当前唯一入口：

```text
sys_elm_ctl(cmd, in_ptr, in_len, out_ptr, out_len) -> isize
```

命令：

- `CORE_QUERY`：查询 ELM Core 能力。
- `MGR_CALL`：向 `elm-mgr` 发送管理请求。
- `EVENT_READ`：读取全局 ELM 事件。
- `EVENT_ACK`：确认全局 ELM 事件。
- `SNAPSHOT_READ`：读取运行拓扑快照。
- `DEBUG_DUMP`：读取诊断信息。

权限规则：

- `CORE_QUERY`、`SNAPSHOT_READ`、`EVENT_READ` 和 `EVENT_ACK` 是查询型入口。
- `MGR_CALL` 和 `DEBUG_DUMP` 当前需要 `SysAdmin` 能力。
- 外部工具不直接加载普通 ELM。
- 外部工具只向 `elm-mgr` 发请求。
- `elm-mgr` 根据策略请求内核执行加载、卸载、替换等操作。
- 内核负责权限检查、缓冲复制、安全校验和最终状态变更。
- 管理调用输入上限当前为 256 KiB payload 加管理调用头。

## 12. `elm-mgr` 管理通道 ABI

`MGR_CALL` 的输入由 `ElmMgrCallHeader` 加 payload 组成，输出由 `ElmMgrResponseHeader` 加可选 payload 组成。所有跨边界结构必须是固定布局，不包含内核指针。

管理通道当前稳定边界：

- 输入 payload 上限由模型层常量 `ELM_MGR_MAX_PAYLOAD` 固定为 256 KiB。
- 完整输入上限由模型层常量 `ELM_MGR_MAX_INPUT` 固定为 `ELM_MGR_MAX_PAYLOAD + sizeof(ElmMgrCallHeader)`。
- 调用方应使用 `ElmMgrCallHeader::empty()` 或 `ElmMgrCallHeader::new()` 构造请求头，避免手写保留字段。
- `ElmMgrCallHeader.flags` 和 `ElmMgrCallHeader.reserved` 当前必须为 0。
- 无 payload 查询命令当前拒绝非空 payload。
- 已定义请求结构中的 `flags`、`reserved` 和长度字段会在进入 ELM Core 前统一校验；未知命令号返回 `UNSUPPORTED`，格式错误返回 `INVALID`。
- `dispatch_mgr_call_on_core` 是管理通道的可测试本地 Core 分发入口，用于覆盖字节协议解析，不经过系统调用复制路径。

当前命令号：

| 命令 | 编号 | 当前状态 | 说明 |
| --- | --- | --- | --- |
| `QueryMenu` | 1 | 已实现 | 返回菜单快照 |
| `LoadCell` | 2 | 已实现 | 接收带父单元和初始预算的 EBI Source 请求，外部输入统一通过已注册 Projection Source 展开为 EBI |
| `DetachCell` | 3 | 已实现 | 执行 finalize、撤销资源、退役 source、移除拓扑并回收动态单元 |
| `PauseCell` | 4 | 已实现 | 支持动态单元暂停，原生单元执行 `ElmModule::quiesce` / `pause` |
| `ResumeCell` | 5 | 已实现 | 支持动态单元恢复，原生单元执行 `ElmModule::resume` |
| `ReplaceCell` | 6 | 已实现 | 支持任意已注册 Projection Source 的迁移式热替换；Builtin/Memory 外部请求返回 Unsupported |
| `QueryTopology` | 7 | 已实现 | 返回父子、依赖、拓展点和拓展项关系 |
| `QueryPolicy` | 8 | 已实现 | 返回策略能力、支持动作和阻断位 |
| `PreflightLifecycle` | 9 | 已实现 | 生命周期操作预检 |
| `QueryAudit` | 10 | 已实现 | 返回管理操作审计环 |
| `QueryNexusBindings` | 11 | 已实现 | 返回能力绑定快照 |
| `PreflightBind` | 12 | 已实现 | 能力绑定预检 |
| `CommitBind` | 13 | 已实现 | 支持 ELM 内建端口和已注册 provider 端口，统一创建绑定与租约 |
| `PreflightUnbind` | 14 | 已实现 | 能力解绑预检 |
| `CommitUnbind` | 15 | 已实现 | 能力解绑、租约撤销和菜单项移除 |
| `SubmitRuntimeLog` | 16 | 已实现 | 通过 `core.log@1` 提交运行时日志 |
| `ReadRuntimeEvent` | 17 | 已实现 | 通过 `core.event@1` 读取运行时事件 |
| `AckRuntimeEvent` | 18 | 已实现 | 通过 `core.event@1` 确认事件游标 |
| `QueryRuntimePorts` | 19 | 已实现 | 返回运行时端口绑定统计 |
| `RegisterProviderPort` | 20 | 已实现 | 注册动态 provider 端口声明，未附带原生 handler 时标记为 TODO |
| `UnregisterProviderPort` | 21 | 已实现 | 注销无活跃 binding 的动态 provider 端口 |
| `QueryProviderPorts` | 22 | 已实现 | 返回 provider 端口、访问策略、调用统计和绑定数量 |
| `InvokeProvider` | 23 | 已实现 | ABI 和校验路径已稳定，支持 kernel provider、子系统 provider 和带 handler 的 ELM 原生 provider；无后端时返回 TODO/UNSUPPORTED |
| `QueryProviderStats` | 24 | 已实现 | 返回 provider 端口统计记录 |
| `QueryHealth` | 25 | 已实现 | 返回 17 类 Core 结构健康记录，覆盖拓扑、运行时、信任、source、journal、资源、执行和序列不变量 |
| `SubmitProviderCall` | 26 | 已实现 | 提交异步 provider 调用，成功后返回 ticket |
| `PollProviderReply` | 27 | 已实现 | 按 ticket 查询并领取异步 provider 结果 |
| `CancelProviderCall` | 28 | 已实现 | 取消排队 provider 调用，或对运行中调用提交取消意图 |
| `QueryProviderQueue` | 29 | 已实现 | 返回 provider 异步队列、运行中数量、结果保留和拒绝统计 |
| `QueryApiRegistry` | 30 | 已实现 | 返回 `elm-mgr` API 注册表，供 ELM 框架发现可用 API |
| `SubscribeEvent` | 31 | 已实现 | 创建事件订阅租约，返回订阅 ID、租约 ID 和初始游标 |
| `UnsubscribeEvent` | 32 | 已实现 | 撤销事件订阅租约并移除订阅记录 |
| `QueryEventSubscriptions` | 33 | 已实现 | 返回当前事件订阅快照 |
| `ReadSubscribedEvents` | 34 | 已实现 | 按订阅 ID 和游标读取事件，`ADVANCE` flag 控制是否推进订阅游标 |
| `QueryProviderSnapshot` | 35 | 已实现 | 按 port 或 binding 调用 provider snapshot 回调；支持 cursor 分页；无回调返回 provider 级 `UNSUPPORTED` |
| `QueryNativeCapabilities` | 36 | 已实现 | 返回原生 ELM imports/exports 快照 |
| `QueryTodoRegistry` | 37 | 已实现 | 返回运行时 TODO registry，覆盖静态未完成项和当前动态 TODO 后端 |
| `QueryExtensions` | 38 | 已实现 | 返回拓展点和拓展挂接快照，作为 mixin 管理面的稳定查询入口 |
| `PreflightExtensionAttach` | 39 | 已实现 | 预检拓展挂接，校验单元、拓展点、契约、重复挂接和拓扑环 |
| `CommitExtensionAttach` | 40 | 已实现 | 提交拓展挂接并写入拓扑、事件和审计 |
| `CommitExtensionDetach` | 41 | 已实现 | 按 extension、target 和 point 精确解绑拓展关系 |
| `DispatchExtension` | 42 | 已实现 | 校验并匹配拓展 dispatch 输入，按拓展边调用 extension provider handler，支持受控 pipeline、stop、replace 和 deny 语义 |
| `QueryFaultDump` | 43 | 已实现 | 返回最近一次 ELM protected-call 架构 fault 恢复记录；无 fault 时返回空记录集 |
| `QueryLifecycleTrace` | 44 | 已实现 | 返回生命周期 trace ring，覆盖生命周期审计和状态迁移结果 |
| `QueryProviderCallTrace` | 45 | 已实现 | 返回 provider 同步调用 trace ring，记录 binding、port、状态和阻断位 |
| `QueryMixinTrace` | 46 | 已实现 | 返回 mixin dispatch trace ring，记录 target、extension、调用数量和阻断位 |
| `QueryReplaceTrace` | 47 | 已实现 | 返回热替换事务 trace ring，记录 generation、迁移长度、状态和阻断位 |
| `QueryPolicyTrace` | 48 | 已实现 | 返回 cell policy 更新 trace ring |
| `QueryResourceDiagnostics` | 49 | 已实现 | 返回资源预算更新和配额拒绝 trace ring |
| `QueryRuntimeJournal` | 50 | 已实现 | 返回运行时 journal ring，作为启动、审计和关键管理动作的结构化历史 |
| `QueryCellPolicy` | 51 | 已实现 | 查询单元级 capability policy 快照 |
| `UpdateCellPolicy` | 52 | 已实现 | 以 generation 与 policy epoch 做乐观并发校验，执行锁定、父子能力委派和运行中调用保护后更新策略 |
| `QueryResourceBudget` | 53 | 已实现 | 查询单元资源预算和当前用量 |
| `UpdateResourceBudget` | 54 | 已实现 | 更新动态单元资源预算；新预算必须覆盖自身用量与子预算，并受直接父单元总预算约束 |
| `QueryTrustState` | 55 | 已实现 | 返回信任根、撤销、已接受 release epoch 和 unsigned-active 状态 |
| `BeginImageSession` | 56 | 已实现 | 创建有界镜像上传会话并预留资源预算 |
| `WriteImageSession` | 57 | 已实现 | 按偏移写入镜像分块并拒绝重叠、越界或不连续状态 |
| `SealImageSession` | 58 | 已实现 | 校验完整长度与摘要并封存镜像会话 |
| `AbortImageSession` | 59 | 已实现 | 中止镜像会话并释放其资源记账 |
| `QueryImageSession` | 60 | 已实现 | 查询镜像会话状态、长度、摘要和过期时间 |

阻断位用于把策略拒绝转化为可观测原因：

- 内建单元受保护。
- 目标单元不存在。
- 当前状态不允许操作。
- 原生代码生命周期执行器未完成。
- 存在子单元、依赖者或拓展项。
- 租约忙碌。
- 热替换安全预检未通过或当前 Source 尚未实现。
- 绑定图不一致。
- 装载来源不支持或仍缺少对应 EBI Source 实现。
- 端口不存在、契约不匹配、绑定重复或端口尚未实现。
- 绑定不存在或受保护。
- provider 不存在或 provider 仍有活跃 binding。
- 单元级 capability policy 拒绝当前操作。
- 调用者不存在、generation 或 policy epoch 已陈旧。
- 调用者不在目标单元的自身或祖先作用域内。
- 策略或预算试图向上放大，或父级总预算不足。
- 镜像来源不可信、Rust ABI 指纹不匹配或 release epoch 回滚。

## 13. EBI、EBI Source、EKI 与 soyo

EBI 的全称是 ELM Binary Interface，中文称为 **ELM 二进制装载接口**。

EBI 不是文件格式。ELM Core 不理解镜像布局、容器布局、分发方式或外部输入格式，也不应该把某种磁盘格式写进核心。外部格式必须由 Projection Source 投影成 EBI 协议对象；启动期内建对象和内核测试对象分别使用 Builtin 与 Memory source kind。ELM Core 只消费 EBI 协议对象。

EKI 是 ELM 原生镜像格式。它的产生目标是让 ELM 在通用 soyo 文件类型进入内核上游前拥有稳定、直接、强 EBI 贴合的镜像承载方式。EKI 不需要模拟通用容器，它应当把 target、manifest、menu、entry、segment、依赖、拓展点和能力声明自然展开为 EBI。

soyo 是未来可拓展文件类型。soyo 本身不实现 EBI，也不应成为 ELM Core 的硬依赖；它只是具备通过某个 ELM profile、对象或 section 组合实现出 EBI 的能力。未来 soyo 可以产出 EBI，也可以内嵌 EKI，但 ELM Core 仍只认 EBI。

当前实现中，`elm-mgr` 只作为 `<builtin>` 根管理单元存在，不具备 EKI 镜像，也不通过 EKI 自举。EKI 投影能力归属于 `elm-mgr` 下的内建子单元 `eki`；用户态工具展示元数据时应显示：

```text
elm-mgr source=<builtin>
eki     source=<builtin>
demo    source=eki
```

上例中的 `demo source=eki` 是用户态展示标签，不是 Core source kind。`ElmEbiSourceKind` 只有 `Projection`、`Builtin` 和 `Memory` 三种值；外部管理通道只接受 `Projection`。EKI 由内建 `eki` 子单元注册固定 Projection Source ID `0x454b_4900_0000_0001`，并通过该 provider 完成 EKI -> EBI 投影。后续 soyo、ELF 或其他容器若要进入 ELM，也必须注册自己的 Projection Source，而不是让 ELM Core 识别具体文件类型。

当前 EBI 对象包含：

- `ElmEbiSourceRequest`：管理装载信封，显式携带 EBI Source kind、父单元 ID、初始资源预算、Source payload 长度和受控授权 flags；固定线格式为 96 字节，全部对齐字节都由显式保留字段覆盖。
- `ElmProjectionSourceRequest`：Projection Source 分发头，携带 provider ID 和实际格式 payload 长度；固定线格式为 24 字节。
- `ElmEbiTarget`：目标架构、EBI ABI 版本和最低 Core 版本。
- `ElmEbiArch`：`Any`、`Riscv64`、`LoongArch64`。
- `ElmEbiUnit`：清单、目标、菜单声明、段声明、入口声明、依赖声明、拓展点声明、拓展声明、provider port 声明、imports 和 exports 元数据。
- `ElmEbiSegment`：段类型、大小、权限 flags、file size、mem size、对齐、EBI Source block 索引、Source 偏移和内容 hash。
- `ElmEbiEntry`：旧式独立原生入口描述；新 Rust ELM 不再单独生成该记录。
- `ElmEbiMenuDecl`：菜单项 kind、flags、label、description 和 route。
- `ElmEbiDependencyDecl`：依赖的目标单元名和契约。
- `ElmEbiExtensionPointDecl`：当前单元开放的拓展点名和契约。
- `ElmEbiExtensionDecl`：当前单元挂接的目标单元名、拓展点名和契约。
- `ElmEbiProviderPortDecl`：当前单元声明的 provider port 契约、访问策略、方向和模式。
- `ElmEbiImportDecl`：当前单元需要的受管、`direct-pinned` 或 `kernel-symbol` 能力入口元数据；直接调用必须携带完整 Rust ABI SHA-256。
- `ElmEbiExportDecl`：当前单元开放的受管或 `direct-pinned` 能力入口元数据；它不是不受约束的全局符号表。
- `ElmModuleDescriptorV1`：原生 Rust ELM 唯一的统一模块描述符，固定导出为 `__elm_module_descriptor_v1`，集中给出生命周期、迁移和 entry 入口。
- `ElmEbiLifecycleHooks`：旧式生命周期表记录。解析器仍能识别该结构，但新原生镜像必须使用统一模块描述符，不能用该表代替描述符。
- `ElmLoadCellResponse`：装载结果、单元 ID、最终状态和原因。

当前 EBI 校验规则：

- ABI 版本必须匹配 `ELM_EBI_ABI_VERSION`。
- 目标架构必须匹配当前内核架构，或使用 `Any`。
- `min_core_version` 不能为 0。
- 段数量不能超过 `ELM_EBI_MAX_SEGMENTS`。
- 段大小、内存大小不能为 0，`file_size` 不能大于 `mem_size`，`align` 非 0 时必须是 2 的幂。
- 代码段必须可执行且不可写；只读数据段不可写不可执行；数据段可写不可执行；BSS 必须 `file_size == 0` 且带零填充语义；重定位段必须标记为重定位输入。
- 若旧式原生入口记录存在，其符号不能为空并且必须通过 EBI 符号名校验；新镜像的 entry 来自统一模块描述符。
- 菜单 label 和 route 不能为空，并且各字段不能超过固定长度。
- 依赖、拓展点、拓展项和 provider port 声明数量不能超过固定上限。
- 依赖和拓展目标使用 manifest name；provider port 复用现有访问策略、方向和模式枚举。
- provider port 声明的 flags 当前必须为 0。
- imports 和 exports 数量不能超过固定上限，契约名必须通过能力契约校验。受管 import/export 用于代际路由和热替换重绑定；`direct-pinned` import/export 还必须提供非零 Rust ABI 摘要并固定目标 generation。`kernel-symbol` import 不参与 ELM export 选择，不能携带 ELM 作用域位；它必须由常驻内核目录按名称、契约、版本、ABI 摘要和能力策略解析，必需符号不存在或权限不足时装载失败，可选符号才允许保持空槽。
- 原生 EBI image 可以携带 payload、符号位置表和 EKI 原生重定位表；带 Code 段的新动态单元必须能在只读段中定位尺寸、对齐和固定头均有效的 `__elm_module_descriptor_v1`，描述符中的所有入口都必须落在 Code 段内。
- EKI 原生重定位当前使用 EKI relocation v1，不直接消费 ELF relocation；支持 image base、segment base、symbol absolute 和 symbol relative 写入语义。
- 动态原生 ELM 必须实现 `ElmModule` 并导出唯一的 `ElmModuleDescriptorV1`；`create`、`initialize` 和 `finalize` 不允许缺失。
- 开发侧只写安全 Rust trait 方法。`#[elm::module]` 生成固定机器边界，生命周期入口使用 `ElmNativeHookContextV1` 指针和 `i32` 状态码，迁移和 entry 使用各自固定 frame。
- 描述符固定头、ABI 版本、结构尺寸、实例布局和所有入口地址都会在执行任何模块代码前校验。模块实例由 `ModuleSlot<T>` 按 generation 管理，不保存在描述符中。

生命周期执行语义：

- `ElmModule::create` 为当前 generation 构造唯一模块实例，`initialize` 负责服务注册、事件订阅、工具能力发布或数据结构准备。
- `ElmModule::finalize` 负责撤销自定义状态、注销服务、解除订阅和释放由单元持有的资源；成功后运行时才销毁实例。
- 这些方法是 ELM 的强制运行时契约，不代表传统模块 `init/exit` ABI。一个 ELM 可以只是工具单元，但仍必须实现统一 trait。
- `migrate_export`、`migrate_import` 和 `migrate_abort` 是热替换迁移方法；默认导出和导入返回不支持，避免未实现迁移的模块被误判为可安全替换。
- 原生 import 在 `initialize` 前进入与独占 execution token 绑定的暂存事务。暂存 handle 只在允许的生命周期阶段可见；首次装载使用 generation 1，热替换使用尚未公开的新逻辑 generation。只有镜像信任、生命周期、entry 和提交校验全部成功后，暂存记录才一次性提升为 Active import，任何失败路径都会在 finalize 完成后丢弃暂存记录。
- 当前阶段已具备 `ElmContext`、固定原生 frame、统一模块描述符和原生 EKI 生命周期执行器；没有 Code/entry/relocation 的声明式单元可由运行时默认生命周期闭合。
- `initialize` 失败时不会激活拓扑，单元进入 `Faulted -> Quarantined`，返回 hook failed reason。
- 已初始化单元卸载前必须执行 `finalize`；失败时保留资源和单元用于诊断，成功后才撤销租约、菜单、binding、provider 和图节点。
- 内建 `elm-mgr` 和内建 `eki` 目前使用启动期合成生命周期状态，二者都以 `<builtin>` 来源进入同一套可观测 cell 模型。

当前装载语义：

- 所有动态单元都必须挂在 `elm-mgr` 管理树中，但不再被硬编码为 `elm-mgr` 的直接子单元。`ElmEbiSourceRequest.parent_cell_id` 可以选择任意处于 `Active`、未隔离且位于调用者自身或后代作用域内的父单元。
- 新单元的普通 capability policy 从直接父单元向下继承，不能获得父单元没有的 action、provider、extension、native 或 resource 权限；`LOCKED` 不自动继承，`DENY_CHILD_ESCALATION` 与 `AUDIT_ALL` 会沿管理树继承。management capability 永不自动继承。父策略缩减若会让现有子策略越过新上限，则事务被拒绝。
- 装载事务会在执行外部原生钩子前持有调用者和动态父单元的 execution token；装载期间不能并发替换、暂停、调整策略或调整预算。提交阶段重新校验 principal、generation、policy epoch、父单元状态和授权作用域。
- 初始预算必须位于直接父单元预算内，并且父单元当前用量加所有活跃直接子预算再加新预算不得超过父单元总预算。
- 动态 EBI 协议对象必须先完成统一模块描述符或声明式生命周期校验；带 Code 段且具备 payload、符号位置表和可解析 EKI relocation v1 的 EKI image 会进入原生执行器。
- 菜单拓展 EBI 协议对象会被解析和预检；无 Code/entry/relocation 的声明式对象由运行时默认生命周期闭合并直接进入 `Active`，随后挂接到 `elm-mgr` 的 `menu.item` 拓展点并创建菜单租约和菜单项。
- 声明式拓扑 EBI 会在改状态前预检 manifest name 唯一性、依赖目标存在性、拓展点存在性、契约匹配和 provider port 契约冲突。
- 原生 EKI 在解析 imports、复制段、应用重定位、切换 W^X 权限、同步指令缓存并校验统一模块描述符后调用 `ElmModule::initialize`；初始化成功的单元才会把依赖、拓展点、拓展项和 provider port 登记到 BindingGraph、PortRuntime 和 ProviderRuntime。普通能力绑定仍由 `PreflightBind/CommitBind` 完成，不在装载时自动创建。
- provider port 声明会在激活阶段注册为动态 provider；声明 `handler_symbol` 且符号可定位时注册为 ELM 原生 provider backend，否则保持不可调用或被原生镜像预检拒绝。
- 带 Code payload 的新 EKI 从统一模块描述符取得 entry，并在激活到 `Active` 后通过 `ElmNativeEntryFrameV1` 调用；框架提供默认空 entry，因此描述符入口始终完整。
- 带 Code/entry/relocation 但缺少架构映射 ops 的环境会停在 `Loaded + NativeCodeTodo`，不执行代码；Projection Source 通过内核 Projection Source registry 转换为 EBI 协议对象。
- `MGR_CALL(LoadCell)` 接收 96 字节 `ElmEbiSourceRequest`，当 `source_kind == Projection` 时，其后必须是 24 字节 `ElmProjectionSourceRequest` 和对应格式 payload。请求必须显式给出非零父单元 ID 和初始预算。`ELM_EBI_SOURCE_FLAG_GRANT_MANAGEMENT` 只允许 Kernel、UserAdmin 或内建 `elm-mgr` 发起，并且只对经过完整签名验证、kind 为 Manager、父单元已持有 management capability 的镜像生效；普通子单元不会继承该能力。EKI 使用 provider ID `0x454b_4900_0000_0001`；启动期 `elm-mgr` 和 `eki` 均使用 Builtin source，测试和内核内部可直接走 Memory source 的 EBI 单元入口。
- 内建 EKI Projection Source 会校验 `Code`、`ReadOnlyData`、`Data`、`Bss`、`Relocation`、`Notes` payload block 与 `Segments` 表逐项一致，再展开为 EBI segment 元数据。
- 内建 EKI Projection Source 支持 header SHA-256 内容证明；`image_hash_offset/image_hash_size` 非 0 时必须指向 32 字节 SHA-256，校验规则为把该 32 字节范围视为全 0 后覆盖整份 EKI 文件计算摘要。
- 外部 EBI image 必须携带 Rust ABI fingerprint，运行时会校验 `elmapi v1`、panic 策略、代码模型、目标特性、rustc 标识、target spec 和内核 API 标识；不匹配会在执行任何镜像代码前返回 `ABI_FINGERPRINT`。
- `ElmEbiProofV1` 对 canonical EBI digest、来源 identifier、来源摘要、签名者 key id、公钥、release epoch 和 ABI fingerprint 进行 Ed25519 签名。无效 proof 即使在允许 unsigned 的测试策略下也不能降级为 unsigned 装载。
- 构建时可通过 `ELM_TRUST_ANCHORS_FILE=<path>` 注入信任根。兼容格式为 `<key-identifier> <64位Ed25519公钥十六进制>`，完整格式为 `<key-identifier> <rollback-authority-identifier> <64位Ed25519公钥十六进制>`；兼容格式把 key identifier 同时作为 rollback authority。多个轮换 key 可以共享同一个稳定 rollback authority，构建脚本会拒绝无效 identifier、公钥长度、十六进制、公钥、重复 key identifier 和重复公钥；信任根在 `elm-mgr` 初始化时恢复持久回滚下界后立即 seal。
- 信任库按“稳定 rollback authority 摘要 + 模块名摘要”记录已接受 release epoch，而不是按签名 key 划分回滚域。轮换 key 只负责当前镜像验签，不能通过更换 key 绕过已有 epoch；低于已接受 epoch 的镜像会返回 `ROLLBACK_REJECTED`。验签接受记录先写入 runtime journal，再提交内存信任状态；持久 journal 启动回放会压缩并恢复每个回滚域的最高 epoch。`QueryTrustState` 暴露 sealed、anchor、revocation、accepted epoch 和 unsigned-active 状态。
- 内建 EKI Projection Source 仍能解析旧式 `LifecycleHooks` block，但 `elm-tools` 不再为新 Rust ELM 生成该 block。
- 内建 EKI Projection Source 已支持 `SymbolLocations` 元数据 block，用于定位统一模块描述符、provider、import/export 和其他原生符号。
- 内建 EKI Projection Source 已支持 imports 和 exports 元数据 block。受管和 `direct-pinned` import 通过 ELM Core 的原生 export 注册表解析；`kernel-symbol` 通过链接期常驻目录解析，并在段重定位前生成真实地址绑定。
- `MGR_CALL(ReplaceCell)` 接收 `ElmReplaceCellRequestV1 + ElmProjectionSourceRequest + source payload`，适用于所有已注册 Projection Source。`migration_limit == 0` 表示使用默认 64 KiB 上限，非 0 时不得超过该上限。
- Projection Source 会按 provider ID 调用已注册投影器；注册表负责 owner、generation、影子 source、活动引用、暂停、原子切换和退役。Busy 退役与恢复冲突会在修改状态前失败，不产生半退役 source。Builtin/Memory 外部请求返回 `UNSUPPORTED`。
- 空 payload、非法 EBI Source 请求、损坏 EKI 或未知 Source kind 会返回 `INVALID`。

尚未完成的格式与生态边界只有：

- TODO(elm)：未来 soyo ELM profile 和其他容器到 EBI 协议对象的独立 Projection Source。
- TODO(elm)：外部 Rust ELM 的调试符号归档、依赖锁定和发布仓库流程。attribute 开发框架、独立仓库模板、双架构 PIE 构建、EKI 打包、签名和运行期装载链路已经完成。

## 14. 模型层模块设计：`libs/elm`

`libs/elm` 是纯模型层。它是 `no_std` crate，只描述架构无关、内核无关的协议和模型，不能依赖 `kernel`、`general` 或 `arch`。它的目标是让内核、用户态管理工具、测试和未来 Rust ELM 框架共享同一套稳定数据结构。

### `lib.rs`

职责：

- 声明模型层 crate 的模块边界。
- 统一 re-export 控制面、EBI、错误、事件、绑定图、ID、租约、清单、菜单、管理通道、枢纽连接层、端口、快照、状态机和拓扑模型。
- 保持 `libs/elm` 对内核实现无依赖，使模型层可以被内核、host 单测和未来用户态工具复用。

设计细节：

- crate 使用 `#![no_std]` 和 `alloc`。
- `lib.rs` 是 ELM 稳定模型 API 的聚合出口。
- 后续新增模型必须先判断是否属于稳定协议层；如果只服务某个内核执行器，不应放入 `libs/elm`。
- 用户态 C 工具通过 `userland/elmctl/include/elmctl_abi.h` 镜像这些固定布局 ABI；头文件对关键结构使用 `_Static_assert`，确保 `repr(C)` 布局变化会在交叉编译阶段暴露。

### `ctl.rs`

职责：

- 定义私有控制面协议。
- 定义 `ELM_CTL_MAGIC`、`ELM_CTL_ABI_VERSION` 和 Core 能力位。
- 定义 `ElmCtlCommand`：`CoreQuery`、`MgrCall`、`EventRead`、`EventAck`、`SnapshotRead`、`DebugDump`。
- 定义 `ElmCtlStatus`，把模型层错误映射为控制面状态。
- 定义 `ElmCtlHeader` 和 `ElmCoreInfo` 固定布局结构。

设计细节：

- `ElmCoreInfo` 暴露 cell、port、lease 数量和最新事件序列。
- 能力位当前表示快照、事件和管理通道可用。
- 控制面只暴露 ELM 自己的 ABI，不承诺 Linux 模块兼容。

### `ebi.rs`

职责：

- 定义 EBI 协议对象。
- 把未来文件格式和 ELM Core 消费对象解耦。
- 校验目标架构、ABI 版本、段声明、菜单声明、统一模块描述符和旧式生命周期表。

设计细节：

- `ElmEbiArch::Any` 可用于架构无关的声明和工具单元。
- `ElmEbiLifecycleHooks` 是旧式表结构；若出现仍要求完整初始化和终结记录，但新原生 ELM 不再生成它。
- `ElmModuleDescriptorV1` 是当前原生 Rust ELM 的唯一生命周期入口表，不定义 C/C++ ABI。
- 生命周期开发语义由 `ElmModule` trait 和 `#[elm::module]` 包装；原生机器边界使用固定 frame 和 `i32` 状态码。
- `Code` 和 `Relocation` 段会触发原生装载器需求。
- `entry` 存在时也视为需要原生装载器。
- 无 Code/entry/relocation 的声明式 EBI 由运行时默认生命周期闭合；带 Code 的新镜像必须提供统一模块描述符。
- `ElmEbiImage` 在 `ElmEbiUnit` 之外携带 payload、符号位置和 EKI relocation v1，供内核原生执行器消费。
- `NativeCodeTodo` 表示当前架构缺少原生镜像映射能力，或镜像命中了其它尚未支持的原生执行边界；内核直接符号目录已经实现，不再使用该状态表示符号导入。

### `error.rs`

职责：

- 定义 `ElmError` 和 `ElmResult`。
- 统一表达重复单元、非法名称、非法版本、非法契约、状态迁移错误、图环、租约 busy、权限拒绝等错误。

设计细节：

- 错误类型服务于模型校验，不直接暴露内核 errno。
- 控制面和管理通道负责把模型错误转成固定 ABI 状态码。

### `event.rs`

职责：

- 定义固定布局事件记录。
- 定义事件序列号。
- 把拓扑事件类型编码成跨边界整数。

设计细节：

- `ElmEventRecord` 包含 sequence、kind、cell、port、binding、lease。
- 空事件由 `ElmEventRecord::zero()` 表示。
- 事件记录不携带指针，也不携带可变长度字符串。

### `frame.rs`

职责：

- 定义枢纽连接层同步调用帧。
- 定义 256 字节固定内联 payload。
- 定义调用状态码和调用 flags。

设计细节：

- `ElmCallFrame` 携带 binding、call、opcode、flags、payload_len 和 payload。
- `ElmReplyFrame` 携带 binding、call、status、flags、payload_len 和 payload。
- 调用帧不携带内核指针，不表达任何具体文件格式，也不依赖原生代码装载器。
- 当前调用帧用于稳定 `InvokeProvider` 边界；没有真实执行器的 provider 不会被当作调用成功。

### `graph.rs`

职责：

- 定义绑定图。
- 管理父子、依赖、拓展点、拓展项和能力绑定关系。
- 校验父子环、依赖环、拓展环和重复能力绑定。

设计细节：

- `BindingGraph` 只存模型关系，不执行端口语义。
- `CapabilityBindingEdge` 记录 binding、consumer、port、contract、generation、lease 和 active。
- `remove_cell` 会移除相关父子、依赖、拓展和能力绑定边。
- 图校验是生命周期预检和绑定预检的硬约束。

### `ids.rs`

职责：

- 定义强类型 ID：`ElmId`、`PortId`、`BindingId`、`ActionId`、`LeaseId`、`Generation`。

设计细节：

- 所有 ID 都是 `u64` 新类型，避免把不同 ID 混用。
- `Generation::FIRST` 为 1，`next()` 用于后续热替换切换代。

### `lease.rs`

职责：

- 定义资源租约模型。
- 定义租约类型、权限、状态和租约注册表。
- 提供撤销、busy 检查和按 binding 查询能力。

设计细节：

- `ResourceLease` 可选绑定到 `BindingId`。
- `begin_revoke` 只允许 `Active -> Revoking`。
- `finish_revoke` 要求没有活跃引用。
- `LeaseRegistry` 当前支持按 owner 批量撤销和按 binding 撤销。

### `manifest.rs`

职责：

- 定义单元清单。
- 定义名称、版本、类型、能力意图和能力提供。

设计细节：

- 名称长度上限 128，版本长度上限 64。
- 名称只允许小写字母、数字、`.`、`-` 和 `_`。
- 类型包括 Manager、Service、Driver、Extension、Filesystem、Network、Debug 和 Other。
- 清单是 ELM 的自描述边界，不包含内核内部类型。

### `menu.rs`

职责：

- 定义 `elm-mgr` 菜单固定布局模型。
- 定义菜单项类型、flags 和快照结构。

设计细节：

- 菜单项类型包括 Group、Action、Toggle 和 Status。
- flags 包括 TODO、Disabled 和 RequiresSysAdmin。
- label、description、route 使用固定长度数组，避免跨边界分配。

### `mgr.rs`

职责：

- 定义 `elm-mgr` 管理通道 ABI。
- 定义命令号、状态码、策略位、阻断位、生命周期请求响应、拓扑快照、审计快照、绑定快照和运行时端口请求响应。

设计细节：

- 当前命令号为 1..60，覆盖动态 provider 注册、注销、查询、同步调用、异步提交、轮询、取消、队列统计、provider snapshot、Core 健康查询、API 注册表、事件订阅、fault dump、运行时 trace、单元策略、资源预算、信任状态和镜像上传会话管理。
- `ElmMgrPolicyInfo` 暴露支持动作、策略 flags、阻断位 mask 和审计容量。
- `ElmRuntimeTraceHeader` 和 `ElmRuntimeTraceRecord` 是运行时结构化观测 ABI，统一承载 lifecycle、provider call、mixin dispatch、replace、policy、resource 和 journal 七类 ring buffer。
- `ElmCellPolicyRequest` 和 `ElmCellPolicyV1` 是单元级 capability policy ABI；当前支持 generation 与 policy epoch 并发校验、不可逆 `LOCKED`、父子能力子集校验、`DENY_CHILD_ESCALATION`、`AUDIT_ALL` 继承和 execution token 忙碌保护。生命周期、能力绑定、事件订阅、provider 注册/调用、provider 异步提交和 mixin 挂接/dispatch 会检查对应 policy 位。
- `ElmResourceBudgetRequest`、`ElmResourceBudgetUpdateRequest` 和 `ElmResourceBudgetResponse` 是分层资源预算 ABI；更新会校验新预算覆盖自身当前用量与活跃直接子预算，并校验该预算仍位于直接父单元的总预算内。
- `ElmLifecyclePlanResponse` 用 allowed、status、final_state 和 blockers 表示预检结果。
- `ElmNexusBindRequest` 使用固定长度契约字段。
- `ElmRuntimeLogRequest` 使用 256 字节固定日志 payload。
- `ElmRuntimeEventResponse` 使用 `has_event` 区分是否返回事件。
- `ElmRuntimePortStatsRecord` 暴露 binding、cell、port、lease、cursor、日志提交数、事件投递数和丢弃事件数。
- `ElmProviderPortRegisterRequest` 描述 provider owner、契约、方向、模式和访问策略。
- `ElmProviderInvokeRequest` 和 `ElmProviderInvokeResponse` 封装通用调用帧。
- `ElmProviderAsyncSubmitRequest` 和 `ElmProviderAsyncSubmitResponse` 封装异步提交请求和 ticket 响应；`timeout_ms=0` 使用默认超时，`result_ttl_ms=0` 使用默认结果保留 TTL，非零值会被上限钳制。
- `ElmProviderAsyncPollRequest` 和 `ElmProviderAsyncPollResponse` 按 ticket 查询状态；终态结果被 poll 后会从结果环移除，并释放对应 provider 租约活跃引用。
- `ElmProviderAsyncCancelRequest` 和 `ElmProviderAsyncCancelResponse` 取消仍在队列中的任务；已经完成或已经过期的 ticket 不会被强行回滚。
- `ElmProviderQueueStatsHeader` 和 `ElmProviderQueueStatsRecord` 暴露 queued、running、retained、queue_limit、max_in_flight、submitted、completed、canceled、expired 和 rejected。
- `ElmProviderSnapshotRequest` 和 `ElmProviderSnapshotHeader` 暴露 provider 自有快照读取边界；管理通道成功不等于 provider 成功，provider 状态在 header 内表达。
- `ElmProviderPortRecord` 和 `ElmProviderPortStatsRecord` 暴露 provider 绑定数量和调用统计。
- provider 观测 flags 已稳定为 `DYNAMIC`、`KERNEL_BACKEND`、`NATIVE_BACKEND` 和 `TODO_BACKEND`，用于区分动态声明端口、内核/子系统后端、ELM 原生 handler 后端和等待执行器的 TODO 后端。
- `PROVIDER_CALL_FAILED` 阻断位用于审计 provider transport 成功但 `ElmReplyFrame.status` 失败的业务调用。
- `ElmCoreHealthHeader` 和 `ElmCoreHealthRecord` 暴露 Core 自检结果；每类检查通过时也会输出 OK 记录，失败时携带对象 ID 和 detail。
- API 注册表和事件订阅命令是 `elm-mgr` 作为 API 网关的第一组外部可发现能力。
- `ElmMgrApiRegistryHeader` 和 `ElmMgrApiDescriptor` 是 `elm-mgr` API 网关的发现 ABI，描述 API 命名空间、名称、契约、类型、flags、命令号和 owner。
- `ElmMgrEventSubscribeRequest` / `ElmMgrEventSubscribeResponse` 负责创建事件订阅；订阅本身由 `EventSubscription` 租约保护。
- `ElmMgrEventSubscriptionHeader` / `ElmMgrEventSubscriptionRecord` 返回订阅快照，包含过滤器、游标、投递计数和丢弃计数。
- `ElmMgrSubscribedEventReadRequest` / `ElmMgrSubscribedEventReadHeader` 负责按订阅读取事件；`ELM_MGR_EVENT_READ_FLAG_ADVANCE` 为 0 时只读取不推进订阅游标，为 1 时读取后推进游标。

### `mgr/api.rs`

职责：

- 定义 `elm-mgr` 管理协议使用的固定布局记录；该文件是 `elm` crate 的私有实现模块，不构成公开 Rust 模块路径。
- 把 API 注册表、事件订阅和订阅读取结构从管理通道主文件中拆出；公开的类型由 `elm` crate 根统一导出，管理操作由 `elm::management::Client` 提供类型化方法。
- 为运行时管理 API 和可发现 provider API 保留统一描述格式；普通稳定子系统 Rust API 不伪装成 `elm-mgr` API，而由同名接口 crate 和内核直接符号目录提供类型化调用。

设计细节：

- API 描述使用固定长度命名空间、名称和契约字段，不携带指针。
- API 类型分为 Control、Snapshot、Event、Provider 和 Subsystem。
- API flags 区分 Stable、TODO、Syscall、Sysfs 和 ProviderOps。
- 事件订阅支持按事件类型、cell、port、binding 和 lease 过滤。
- 事件订阅读取和 `core.event@1` 运行时端口读取是两条路径：前者是 `elm-mgr` API 网关能力，后者是枢纽连接层端口能力。

### `nexus.rs`

职责：

- 定义枢纽连接层模型。
- 定义流契约、能力意图、能力提供、方向、模式、并发和背压。

设计细节：

- `FlowContract` 是稳定兼容边界。
- `NexusIntent` 表达 Consume、Offer、Extend、Observe 和 Control。
- `NexusOffer` 表达契约和流模式。
- 并发和背压先进入模型层，真实调度由后续端口执行器实现。

### `ports.rs`

职责：

- 定义 ELM 自有内建端口描述。
- 为启动期 ELM Core 提供不含子系统语义的端口列表。

设计细节：

- 当前有 4 个 ELM 自有内建端口：`core.log@1`、`core.event@1`、`mgr.menu.item@1` 和 `mgr.action.invoke@1`。
- VFS、设备、网络、IRQ、DMA、MMIO 等端口不在这里声明，必须由各子系统自己的 `elm.rs` 导出 provider specs。
- 端口描述已包含访问策略和是否可调用标记。

### `provider.rs`

职责：

- 定义 `ElmKernelProviderSpec` 通用规格。
- 定义内核 provider 的 `invoke`、单页 `snapshot`、分页 `snapshot_paged` 和 `on_revoke` 回调形状。
- 提供默认 unsupported 回调，用于尚未接入真实子系统逻辑的 TODO provider。

设计细节：

- `ElmKernelProviderSpec` 同时描述 API 注册表记录和 provider 端口记录。
- 规格只保存静态字符串、方向、模式、访问策略和函数指针，不包含子系统私有状态。
- 子系统 provider 可以使用完整 `ElmKernelProviderSpec::new` 接入真实 `invoke` / `snapshot` / `on_revoke` 回调，并可通过 `with_paged_snapshot` 增加 cursor 分页快照；`device.discovered@1` 已使用该路径输出设备发现快照。
- 尚未接入真实语义的子系统 provider 使用 `subsystem_todo` 构造规格，调用会经过完整 provider runtime 后返回 `UNSUPPORTED`。

### `snapshot.rs`

职责：

- 定义固定布局运行拓扑快照。
- 定义 cell 和 port 快照结构。

设计细节：

- `ElmSnapshotHeader` 携带 ABI 版本、entry size、cell/port/lease 数量和事件序列。
- `ElmCellSnapshot` 携带 ID、parent、state、kind、EBI 架构、EBI 状态、是否原生代码、generation 和名称。
- `ElmPortSnapshot` 携带 ID、owner、方向、模式、实现状态和契约。

### `state.rs`

职责：

- 定义内核单元生命周期状态机。
- 定义合法状态转移。

设计细节：

- 状态机是生命周期预检和提交的共同约束。
- 非法转移返回 `InvalidTransition`。
- 热插拔和热替换必须围绕该状态机扩展，不能临时绕过。

### `topology.rs`

职责：

- 定义拓扑事件模型。
- 定义拓扑快照模型占位。

设计细节：

- 事件类型包括单元增加、状态变化、绑定增加、绑定移除、租约增加、租约撤销、端口增加、菜单项增加、菜单项移除和单元移除。
- 当前快照导出主要由 `snapshot.rs` 和管理通道固定布局承担。

### `tests.rs`

职责：

- 提供 host 单测。
- 覆盖模型层名称校验、契约校验、状态迁移、绑定图环检测、租约撤销、管理通道结构和 EBI 校验。

设计细节：

- 单测必须保持在 host 可运行，避免依赖内核环境。
- 模型层测试是 ELM ABI 稳定性的第一道保护。

## 15. 内核运行时模块设计：`kernel/src/elm`

`kernel/src/elm` 是内核 ELM Core 的落地层。它把 `libs/elm` 的纯模型转换为真实内核状态、系统调用、事件、日志、菜单、绑定和租约。

### `mod.rs`

职责：

- 组织 ELM 内核模块。
- 提供初始化入口。
- 导出 `with_core` 访问全局 ELM Core。

设计细节：

- 初始化时注册内建 `elm-mgr` 和内建 `eki` 子单元。
- 对外隐藏全局锁细节，减少其它模块直接接触核心状态。

### `core.rs`

职责：

- 维护 ELM Core 全局状态。
- 管理 cells、ports、runtime ports、menu items、leases、events、audits 和 binding graph。
- 实现绑定、解绑、生命周期、EBI 协议对象装载入口、运行时日志、运行时事件和 debug dump。

设计细节：

- `CellRuntime` 保存单元 ID、parent、state、kind、generation、name、EBI 架构、EBI 状态、是否含原生代码、拥有的绑定和菜单项。
- `RuntimePortBinding` 保存 binding、cell、port、lease、cursor、submitted_logs、delivered_events 和 dropped_events。
- `ProviderRuntime` 保存 port、owner、访问策略、执行器状态、是否动态、调用次数、失败次数和撤销次数。
- `ProviderRuntime` 同时保存异步队列上限、并发上限、运行中数量、异步提交数、完成数、取消数、过期数和拒绝数。
- `ElmMgrRuntime` 保存 `elm-mgr` 自身运行时状态，包括 API 注册表、API generation、事件订阅表和订阅 ID 分配器。
- `EventSubscriptionRuntime` 保存订阅 ID、owner、事件租约、游标、过滤器、投递计数和丢弃计数。
- `ProviderBackend::Kernel(kind)` 表示 ELM 自有内核 provider 执行器；当前第一个真实执行器是 `MgrActionInvoke`。
- `ProviderBackend::KernelOps(spec)` 表示由某个子系统导出的通用 provider 规格，Core 只根据规格调用 `invoke` / `on_revoke`，不解释子系统语义。
- 事件环和审计环容量当前均为 128。
- lifecycle、provider call、mixin dispatch、replace、policy、resource 和 runtime journal trace ring 容量当前均为 128；溢出时丢弃最旧记录并累计 dropped count。
- 动态 cell ID 从 100 开始，避免与内建 ID 冲突。
- 动态 port ID 从 100 开始，避免与内建端口冲突。
- 内建 `elm-mgr` ID 为 1，内建 EKI 投影单元 `eki` ID 为 2，动态单元 ID 从 100 开始分配。
- 绑定提交会先做 preflight，再分派到具体端口 attach 路径。
- `core.log@1` 创建 `RuntimePort` 写租约。
- `core.event@1` 创建 `RuntimePort` 读租约。
- `mgr.menu.item@1` 创建菜单租约和菜单项。
- `elm-mgr` 启动时会注册一个内建健康检查菜单动作，该动作不带 TODO 标志，通过 `mgr.action.invoke@1` 调用。
- 动态 provider 端口声明 `handler_symbol` 且原生镜像能定位该符号时，会创建可调用的 ELM 原生 handler 后端。
- `ProviderBackend::ElmNative(native)` 表示由 ELM 原生镜像提供的 provider handler，调用 ABI 为 `ElmNativeProviderCallV1`。
- `ProviderBackend::ElmNativeTodo` 明确标记缺少 handler 或仍等待原生执行器的 provider 边界。
- 异步 provider 队列由 `ProviderAsyncJob`、running 调用表和 `ProviderAsyncResult` 分离建模；job 表示仍等待执行的调用，running 表示已经被 worker 取走但尚未完成的调用，result 表示已完成、失败、取消或过期并等待外部领取的终态。
- 提交异步 job 时会给 binding lease 增加 active ref；job 被取消、result 被 poll、result TTL 过期或结果环淘汰时释放 active ref，因此 `PreflightUnbind` 和生命周期预检能观察到真实忙碌状态。
- 队列容量按 provider 端口计算，默认上限为 64；`Exclusive` 端口上限为 1，`Ordered` 端口上限为 32，`Shared`、`Pipeline` 和 `Broadcast` 使用默认上限。
- provider worker 从队列中取出可运行 job，复用同步 provider 后端执行逻辑，并把 `ElmReplyFrame.status != OK` 计为业务失败和 `PROVIDER_CALL_FAILED` 审计。
- 排队超时会生成 `Expired` 结果并保留到 poll 或 TTL 清理；运行中调用可被 poll 观测并可接收取消意图。ELM 原生 provider handler 受原生调用门 deadline 和 timer trap 强制退出保护；内核 provider 回调不由 ELM 抢占。运行中调用带取消意图返回时标记为 `Canceled`，否则完成时已经超过 deadline 会标记为 `Expired`。
- `ProviderRuntime::record_flags()` 是 provider 观测 flags 的唯一派生入口，`QueryProviderPorts` 和 `QueryProviderStats` 使用同一套 flags 语义。
- detach 会阻断仍有子单元、依赖者、拓展项或 busy 租约的目标单元。
- provider owner detach 会阻断仍有活跃 binding 的 provider 端口。
- owner detach 会移除 owner 持有的事件订阅记录，并通过租约撤销链路释放订阅资源。
- `register_builtin_mgr_api()` 会一次登记全部 60 个 `ElmMgrCallKind` v1 接口，descriptor ID 与 call kind 数值一致；后续显式注册的子系统 provider API 使用独立动态 ID 区间。
- VFS、device、network、IRQ、DMA、MMIO 等子系统如果需要提供可发现运行时服务，可以由各子系统自己的 `elm.rs` 导出 `ElmKernelProviderSpec`，但必须由子系统自身初始化完成后显式注册；ELM 启动期不再批量汇聚这些规格。
- `ElmKernelProviderSpec` 是当前子系统接入 `elm-mgr` 的统一规格形状，负责 API 描述、端口描述、invoke、单页 snapshot、分页 snapshot 和 revoke 回调；当前子系统回调先返回 `UNSUPPORTED`，但已经走完整 provider runtime 链路。
- `health_bytes()` 会输出 17 类结构健康记录：graph、cells、ports、providers、bindings、runtime ports、menu、events、audits、native capabilities、TODO registry、trust、projection sources、journal、resources、executions 和 sequences。
- 健康检查会交叉校验事件订阅与租约、source owner/generation、信任接受记录、journal hash chain、资源账本、执行引用和全部单调 ID，避免各运行时表之间形成悬空状态。
- `/sys/kernel/elm` 提供 16 个只读节点：`core`、`policy`、`health`、`menu`、`topology`、`ports`、`providers`、`bindings`、`events`、`audit`、`api`、`trust`、`projection-sources`、`journal`、`executions` 和 `diagnostics`。
- debug dump 会输出 cells、ports、bindings、leases、runtime ports、source、执行、fault、native capability、TODO registry 和 health 摘要，并保留后续独立主线说明。
- runtime journal 由审计路径、启动路径和镜像信任接受路径共同写入。v1 记录固定为 240 字节，包含单调 sequence、前序哈希、记录哈希以及信任记录使用的完整 rollback authority 摘要、模块摘要和 signer key 摘要。`kernel::elm` 以 `ElmJournalBackendOps` 暴露 `capacity/read/append/sync` 四个静态回调，并要求后端在 ELM 初始化前通过 `register_journal_backend` 登记；运行时随后执行顺序回放、哈希链校验、容量检查和同步提交，回放得到的最高 release epoch 会在信任库 seal 前恢复。未登记后端时运行在可观测的易失模式；可选后端失败后降级为易失模式并停止向不确定尾部追加；强制后端失败会封闭后续变更操作。
- 当前热替换已有迁移钩子、受管 import 唯一最高版本选择与回滚、provider 后端原位切换、Projection Source 原子 generation 切换和 replace trace。v1 明确定义为排空后切换，不把跨 generation 迁移运行中调用作为隐含语义。

### `event.rs`

职责：

- 为 `sys_elm_ctl(EVENT_READ/EVENT_ACK)` 提供全局事件读取和确认辅助。

设计细节：

- 全局事件读取使用 Core 内的 acknowledged sequence。
- 运行时端口事件读取使用每个 `RuntimePortBinding` 自己的 cursor，两者语义不同。

### `executor.rs`

职责：

- 启动 ELM 后台执行器。
- 唤醒 provider 异步队列 worker。

设计细节：

- provider worker 在 `elm-mgr` 初始化成功后启动。
- worker 只负责等待、唤醒和循环驱动 `ElmCore::run_one_async_provider_job_at`，不直接修改队列内部结构。
- worker 在空队列时睡眠在 `WaitQueue` 上，`SubmitProviderCall` 成功入队后唤醒。
- 队列状态、租约 active ref、结果 TTL、审计和统计全部仍由 `core.rs` 维护，避免后台线程复制策略。

### `menu.rs`

职责：

- 管理 `elm-mgr` 菜单运行时对象。
- 把菜单项编码为固定布局快照。

设计细节：

- `MenuItemRuntime` 保存运行时字符串和 owner/action。
- 快照导出时转换为 `ElmMenuItemSnapshot` 固定数组。
- 菜单 generation 用于外部工具判断菜单变化。

### `mgr_channel.rs`

职责：

- 分发 `MGR_CALL`。
- 解析固定布局请求。
- 封装固定布局响应。

设计细节：

- 所有请求解析都显式检查 payload 长度。
- 不接受长度不匹配的输入。
- `LoadCell` 当前接收 EBI Source 请求；外部通道只接受 `Projection`，先解析固定 24 字节 `ElmProjectionSourceRequest`，再按 provider ID 调用投影器。内建 `eki` 子单元只是其中 ID 固定的一种 provider；Builtin/Memory 仅供内核内部入口使用。
- `ReplaceCell` 会解析 `ElmReplaceCellRequestV1` 和 `ElmProjectionSourceRequest`，所有已注册投影器产出的 EBI image 使用同一套热替换事务；Builtin/Memory 外部请求返回 `UNSUPPORTED`。
- 运行时日志和事件命令会校验 binding 是否存在、端口是否匹配、租约和状态是否允许。
- provider 注册命令会校验 owner、契约、方向、模式、访问策略和保留 flags。
- provider 调用命令会校验 binding、租约、端口可调用性和 payload 长度。
- provider transport 错误通过 `MGR_CALL` response status 返回；provider 业务结果通过 `ElmReplyFrame.status` 和 reply payload 返回，业务失败会计入 provider failed_calls 并写入 `PROVIDER_CALL_FAILED` 审计。
- provider 异步提交命令会校验 binding、租约、端口可调用性、后端状态和队列容量；成功提交后返回 ticket，并唤醒 provider worker。
- provider 异步轮询命令会先清理过期结果和超时 job；终态结果被领取后立即释放租约活跃引用。
- provider 异步取消命令会立即取消仍在队列中的 job；对运行中调用会记录取消意图并返回 `Running + BUSY`。ELM 原生 handler 仍受提交时 timeout 和 timer trap 强制退出约束，但 cancel 不会缩短既定 deadline；任意内核 provider 回调不提供异步强杀语义。
- provider 队列查询命令返回所有 provider 的队列深度、运行中数量、保留结果数、容量、并发上限和累计统计。
- API 注册表查询命令返回所有 `elm-mgr` 可发现 API 的固定布局描述。
- 事件订阅命令会为订阅创建 `EventSubscription` 租约；退订会撤销租约并移除订阅记录。
- 订阅读取命令支持 `ADVANCE` flag：不设置时只读不推进订阅游标，设置时读取后推进订阅游标。

### `ports.rs`

职责：

- 定义内核中的端口运行时描述包装。

设计细节：

- `PortRuntime` 保存端口 ID、owner、自有合约字符串、方向、模式、访问策略、可调用标记和实现状态。
- provider registry 当前在 ELM Core 中维护，端口运行时负责提供契约、方向、模式、访问策略和可调用标记。
- 后续完整端口提供者需要在这里或子模块中挂接真实执行器表、撤销回调、静默化回调和统计接口。

### `snapshot.rs`

职责：

- 导出 ELM 运行拓扑快照。

设计细节：

- 输出顺序为 header、cell entries、port entries。
- 快照只包含固定布局结构。
- 快照记录 EBI 状态和 native_code 标记，方便外部工具区分纯声明单元和等待原生装载器的单元。

### `syscall.rs`

职责：

- 把内核系统调用参数转换为 ELM 控制面命令。
- 执行用户缓冲区复制、权限检查、输入长度检查和输出长度检查。

设计细节：

- `MgrCall` 和 `DebugDump` 需要 `SysAdmin`。
- `read_input_bytes` 拒绝超过上限的管理输入。
- `write_bytes` 在输出缓冲区不足时返回 `EMSGSIZE`。
- 固定布局结构通过只读字节视图复制给用户态，调用点必须保证结构不包含内核指针。

### `general/src/vfs/sysfs.rs`

职责：

- 提供 `/sys/kernel/elm` 只读观测目录。
- 通过 `register_elm_renderer(fn(&str) -> String)` 注册 ELM 文本渲染回调，避免 `general` 反向依赖 `kernel`。

设计细节：

- `/sys/kernel/elm` 当前包含 `core`、`policy`、`health`、`menu`、`topology`、`ports`、`providers`、`bindings`、`events`、`audit`、`api`、`trust`、`projection-sources`、`journal`、`executions` 和 `diagnostics`。
- sysfs 只承担观测，不承担控制；所有写入、加载、绑定、订阅和卸载仍必须走 `sys_elm_ctl(MGR_CALL)`。
- sysfs 输出是文本诊断面，不替代固定布局 ABI；外部工具需要稳定机器解析时仍应使用 `MGR_CALL`。

## 16. 观测、审计与调试

ELM 的可观测性由四条路径组成：

- Core query：快速获取 Core 能力和数量。
- Snapshot：获取 cells 和 ports 的固定布局快照。
- Events：按序列读取拓扑变化。
- Audit：读取管理操作审计环。
- Sysfs：通过 `/sys/kernel/elm/*` 读取只读文本诊断快照。

Cell snapshot 当前不仅包含单元 ID、父单元、状态、类型、generation 和名称，还包含 EBI Source 类型、生命周期钩子/执行状态、原生段/import/export 数量、native fault 数、软隔离状态、隔离 blocker、资源预算和当前资源占用。该快照是 `/bin/elmctl snapshot` 和后续用户态管理器判断单元是否可热插拔、可替换、可恢复的稳定观测入口。

审计记录覆盖：

- 生命周期操作。
- 绑定和解绑。
- 运行时日志提交。
- 运行时事件读取和确认。
- 装载被缺失、未知 Projection provider 或非法 EBI Source 边界阻断。
- 热替换预检阻断、Projection provider 投影失败、Builtin/Memory 外部请求拒绝和生命周期钩子失败。

运行时端口统计覆盖：

- binding ID。
- cell ID。
- port ID。
- lease ID。
- event cursor。
- submitted logs。
- delivered events。
- dropped events。

`/sys/kernel/elm` 当前观测文件：

- `core`：初始化状态、对象数量、订阅数量、API 数量和健康摘要。
- `policy`：策略能力、支持动作、阻断位和审计容量。
- `health`：结构化健康检查文本版本。
- `menu`：`elm-mgr` 菜单项。
- `topology`：父子、依赖、拓展点和拓展项。
- `ports`：枢纽端口。
- `providers`：provider 后端、统计和异步队列摘要。
- `bindings`：能力绑定。
- `events`：事件环和事件订阅。
- `audit`：管理审计环。
- `api`：`elm-mgr` API 注册表。
- `trust`：信任根、撤销和 release epoch 状态。
- `projection-sources`：投影器 owner、generation、引用、暂停和退役状态。
- `journal`：运行时 journal 与持久后端状态。
- `executions`：单元执行、provider 调用和忙碌引用状态。
- `diagnostics`：fault、native capabilities、TODO registry、trace、资源和健康摘要的统一诊断视图。

这套观测机制的目标是让外部工具不需要解析内核日志，也能判断当前 ELM 拓扑、策略阻断原因和端口运行状态。需要稳定机器解析或执行控制动作时，仍然必须使用固定布局的 `sys_elm_ctl(MGR_CALL)`；sysfs 不提供写入口。

## 17. 当前实现进度

已完成：

- `libs/elm` 已提供纯模型层，包括清单、状态机、绑定图、资源租约、事件、快照、菜单和管理通道固定布局。
- 内核启动时已注册内建 `elm-mgr` 和内建 `eki` 子单元。
- 内核启动拓扑只包含根管理单元和内建端口，不再注入演示单元。
- `sys_elm_ctl` 已支持 `CORE_QUERY`、`SNAPSHOT_READ`、`EVENT_READ`、`EVENT_ACK`、`MGR_CALL` 和 `DEBUG_DUMP`。
- `MGR_CALL(QueryMenu)` 已返回固定布局的菜单快照。
- `MGR_CALL(QueryPolicy)` 已返回当前单元管理器策略能力、支持动作、阻断位和审计容量。
- `MGR_CALL(QueryTopology)` 已返回父子、依赖、拓展点和拓展项组成的关系快照。
- `MGR_CALL(PreflightLifecycle)` 已支持暂停、恢复、脱离和替换的策略预检。
- `MGR_CALL(QueryAudit)` 已返回管理操作审计环，包括动作、状态、阻断位和最终状态。
- `MGR_CALL(QueryNexusBindings)` 已返回枢纽连接层绑定快照。
- `MGR_CALL(PreflightBind/CommitBind)` 已支持内建菜单端口 `mgr.menu.item@1` 的绑定预检和提交。
- `core.log@1` 和 `core.event@1` 已支持真实绑定、租约登记、查询快照和撤销。
- `MGR_CALL(SubmitRuntimeLog)` 已支持通过 `core.log@1` 绑定提交固定长度日志 payload。
- `MGR_CALL(ReadRuntimeEvent/AckRuntimeEvent)` 已支持通过 `core.event@1` 绑定按游标读取和确认 ELM 事件。
- `MGR_CALL(QueryRuntimePorts)` 已返回运行时端口绑定统计，包括日志提交数、事件投递数和丢弃事件数。
- `MGR_CALL(RegisterProviderPort/UnregisterProviderPort)` 已支持动态 provider 端口声明注册和注销；动态端口合约由运行时自有字符串保存，不再泄漏为静态字符串。
- `MGR_CALL(QueryProviderPorts/QueryProviderStats)` 已返回 provider 端口、访问策略、绑定数量、调用次数、失败次数、撤销次数和 provider 观测 flags。
- `MGR_CALL(QueryProviderSnapshot)` 已接入 `ElmKernelProviderSpec::snapshot`、`snapshot_paged` 和 ELM 原生 snapshot 回调；显式注册的子系统 provider 可通过该路径返回自身 payload，没有 snapshot 回调的 provider 会通过固定 header 返回 `UNSUPPORTED`，ELM 原生 provider 当前不提供 snapshot 时返回 `UNSUPPORTED`。
- `MGR_CALL(InvokeProvider)` 已保留 256 字节 `ElmCallFrame` / `ElmReplyFrame` ABI；`mgr.action.invoke@1` 已接入 kernel-backed 执行器，管理通道已覆盖健康动作调用和业务失败审计，带 `handler_symbol` 的动态 ELM 原生 provider 会进入 `ElmNativeProviderCallV1` 调用边界。
- `MGR_CALL(SubmitProviderCall/PollProviderReply/CancelProviderCall/QueryProviderQueue)` 已形成真实异步队列闭环，支持 ticket、队列容量、运行中观测、超时、结果 TTL、排队取消、运行中取消意图、队列统计和租约 active ref 保护。
- `MGR_CALL(QueryApiRegistry)` 已返回 `elm-mgr` API 注册表，内建部分严格覆盖 `ElmMgrCallKind` 1..60 且 descriptor ID 与 call kind 一致；显式注册的 provider specs 追加到独立动态 ID 区间。
- `MGR_CALL(QueryTodoRegistry)` 已返回统一 TODO registry，静态项只保留 soyo/其他格式 Projection Source 与外部 Rust ELM 的调试、依赖锁定和发布生态；pending native EBI、缺少 handler 的 provider 和资源超限状态会作为动态记录出现。同步 fault、panic 和 timer 强制退出边界不再登记为 TODO。
- `MGR_CALL(QueryExtensions/PreflightExtensionAttach/CommitExtensionAttach/CommitExtensionDetach/DispatchExtension)` 已提供 mixin 管理面固定布局 ABI：拓展点声明 `Chain`、`Observer` 或 `Exclusive` 组合模式，拓展边声明独立 `handler_contract` 和有符号优先级。`Chain` 按“优先级降序、单元 ID 升序”稳定执行，并允许 `REPLACE`、`STOP` 和 `DENY`；mixin provider 调用边只放行 `ELM_MIXIN_REPLY_FLAGS_MASK` 中定义的控制位，普通 provider 仍严格要求 reply flags 为 0。`Observer` 向全部观察者传递同一份原始 payload，拒绝任何控制型 reply flag，单个观察者失败不会截断其余观察者；`Exclusive` 在图校验和挂接预检阶段强制最多一个拓展项。dispatch 优先按 `handler_contract` 精确选择 extension cell 拥有的可调用 provider，仅在该单元恰好只有一个可调用 provider 时允许无歧义回退，并通过临时租约和合成调用边执行。mixin 因此具备对显式 patch point 的运行时补丁能力，但不提供任意地址内存改写。
- `MGR_CALL(SubscribeEvent/UnsubscribeEvent/QueryEventSubscriptions/ReadSubscribedEvents)` 已形成事件订阅闭环，支持订阅租约、独立游标、过滤器、只读不推进和读取后推进两种模式。
- `/sys/kernel/elm` 已提供 16 个只读文本观测节点，并把 fault、native capability、TODO 和 trace 汇总到 `diagnostics`，同时单独公开 trust、projection sources、journal 与 executions。
- `MGR_CALL(QueryHealth)` 已返回 17 类结构化 Core 健康记录，可定位 graph、cell、port、provider、binding、runtime port、menu、event、audit、native capability、TODO registry、trust、source、journal、resource、execution 和 sequence 不变量破坏。
- `MGR_CALL` 管理通道已收口输入上限、请求头构造器、保留位零值策略和无 payload 查询命令校验；格式错误统一返回 `INVALID`，未知命令号返回 `UNSUPPORTED`。
- 动态 provider 端口已支持 `Public`、`ExtensionOnly` 和 `Internal` 三类访问策略。
- 缺少 handler 的动态 provider 端口会被预检阻断为 `PORT_TODO`，已纳入 provider busy、注销和观测链路；带原生 handler 的端口可以正常绑定和调用。
- `MGR_CALL(PreflightUnbind/CommitUnbind)` 已支持动态能力绑定的预检和撤销；内建保护绑定不可撤销。
- 拓展快照会同时返回模式、优先级、拓展点契约和 handler 契约；dispatch 响应回显实际组合模式及 matched/called 数量。provider 缺失时返回 `UNSUPPORTED`，handler 调用已经发生但其返回值被策略拒绝时仍计入 called 数量，使审计与实际执行一致。
- 绑定图已记录真实能力绑定边，并将绑定、菜单租约和菜单项纳入同一条撤销链路。
- EBI 已重构为稳定装载协议对象，包括目标架构、ABI 版本、清单、菜单声明、段声明、声明式拓扑、统一模块描述符定位、imports/exports 和完整 Rust ABI 摘要。
- `MGR_CALL(LoadCell)` 已接收 EBI Source 请求；外部输入必须通过 Projection Source，内建 EKI 投影器只是 provider ID `0x454b_4900_0000_0001` 对应的一种实现，并拒绝旧裸 EBI 字节格式。
- 动态原生 ELM 已强制要求唯一 `ElmModuleDescriptorV1`；`#[elm::module]` 统一生成生命周期、迁移和 entry trampoline，旧式独立生命周期 attribute 不再存在。
- 内建 EKI Projection Source 已支持依赖、拓展点、拓展项、provider port、原生 payload segment、imports、exports、Rust ABI 摘要、符号位置和 EKI relocation v1；EKI v1 的拓展点记录显式编码组合模式，拓展项记录显式编码 handler 契约和优先级，装载时会完成保留位、固定记录长度、枚举值和字符串边界校验，再展开为 EBI image。
- 带 Code payload、统一模块描述符位置和可解析 relocation 的原生 EKI 会进入原生镜像执行器；执行器完成段复制、重定位、W^X 权限封口、指令缓存同步和描述符校验后调用 `ElmModule::initialize`，激活声明式拓扑，并在进入 `Active` 后调用描述符 entry。
- 无 Code/entry/relocation 的声明式 EBI 会由运行时默认生命周期闭合并直接进入 `Active`；带 Code/entry/relocation 但缺少架构映射 ops 的环境会登记为 `Loaded + NativeCodeTodo`，不执行代码。
- 外部 no_std Rust ELM 使用同名 `allocator` 接口 crate 提供的 `KERNEL_ALLOCATOR` 作为全局分配器，可以直接使用 `Box`、`Vec`、`String` 和 `Arc`。四个 `GlobalAlloc` 入口在装载期绑定到常驻内核全局分配器；分配热路径通过当前 ELM 执行上下文自动计入 cell 预算，不需要 namespace、函数表或授权 token。
- `elm` 已提供独立的 module 编译面和 attribute 开发框架：`#[elm::module]` 注册唯一 `ElmModule` 实现；provider、export 和 mixin attribute 可以标记该实现的方法；`#[elm::import]`、`#[elm::kernel_symbol]` 和 `#[elm::payload]` 声明显式镜像协议。普通 allocator 与设备代码直接使用原 crate 的类型、trait、方法和静态对象，不需要再用设备专用 attribute 重写一套模型。
- `tools/elm-tools build` 已能把外部 Rust 仓库固定构建为 ELF64 little-endian PIE，再转换为带 header SHA-256 的 EKI：它会抽取页对齐连续 `PT_LOAD` 段、统一模块描述符、native import/export、Rust ABI 摘要、provider、mixin 和 payload 元数据，并把 `R_RISCV_RELATIVE` / `R_LARCH_RELATIVE` 转换为 EBI `ImageBase64` 重定位。非 `ET_DYN`、text relocation、未知动态重定位或不兼容布局会在打包期被拒绝。
- `DirectImport<F>` 和 `direct-pinned` export 已按规范函数签名生成完整 SHA-256，并在 ELM 间解析时要求名称、契约、版本和摘要全部匹配。`#[elm::kernel_symbol]` 使用同一类型化槽协议；常驻目录已实现名称、契约、最高兼容版本、ABI 摘要、接口指纹和能力策略校验，可选导入允许空槽，必需导入失败会在执行任何模块代码前终止装载。
- `elm-tools new`、`sync-framework`、`build`、`sign` 和 `verify` 已形成独立仓库闭环；同一 hello ELM 已在 RISC-V64 与 LoongArch64 QEMU 中通过签名装载，生命周期日志、`Active` 状态、信任 epoch、故障计数和 Core health 均验证通过。
- Core 已记录动态单元生命周期声明、测试执行器可用性、初始化状态、完成卸载状态、pending EBI load 计划、EBI Source kind、资源预算、native fault 次数和软隔离状态；启动期 `elm-mgr` 作为 Builtin Source 根单元进入同一套 EBI 模型，启动期 `eki` 作为 Builtin Source 子单元挂在 `elm-mgr` 下。
- `ElmContext` 已作为 Rust 生命周期上下文进入模型层，当前不进入 `sys_elm_ctl` 字节 ABI。
- kernel-tests 已具备受控生命周期测试执行器：可验证无原生镜像单元初始化后激活菜单、拓扑和 provider，初始化失败隔离，卸载前 finalize，以及 finalize 失败保留资源。
- 受控测试执行器不会激活带原生 entry、代码段或重定位段的单元；带代码段的单元只能由原生 EKI 执行器处理，带 `entry` 的 Code EKI 已由原生执行器负责入口调用。
- `PauseCell` 和 `ResumeCell` 已通过统一预检策略支持动态单元的真实状态切换；原生单元会调用 `ElmModule::quiesce`、`pause` 和 `resume`，暂停后的单元不能继续发起或承载 provider 调用。
- `DetachCell` 已通过统一预检策略支持动态单元的资源租约撤销、菜单项移除、绑定图摘除和退役；尚未激活的原生 TODO 单元可作为元数据直接摘除。
- `DetachCell` 会阻断仍有子单元、依赖者、拓展项、忙碌租约、provider 队列/保留结果/运行中调用或 native export 被其他单元 import 的目标单元，避免破坏当前拓扑。
- `ReplaceCell` 已支持所有已注册 Projection Source 的热替换入口：声明式 image 可直接提交 generation 和元数据更新；原生 image 支持迁移式事务，包括新镜像影子装载、初始化期 import 暂存、新 `initialize`、旧 `quiesce`、旧 `migrate_export`、新 `migrate_import`、暂存 import 提升、旧 `finalize`、失败 abort/finalize、旧代恢复、source 回滚和 generation 提交；Builtin/Memory 外部请求不进入替换事务。
- 直接符号链路已经使用独立仓库 ELM 在 RISC-V64 与 LoongArch64 QEMU 中完成验证：镜像能够解析并调用 allocator 分配、释放、扩容、查询以及 `general::dev` 查询符号，随后完成 initialize、健康快照、finalize 和 detach。启动期 `kernel-tests` 同时覆盖目录校验、能力拒绝、可选槽、事务回滚和设备资源归属。

## 18. 运行期 smoke 测试链路

当前已经具备从用户态进入 `elm-mgr` 的完整运行链路：

```text
用户态 /bin/elmctl 或 /bin/elmctl-smoke
    -> syscall(SYS_ELM_CTL = 509)
        -> ElmCtlCommand::MgrCall
            -> elm-mgr 管理通道
                -> ELM Core
                    -> 菜单、策略、健康检查、TODO registry、API 注册表、事件订阅、能力绑定、provider 调用、审计、Projection Source 装载边界
```

`elm-mgr` 本身是启动期内建 ELM。它在 `sched::boot_init()` 之后、用户态 init 进程启动之前完成初始化，地位类似用户态的 init 进程：后续所有动态 ELM 都位于 `elm-mgr` 管理树中，但可以通过装载请求挂到任意合法动态父单元之下；所有外部管理工具都通过 `elm-mgr` 暴露的控制面进入 ELM Core。`elm-mgr` 的来源固定显示为 `<builtin>`，不会显示为 EKI、soyo、ELF 或其他镜像类型。内建 `eki` 是 `elm-mgr` 的子 ELM，来源同样显示为 `<builtin>`，它只负责提供 EKI 投影能力。未来 VFS、调度、设备等常驻子系统如果需要对外提供可发现、可绑定、可审计的运行时服务，应注册为枢纽连接层端口；计划迁出的网络栈由对应 ELM 自己发布端口。普通稳定 Rust API 则由常驻子系统登记为内核直接符号，ELM 通过同名接口 crate 调用真实实现，不为每个子系统新增私有 syscall，也不让 `elm-mgr` 成为数据热路径。

仓库提供两个用户态入口：

- `userland/elmctl/`：正式管理工具骨架，构建后安装为 `/bin/elmctl`。它使用共享 C ABI 头和 client helper，支持 Core query、snapshot、全局事件、debug dump、所有 `elm-mgr` 查询命令、EKI 装载/替换、生命周期操作、绑定/解绑、runtime log/event、provider 注册/注销/调用、异步 provider、provider snapshot 和事件订阅。管理器自有查询表会结构化解码；具体子系统 provider 的业务 payload 保持十六进制输出，由对应子系统协议解释。
- `userland/elmctl-smoke/elmctl_smoke.c`：运行期验收工具，构建后安装为 `/bin/elmctl-smoke`。它不依赖 Linux 模块 ABI，不使用 ioctl，不读取 `/proc` 或 `/sys`，只直接调用私有系统调用 `SYS_ELM_CTL` 并执行固定验收步骤。
- `tools/elm-tools/`：宿主侧 EKI 与 Rust ELM 工程工具，不加入主 workspace。当前支持 `new` 创建独立仓库、`sync-framework` 同步 `elm`、`kernel-symbols`、同名 `allocator` 和 `general` 接口 crate、`build` 执行双架构 PIE 构建/打包/签名，以及 `pack-metadata`、`pack-elf`、`inspect`、`hash`、`keygen`、`sign`、`verify`。由于容器默认 Cargo target 是内核 bare-metal 目标，构建该工具时必须显式使用 host target，例如 `cargo run --manifest-path tools/elm-tools/Cargo.toml --target x86_64-unknown-linux-gnu -- verify demo.eki`。

### elmapi v1 与单一 API 根

`elmapi` 只承载 ELM 运行时自身的上下文、日志、终止、受管 ELM 间调用、mixin 分发和管理能力，不承载 allocator、设备等内核子系统 API。稳定路径是：

1. EBI unit 在 `api_compatibility` 中声明最多 16 个按升序排列的兼容 `elmapi` 版本、必需特性位和 root import index。
2. root import 必须是 `elm.api.root` / `elm.api.root@1` / wildcard version，并且必须由唯一一个 `ImportAbs64` relocation 写入可写 Data/Bss 槽位。Core 拒绝把根指针直接重定位进代码段或只读段。
3. Core 计算模块兼容集与运行时支持集的最高公共版本；首次稳定版本为 `elmapi v1`。必需特性不满足时，装载在执行任何模块代码前失败。
4. 根表只提供普通运行时表和按 identifier 查询命名空间表的入口。普通运行时表包含专用 mixin 分发、当前上下文、日志、终止和受管 import 调用，不暴露可传任意命令号的通用管理分发函数。
5. 受授权 Manager 通过 identifier `elm.management` 取得独立的 `ElmManagementApiV1`。`elm::management::Client` 在取得表时校验版本、尺寸、切换代和 capability，并在每次调用后校验管理响应头、固定回复或分页回复；内核在每次分发时再次校验当前单元种类、状态、切换代和 management capability。
6. 普通 ELM 使用 `elm::runtime::*`；Manager ELM 只有在工程启用 `management` feature 且镜像获得显式 management capability 后才能使用 `elm::management::*`。`elm::mgr`、`elm::elmmgr`、独立 `elmmgr` crate 和裸管理 dispatch 均不是 v1 API 的一部分。
7. management capability 不从父单元自动继承，不能通过 `UpdateCellPolicy` 自行添加或移除。授予请求只接受 Kernel、UserAdmin 或内建 `elm-mgr`，目标镜像必须声明 `ElmKind::Manager`、通过完整签名信任验证，并挂在已经持有 management capability 的父单元下；热替换同样保持签名要求。

### 内核直接符号目录

内核子系统 API 使用独立于 `elmapi` 的直接符号协议：

1. 中立 crate `libs/kernel-symbols` 定义描述符、能力组和 `#[kernel_symbols::export]`。描述符可以登记经过审核的自由函数、固有方法和非 `static mut` 静态对象；方法接收者是 Rust ABI 的正式组成部分，静态对象则按其真实地址参与重定位。泛型、async、const 和显式外部 ABI 仍不进入当前稳定目录。
2. `elm-tools export-interface` 从目标架构实际生成的 `allocator`、`general` `.rlib/.rmeta` 提取精确 crate metadata、依赖闭包和 Rust 链接符号。外部工程中的同名 façade 在正式 bare-metal 构建中只 `pub use` 这些 metadata 暴露的真实类型与方法，不保存手写类型副本或第二份运行状态。接口包另外附带与接口摘要绑定的只读 LSP 源码投影，使 rust-analyzer 能定位真实模块、类型、方法和文档。ELM 工程默认启用只用于分析的 `elm-lsp` feature，把 façade 后端切换到源码投影，因此编辑器即使使用 `target_os = "none"` 的目标也能建立完整定义图；正式 `elm-tools build` 传入 `--no-default-features`，把 façade 后端切回目标专属 metadata，并且不会编译源码投影。
3. 普通源码调用由 rustc 产生真实 Rust 符号引用；打包器依据目标接口清单把稳定链接名和审核过的 mangled alias 统一转换为 EBI `kernel-symbol` import。导出工具会重新从源码计算对应公开方法 ABI，接收者或参数不一致时直接拒绝生成接口包。`#[elm::kernel_symbol]` 只保留给确实需要手工声明固定槽的底层场景。
4. 镜像 ABI 指纹绑定目标架构、rustc、target spec、panic 策略、`elmapi`、内核符号描述符 ABI，以及真实 `allocator/general` 接口源码的规范 SHA-256。单个符号再按名称、契约、版本和规范 Rust ABI 摘要匹配；接口包同时记录目标内核摘要、源文件数量、精确 metadata 文件和实际链接别名。
5. 装载器先验证链接目录结构和三元身份唯一性，再按父 cell 的 `kernel_symbol_capabilities` 上限解析每个导入。`CORE_SAFE`、普通 allocator 内存和 allocator 诊断属于默认安全组；物理内存、allocator 管理以及所有设备能力属于特权组，外部镜像必须经过签名验证并由合法 authority 显式批准。能力判定只发生在地址提交前，不存在可复用 grant 或每次调用 token。
6. 地址写入模块可写导入槽并完成重定位后，业务调用直接进入常驻 Rust 实现。`elm-mgr`、namespace 查询、provider dispatch 和管理 syscall 都不在数据路径上；运行时只继续提供当前 cell 上下文、故障边界、预算计量和长期资源归属钩子。
7. 热替换会为新镜像重新解析全部直接符号并重新执行能力裁决，失败时不改变旧 generation。内核符号本身常驻且不按 ELM generation 路由；ELM 间的 `direct-pinned` import 仍按原有 generation 规则处理，二者不能混淆。

`allocator` 接口 crate 直接实现与内核同名的 `KernelMemorySubsystem` 和 `KERNEL_ALLOCATOR`。普通 `GlobalAlloc`、分配查询、物理页和地址转换入口分别映射到能力组；当前 ELM 调用门通过 allocator 的中立计量回调自动识别 owner、执行预算预留/调整/释放。接口 crate 自身不包含 allocator 算法或第二份堆状态。

固定线协议仍禁止直接放入 `Vec`、`String`、trait object 或 Rust 引用，因此 provider、受管 export、mixin payload 和迁移数据必须使用稳定编码。内核直接符号属于同构 Rust ABI，可以按经审核的真实签名传递 `Arc`、trait object 和其它 Rust 类型；其安全前提是完整接口指纹一致，并且长期对象必须在 finalize/detach 前释放或由常驻子系统登记到资源归属链。

### Rust ELM 独立仓库开发框架

外部 ELM 不维护函数表式第二套 API，也不从原内核仓库建立 path dependency。`elm-tools sync-framework` 同步 `libs/elm`、中立 `kernel-symbols`、两个 metadata façade，以及接口包提供的 `.elm/kernel-source` LSP 源码投影；目标专属的真实 `allocator/general` metadata、Rust 单态化支持归档和审核导入库来自 `export-interface` 产物。普通开发代码通过 `elm::runtime::*` 使用 ELM 自身运行时能力，直接按 `allocator::*`、`general::dev::*` 路径调用内核子系统，并通过 `elm::*` 使用固定帧、载荷、import/export 和 attribute；Manager 工程额外启用 `management` feature。源码投影保留发布时的真实模块层级和定义位置，只用于补全、诊断、悬停和跳转；其内部 package 统一使用 `elm-lsp-*` 身份以免与正式 façade 冲突。根 ELM 是独立 Cargo package，`.elm/framework` 与 `.elm/kernel-source` 分别是嵌套 workspace，因此 rust-analyzer 不会把全部内核投影误判为根工程成员；正式构建仍严格使用目标接口包中的二进制 metadata，不编译投影中的子系统实现。

创建一个独立仓库：

```sh
cargo run --manifest-path tools/elm-tools/Cargo.toml --target x86_64-unknown-linux-gnu -- \
  new build/demo-hello \
  --name demo.hello \
  --kind service \
  --source local.demo
```

生成结果包含：

- `Cargo.toml`：独立 package，依赖同步后的 `elm`、`allocator` 与 `general`；普通工程为 `elm` 启用 `module, macros`，Manager 工程额外启用 `management`。默认 `elm-lsp` feature 只把 `allocator/general` façade 接到源码投影，正式构建关闭根 package 的默认 feature；release 与分析使用的 dev profile 都固定 `panic = "abort"`。
- `Elm.toml`：名称、版本、种类、来源 identifier、菜单和单元依赖的唯一声明源。
- `src/main.rs`：`no_std + no_main` 生命周期模板；链接同名 `allocator` 后直接使用内核全局分配器，并用 `Vec`、`String`、`Box` 和 `Arc` 验证 Rust 堆；示例对象在初始化钩子结束前全部析构。
- `elm.ld`：固定的连续 `PT_LOAD`、非装载 `.elm.meta` 和动态重定位布局。
- `.cargo/config.toml`：RISC-V64 与 LoongArch64 的 PIE、small code model 和 linker 配置。
- `.elm/framework`：与内核源码解耦的独立嵌套 workspace，包含 `elm`、`kernel-symbols`、`allocator` 和 `general` 接口 crate；更新工具后使用 `sync-framework` 显式同步。
- `.elm/kernel-source`：与接口 SHA-256 绑定的独立嵌套 workspace，只用于 LSP 源码投影。rust-analyzer 通过根 package 默认启用的 `elm-lsp` feature 分析它，实际 ELM 目标构建关闭该 feature，因此该目录不会进入代码生成和链接。

最小模块代码不手写任何 EBI ABI：

```rust
#![no_std]
#![no_main]

use elm::{ElmModule, HookError, HookResult, LifecycleContext};

struct Demo;

#[elm::module]
impl ElmModule for Demo {
    fn create(_context: &LifecycleContext) -> Result<Self, HookError> {
        Ok(Self)
    }

    fn initialize(&mut self, _context: &LifecycleContext) -> HookResult {
        elm::runtime::log(6, "demo.hello: initialized")
            .map_err(|_| HookError::new(-1))
    }

    fn finalize(&mut self, _context: &LifecycleContext) -> HookResult {
        elm::runtime::log(6, "demo.hello: finalized")
            .map_err(|_| HookError::new(-1))
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {
    elm::runtime::abort_panic()
}
```

`kind = "manager"` 的工程会由 `elm-tools` 自动启用 `management` feature。管理型 ELM
通过类型化客户端取得管理命名空间，不接触裸命令号或裸函数指针：

```rust
let manager = elm::management::Client::acquire()
    .map_err(|_| elm::HookError::new(-1))?;
let policy = manager
    .query_policy()
    .map_err(|_| elm::HookError::new(-1))?;
```

普通 ELM 不启用该 feature；即使手工启用，内核也会在命名空间取得和每次调用时同时
校验 `ElmKind::Manager`、当前切换代、运行状态和显式 management capability。

当前 attribute 语义已经固定为 v1：

- `#[elm::module]` 必须且只能标记一个 `impl ElmModule for T`。`create`、`initialize` 和 `finalize` 是必需 trait 方法；`quiesce`、`pause`、`resume`、迁移方法和 `entry` 是可覆盖的 trait 方法，不再使用独立 attribute。
- provider、provider snapshot、export、设备回调和 mixin attribute 可以直接标记 `ElmModule` 实现中的 `&self` 方法；宏通过当前 generation 的 `ModuleSlot<T>` 调用唯一活动实例。
- `#[elm::import(...)]` 作用于 `ManagedImport` 或 `DirectImport<F>` 静态槽；`ManagedImport` 负责固定帧、代际路由和回复校验，`DirectImport<F>` 只在 ABI 摘要完全匹配并固定目标 generation 后返回类型化函数指针。
- `#[elm::kernel_symbol(...)]` 声明由内核直接符号目录解析的 `DirectImport<F>`。宏验证显式 ABI 字符串与函数指针类型一致；打包器裁剪未链接槽，装载器完成目录、接口指纹、能力和地址校验后才允许模块进入生命周期入口。
- `#[elm::export(...)]` 在 `managed` 模式生成固定调用帧 trampoline，在 `direct-pinned` 模式导出真实 Rust 函数并写入规范签名摘要；工具拒绝名称、符号、模式或摘要不一致的元数据。
- `#[elm::payload("contract@version")]` 为具名字段结构体生成固定小端线编码，只接受定宽整数、`bool` 和 `[u8; N]`，v1 总尺寸不得超过 256 字节。
- `#[elm::mixin_point(..., stages(...))]` 把普通安全 Rust 函数包装为 ingress、substitute、egress、observe 补缀点；`#[elm::mixin(...)]` 生成对应 provider 和拓展声明，支持完整有符号优先级。
- 宏在编译期拒绝手写 `extern`、`unsafe fn`、泛型 ABI 函数、非法契约、无效版本范围、重复 stage、超长补缀点和越界优先级；打包器再次执行独立的元数据与 EBI 校验。

attribute 生成的记录位于非装载段 `.elm.meta`。该段使用 `ELMMETA1` 固定记录、字段排序、CRC32 和零填充规则，不能进入任何 `PT_LOAD`；`elm-tools` 只读取该协议，不依赖 Rust 符号修饰规则，也不会从函数名猜测拓扑。

构建签名镜像：

```sh
cargo run --manifest-path tools/elm-tools/Cargo.toml --target x86_64-unknown-linux-gnu -- \
  build build/demo-hello \
  --arch all \
  --key build/proof-demo.seed \
  --epoch 1
```

只做本地不可信测试时可以显式使用 `--unsigned`；`--unsigned` 不能与 `--key` 或 `--epoch` 混用。生产构建必须提供 32 字节 Ed25519 seed 和非零 release epoch。输出文件位于工程 `dist/`，名称为 `<elm-name>-riscv64.eki` 与 `<elm-name>-loongarch64.eki`。

`build` 的完整链路是：

```text
Elm.toml + `ElmModule`/Rust attribute 源码
    -> rustc/rust-lld PIE（ET_DYN）
    -> 统一模块描述符 + .elm.meta + PT_LOAD + .rela.dyn
    -> elm-tools 严格解析
    -> R_RISCV_RELATIVE / R_LARCH_RELATIVE 投影为 EBI ImageBase64
    -> import slot 投影为 EBI ImportAbs64
    -> EKI header/content hash
    -> 可选 Ed25519 EBI proof
    -> parse_eki_image 自校验
```

原生 ELF 必须是从虚拟地址 0 开始、页对齐且连续映射的 `ET_DYN`。打包器只接受无符号 `R_*_RELATIVE` 动态重定位，并拒绝 `ET_EXEC`、text relocation、未知动态重定位、重复目标槽、越界 addend 和不连续 `PT_LOAD`。这样 GOT、函数指针和静态表由明确的 ELF 重定位驱动，不通过扫描数据猜测指针。

已有 ELF 的低层入口仍保留为：

```sh
cargo run --manifest-path tools/elm-tools/Cargo.toml --target x86_64-unknown-linux-gnu -- \
  pack-elf <project-directory> <image.elf> <out.eki>
```

`pack-elf` 的身份、版本、来源、菜单和依赖只读取 `Elm.toml`；统一模块描述符、provider、import/export、payload 和 mixin 从 ELF 符号与 `.elm.meta` 读取。命令行不再接受手工重复声明这些信息的参数。签名与验签可以独立执行：

```sh
cargo run --manifest-path tools/elm-tools/Cargo.toml --target x86_64-unknown-linux-gnu -- \
  sign unsigned.eki signed.eki private-seed.bin local.demo 1

cargo run --manifest-path tools/elm-tools/Cargo.toml --target x86_64-unknown-linux-gnu -- \
  verify signed.eki
```

运行时通过 `/bin/elmctl load-eki <file>` 装载。仓库中的 `scripts/elm-qemu-smoke-init.sh` 可在测试构建时临时安装为 init，自动执行 trust、load、snapshot 和 health 检查；该脚本不进入正常 initramfs。统一模块描述符会在执行 `initialize` 前完成固定头、实例布局、只读段位置和全部入口地址校验。

elmapi 尚未发布，因此上述全部接口都是 v1；不存在 v2、兼容别名或旧式直接日志导入路径。未来正式发布后才允许在现有兼容版本集合上演进。

`elmctl-smoke` 会执行以下检查：

- `CoreQuery`：确认 ELM Core magic、ABI、能力位、cell 数量、内建 `elm-mgr`/`eki` 拓扑和端口数量。
- `QueryPolicy`：确认 `elm-mgr` 支持 provider invoke、健康检查、异步 provider、TODO registry、资源预算策略、lifecycle hook failed blocker 和 resource quota blocker。
- `QueryMenu`：确认内建健康检查菜单项 `elm/mgr/health` 存在，并读取其 action id。
- `QueryHealth`：确认 Core 结构化健康检查为 OK。
- `QueryTodoRegistry`：确认运行时 TODO registry 可通过用户态管理通道查询。
- `QueryNexusBindings` / `CommitBind`：复用或创建 `elm-mgr -> mgr.action.invoke@1` 能力绑定。
- `InvokeProvider`：通过 `ElmCallFrame` 调用健康检查 action provider。
- `QueryProviderPorts` / `QueryProviderSnapshot`：确认启动期只存在 ELM 自有 provider；显式注册 provider 后再验证 provider 可发现和 snapshot 路由。
- `QueryApiRegistry`：确认 `elm-mgr` API 网关至少公开当前已知管理 API、事件 API 和 provider API；子系统 provider 只在显式注册后进入 API 注册表。
- `SubscribeEvent` / `QueryEventSubscriptions`：为内建 `elm-mgr` 创建事件订阅租约，并确认订阅快照可读。
- `LoadCell`：构造一个最小 EKI payload，经固定 ID 的内建 EKI Projection Source 投影并由 `elm-mgr` 管理通道装载为 EBI 协议对象，预期进入 `Active`。
- `DetachCell`：摘除上一步已激活的 EKI 元数据单元，确认元数据路径可清理。
- `ReadSubscribedEvents` / `UnsubscribeEvent`：读取 EKI 装载和脱离产生的订阅事件，然后撤销订阅租约并确认没有订阅泄漏。
- `QueryAudit`：确认管理审计流可读。

构建方式：

```sh
docker run --rm -it -v "$PWD":/work -w /work zhouzhouyi/os-contest:20260510 bash
make kernel-rv
make kernel-la
```

`make kernel-rv` 会把 RISC-V64 静态链接版本安装到 `userland/rootfs-rv/bin/elmctl` 和 `userland/rootfs-rv/bin/elmctl-smoke`，并重新打包到 `build/initramfs-rv.cpio`。`make kernel-la` 同理安装 LoongArch64 版本到 `userland/rootfs-la/bin/elmctl` 和 `userland/rootfs-la/bin/elmctl-smoke`。

RISC-V64 手动运行方式：

```sh
qemu-system-riscv64 -machine virt -kernel kernel-rv -m 1G -nographic -smp 1 \
  -drive file=./build/sdcard-rv.img,if=none,format=raw,id=x0 \
  -device virtio-blk-device,drive=x0 -no-reboot \
  -device virtio-net-device,netdev=net0 -netdev user,id=net0 -rtc base=utc
```

启动后在 `press Ctrl+C within 3 seconds to enter shell` 窗口按 `Ctrl+C` 进入 shell，然后运行：

```sh
/bin/elmctl-smoke
```

人工诊断时可以运行：

```sh
/bin/elmctl core
/bin/elmctl snapshot
/bin/elmctl menu
/bin/elmctl policy
/bin/elmctl health
/bin/elmctl providers
/bin/elmctl api
/bin/elmctl todo
```

预期输出包含：

```text
[elm-smoke] core query ok
[elm-smoke] policy query ok
[elm-smoke] menu query ok
[elm-smoke] health query ok
[elm-smoke] todo registry ok
[elm-smoke] bind mgr action provider ok
[elm-smoke] invoke health action ok
[elm-smoke] device discovery payload ok
[elm-smoke] device discovery provider ok
[elm-smoke] vfs lookup provider ok
[elm-smoke] api registry ok
[elm-smoke] event subscribe ok
[elm-smoke] load minimal EKI ok
[elm-smoke] detach minimal EKI ok
[elm-smoke] subscribed event read ok
[elm-smoke] event unsubscribe ok
[elm-smoke] audit query ok
[elm-smoke] PASS
```

如果某一步失败，`elmctl-smoke` 会返回非零状态，并打印失败步骤、errno 或管理通道状态。该工具是当前阶段的运行期验收入口；如果它不能通过，说明 `elm-mgr` 用户态控制链路、固定布局 ABI、API 注册表、事件订阅、能力绑定、provider 调用或 EBI Source 边界发生了退化。

当前剩余主线：

- 内核直接符号导出：当前已完成 allocator 与 `general::dev` 的真实 Rust 符号、同名接口 crate、能力策略和资源归属链；后续子系统必须沿相同目录协议分批审核，不能重新引入 namespace 函数表或占位接口。计划迁出内核的网络栈不从常驻实现导出。
- 子系统 provider：设备、VFS、网络、IRQ、DMA、MMIO 等真实能力必须在各自子系统内部实现并显式注册。
- 用户态管理工具：补齐面向实际部署的策略编辑、镜像仓库、交互式诊断和运维工作流。
- ELM 调试与发布生态：补齐调试符号归档、IDE 映射、依赖锁定、可复现发布索引和镜像仓库；attribute、独立仓库模板、双架构 PIE、EKI、签名和运行期装载链路已经具备。
- 其他格式投影：soyo 或其他容器若需要承载 ELM，分别提供独立 Projection Source；该工作不修改 ELM Core。

阶段收束结论：

- 当前 ELM 已经形成稳定的管理运行时和原生执行边界：`elm-mgr` 可以作为外界入口管理菜单、策略、拓扑、审计、provider、事件订阅、资源预算、隔离状态、完整 60 项 API 注册表、信任、Projection Source、journal、镜像会话和 EBI 装载状态；EKI 由内建 `eki` 子单元以固定 Projection Source ID 接入。
- 当前阶段不再继续把零散能力堆进 `MGR_CALL`。后续新增能力必须归入子系统 provider、用户态管理工具、ELM 调试与发布生态、其他格式 Projection Source 或网络栈 ELM 化等明确主线。
- `elm-mgr` 是所有 ELM 通向运行时管理能力的唯一网关，但不是内核数据热路径。普通稳定 Rust API 通过同名接口 crate 和装载期绑定的真实内核符号直接调用；只有需要运行时发现、绑定、审计、跨单元调用或异步流控时才通过 provider Ops。不为每个子系统新增私有 syscall，也不在 `elmapi` 中堆放内核 API。
- 内核直接符号链已经进入双架构验证：独立 ELM 工程能够使用内核 `KERNEL_ALLOCATOR` 完成分配、查询、扩容和释放，并调用 `general::dev` 真实查询接口。接口源码指纹、符号 ABI、能力上限、特权授权、可选导入、装载回滚、cell 分配计量和设备资源自动撤销均由运行时校验。
- `EKI` 是近期原生 ELM 镜像承载方式，`soyo` 仍保持后置；ELM Core 继续只消费 EBI 协议对象，不绑定具体文件格式，具体镜像类型必须通过 Projection Source 进入。
- 当前带 Code payload 的原生 EKI 已接入真实镜像执行器；imports 已可通过已装载原生 exports 解析，受管 import 可选择唯一最高兼容 export 并在替换时回滚，带 `handler_symbol` 的 ELM 原生 provider 已可调用，原生 `entry` 已通过 `ElmNativeEntryFrameV1` 在激活后调用。迁移式热替换、调用排空、Projection Source 原子 generation 切换、provider snapshot 分页、资源预算、隔离、同步 fault、panic 恢复和 timer 强制退出均已进入双架构验证链路。

## 19. 后续阶段路线

第一主线：子系统 provider 接入。

- 在设备、VFS、IRQ、DMA、MMIO、块 I/O 等常驻子系统内部实现各自 `Ops`、snapshot、revoke 和协议结构；计划迁移为 ELM 的网络栈不新增内建 provider。
- 子系统初始化成功后显式注册 `ElmKernelProviderSpec`，Core 不保存子系统特殊分支。
- 以真实负载验证同步、异步、取消、配额、热插拔和审计语义。

第二主线：用户态管理工具。

- 在现有 `elmctl` 协议客户端上补齐策略、预算、信任、source、journal、trace 和批量事务工作流。
- 提供稳定的机器可读输出、镜像验签、部署记录和故障包导出。
- 保持所有控制操作走 `sys_elm_ctl(MGR_CALL)`；sysfs 始终只读。

第三主线：ELM 调试与发布生态。

- 为现有 attribute 与独立仓库框架补齐调试符号归档、源文件映射、测试 harness 和 IDE 集成。
- 固化 `elm::runtime::*`、`elm::management::*`、`kernel-symbols` 目录协议与同名接口 crate 的发布边界。
- 在现有 PIE -> EKI、签名和验签链路上补齐依赖锁定、可复现构建、发布索引与镜像仓库。

第四主线：其他格式 Projection Source。

- soyo 或其他容器分别实现独立投影器和来源证明，不向 `ElmEbiSourceKind` 增加格式枚举。
- Projection Source 必须复用现有 owner、generation、引用、暂停、原子切换、退役与健康检查语义。

第五主线：子系统 ELM 化。

- 在 provider 与开发框架稳定后，逐步把计划中的网络栈等能力迁移为 ELM。
- 迁移过程不得让 ELM Core 反向依赖网络、VFS 或具体设备实现。

## 20. 禁止事项

实现时必须避免：

- 不做 Linux 模块 ABI。
- 不做 `export_symbol`。
- 不做 `modprobe` 兼容。
- 不做 `module parameter` 兼容。
- 不把 ELM 生命周期简化为 `init/exit`。
- 不让 ELM 直接实现内核内部 trait。
- 不让 ELM 保存裸内核对象指针。
- 不把依赖解析简化为符号解析。
- 不把卸载安全简化为引用计数。
- 不把设备驱动等同于加载一个传统模块。
- 不把 EKI、soyo 或其他具体文件格式耦合进 ELM Core。
- 不把尚未接入的子系统 provider 或开发工具体验伪装成已经完整实现。

唯一允许的类比表述：

> ELM 不采用传统动态内核模块的符号链接模型，而采用枢纽连接层、流契约和资源租约模型。
