# ELM 最终设计目标：可拓展内核单元系统

## 1. 定位

ELM，全称 **Extensible Loadable Module**，中文统一称为 **可拓展内核单元**。

ELM 不是 Linux 模块 ABI 的兼容层，也不是传统动态内核模块机制。ELM 的目标是在当前内核中提供一种面向 Rust 的运行时拓展体系：内核提供可信执行底座、资源所有权、状态机、能力边界和可观测拓扑；`elm-mgr` 作为 ELM 外界接口核心、运行时和 API 网关，负责把策略、菜单、依赖选择、事件订阅、子系统 API 暴露和外部管理入口统一收口。

ELM 的基本原则：

- 不兼容 Linux `init_module`、`finit_module`、`delete_module` ABI。
- 不采用 `ko`、`modprobe`、`export_symbol`、GPL namespace 等传统模型。
- 不把 ELM 简化为“装入一段代码并调用 init/exit”。
- 不让 ELM 直接依赖内核内部 trait、裸指针或不稳定符号。
- ELM 面向 Rust 框架开发；C/C++ 兼容和许可证策略暂不进入目标。
- 开机时内核加载根管理单元 `elm-mgr`。
- 后续所有 ELM 都是 `elm-mgr` 管理树下的子单元。
- 每个动态 ELM 都必须提供 Rust 生命周期钩子 `on_initialize` 和 `on_finalize`。
- 一个 ELM 可以拓展另一个 ELM，也可以被另一个 ELM 拓展。
- ELM 之间可以同时存在父子、依赖、提供、拓展和能力绑定关系。
- ELM 支持热插拔、热替换、暂停、恢复、故障隔离和回滚。

当前设计走向是：ELM 不再被视为“内核模块加载器”，而是收敛为 **能力织网运行时**。内核单元不通过符号互相链接，而是通过带版本的流契约连接到织网端口。资源访问通过租约完成，生命周期变更通过预检和提交完成，运行时状态通过快照、事件和审计可观测。

## 2. 统一中文术语

| 英文名 | 中文名 | 含义 |
| --- | --- | --- |
| ELM Cell | 内核单元 | 一个可管理、可绑定、可拓展的 ELM 实例 |
| elm-mgr | 单元管理器 | 启动期根管理单元，负责策略和用户可见管理 |
| elm-mgr API Gateway | 单元 API 网关 | `elm-mgr` 对外公开管理 API、事件 API 和子系统 API 的统一入口 |
| Nexus | 能力织网 | ELM 之间和内核能力之间的运行时连接网络 |
| Nexus Port | 织网端口 | 能力织网中可绑定的能力入口或出口 |
| Port Provider | 端口提供者 | 向能力织网注册端口并执行端口语义的内核或 ELM 实体 |
| Flow Contract | 流契约 | 描述一次能力流的输入、输出、错误、并发和背压语义 |
| Intent | 能力意图 | 内核单元声明自己想消费、提供、拓展或观察的能力 |
| Offer | 能力提供 | 内核单元声明自己能提供的能力 |
| Binding | 能力绑定 | 一个能力意图与一个织网端口或能力提供之间的实际连接 |
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
| EBI Source | EBI 来源 | 任意能够产出 EBI 协议对象的镜像、容器、内建对象或测试对象 |
| EKI | ELM 内核镜像 | ELM 原生镜像格式，天然贴合 EBI，用于近期承载 ELM 单元 |
| soyo | soyo 可拓展文件类型 | 未来通用文件类型，本身不等于 EBI，但具备通过 profile 或对象实现出 EBI 的能力 |

## 3. 总体架构

```text
内核 ELM Core
    |
    +-- elm-mgr：根管理单元
          |
          +-- 普通内核单元
          +-- 拓展单元
          +-- 服务单元
          +-- 驱动单元
          +-- 子管理单元
```

内核 ELM Core 负责硬约束：

- 加载启动期 `elm-mgr`。
- 维护真实状态机、运行拓扑、资源租约、事件序列、审计环和绑定图。
- 校验单元清单、目标架构、ABI 版本、能力权限、依赖合法性和端口契约。
- 执行静默化、脱离、退役、故障隔离等安全流程。
- 提供私有系统调用 `sys_elm_ctl`。
- 为 `elm-mgr` 提供可信执行底座和可验证提交路径，不让普通 ELM 直接接触子系统内部对象。
- 保留 EBI 协议对象装载入口，等待 EBI Source 输入 ABI 接入。

`elm-mgr` 负责策略：

- 管理所有后续 ELM。
- 作为所有 ELM 通向内核能力的统一通道。
- 维护模组菜单和用户可见管理界面。
- 决定加载、卸载、启用、禁用、替换和配置策略。
- 编排普通 ELM 之间的依赖、拓展和能力绑定。
- 处理外部工具通过 `sys_elm_ctl` 发送的管理请求。
- 公开 `elm::mgr::api::*` 形式的 Rust ABI，让未来 ELM 框架只依赖单元管理器 API，不直接依赖 `general`、`kernel` 或具体子系统。
- 通过统一的 provider Ops 接纳 VFS、设备、网络、IRQ、DMA、MMIO 等子系统导出的能力。
- 将策略结果转化为内核可验证的预检和提交操作。

普通 ELM 负责能力：

- 声明能力意图、能力提供、依赖和拓展项。
- 通过织网端口参与能力流。
- 通过资源租约访问内核资源。
- 遵守状态机、热插拔和故障隔离规则。
- 不持有裸内核对象指针，不直接调用未声明的内核内部接口。

## 4. 核心运行模型

ELM 的运行模型由五个对象构成：

- 内核单元：运行时可管理的最小实体，具有 `ElmId`、名称、类型、状态、切换代和 EBI 装载状态。
- 织网端口：能力织网中的稳定连接点，具有 `PortId`、流契约、方向、模式和实现状态。
- 能力绑定：把一个内核单元连接到一个织网端口，具有 `BindingId`、契约、切换代、活动状态和可选租约。
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
- 同步调用帧：ABI 已稳定为 `ElmCallFrame` / `ElmReplyFrame`；已接入 kernel-backed 管理动作 provider，动态 ELM 原生 provider 仍保持 TODO 边界。
- 异步 provider 队列：`elm-mgr` 可以提交 provider 调用、轮询结果、取消排队任务并查询队列统计；队列会持有 provider 租约直到结果被领取、TTL 过期或结果环淘汰。
- `elm.mgr.api.registry@1`：`elm-mgr` 公开 API 注册表，描述当前可用的管理 API、事件 API、provider API 和未来子系统 API。
- `elm.mgr.event.*@1`：`elm-mgr` 提供事件订阅、订阅查询、订阅读取和退订能力，每个订阅都有独立租约和游标。

第一版同步调用帧固定内联载荷为 256 字节。调用帧只表达 `binding_id`、`call_id`、`opcode`、`flags` 和 payload，不携带指针，也不绑定文件格式。`mgr.action.invoke@1` 使用 `ElmActionInvokeRequest` 和 `ElmActionInvokeReply` 作为 payload ABI；动态 provider 只能作为声明进入能力织网，真实 ELM 原生执行器仍是 `TODO(elm)`。

异步 provider 队列复用同一个调用帧，不创造第二套 provider ABI。`SubmitProviderCall` 把 `ElmCallFrame` 包进 `ElmProviderAsyncSubmitRequest`，只额外描述超时、结果保留 TTL 和保留 flags。同步 `InvokeProvider` 仍保留，用于低延迟管理动作和兼容现有外部工具；异步路径用于需要背压、取消、超时和结果保留的 provider 调用。

## 5. 能力织网

ELM 不直接适配内核 trait，也不直接导出或导入符号。所有交互都经过能力织网。

```text
内核单元
    声明能力意图
        绑定织网端口
            获得资源租约
                参与流契约
                    由端口提供者或事件反应器处理事件
```

织网端口代表一种可组合能力。它可以表示设备事件、文件系统操作、网络包流、IRQ 事件、菜单项注册、配置变更或诊断输出。内核内部可以存在桥接层，但 ELM 看到的稳定边界永远是织网端口、流契约和资源租约。

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

## 6. 内建织网端口

| 端口 | 方向 | 模式 | 当前状态 | 设计语义 |
| --- | --- | --- | --- | --- |
| `core.log@1` | Sink | Shared | 已实现 | 单元向内核日志提交固定长度运行时日志 |
| `core.event@1` | Source | Broadcast | 已实现 | 单元按游标读取 ELM 拓扑和管理事件 |
| `mgr.menu.item@1` | Sink | Ordered | 已实现 | 单元向 `elm-mgr` 注册菜单项 |
| `mgr.action.invoke@1` | Control | Shared | 已实现第一版 | `elm-mgr` 内建管理动作调用入口，当前支持健康检查动作 |
| `device.discovered@1` | Source | Broadcast | TODO(elm) | 设备发现事件流 |
| `device.claim@1` | Control | Exclusive | TODO(elm) | 设备声明、抢占和释放控制 |
| `irq.event@1` | Source | Shared | TODO(elm) | IRQ 事件分发 |
| `dma.buffer@1` | Duplex | Shared | TODO(elm) | DMA 缓冲区申请、映射、同步和释放 |
| `mmio.window@1` | Duplex | Shared | TODO(elm) | MMIO 窗口映射和访问租约 |
| `io.block.submit@1` | Sink | Shared | TODO(elm) | 块 I/O 请求提交 |
| `io.packet.rx@1` | Source | Pipeline | TODO(elm) | 网络包接收流 |
| `io.packet.tx@1` | Sink | Pipeline | TODO(elm) | 网络包发送流 |
| `vfs.lookup@1` | Control | Shared | TODO(elm) | VFS 路径查找控制面 |
| `vfs.read@1` | Control | Shared | TODO(elm) | VFS 读控制面 |
| `vfs.write@1` | Control | Shared | TODO(elm) | VFS 写控制面 |

这些端口不是最终能力集合，只是启动期内建端口。后续完整 ELM 设计中，端口提供者也可以来自 ELM 本身：一个 ELM 可以声明新的能力提供，经过 `elm-mgr` 策略和 ELM Core 校验后，把新端口注册进能力织网。这样设备类型、VFS 扩展点、网络处理链、诊断能力和子管理能力都不需要写死在核心中。

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
- 能力绑定关系：描述当前单元连接到某个织网端口。

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

- `PauseCell` 支持动态、非原生单元从 `Active` 进入 `Quiescing` 再进入 `Paused`。
- `ResumeCell` 支持动态、非原生单元从 `Paused` 回到 `Active`。
- `DetachCell` 支持动态单元撤销租约、移除菜单项、摘除绑定图并退役。
- `PreflightLifecycle` 会返回阻断位、最终状态和受影响的子单元、依赖者、拓展项数量。
- 内建单元受保护，默认不能被暂停、脱离或替换。
- 含原生代码的已激活单元仍阻断生命周期操作，直到卸载执行器完成。

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

当前 `ReplaceCell` 只保留稳定命令号和结构化预检响应，会记录 `REPLACE_TODO` 审计。完整热替换仍是 `TODO(elm)`：需要影子绑定、状态迁移、切换代回滚、端口执行器暂停和原生代码卸载协作。

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
- 管理调用输入上限当前为 4096 字节 payload 加管理调用头。

## 12. `elm-mgr` 管理通道 ABI

`MGR_CALL` 的输入由 `ElmMgrCallHeader` 加 payload 组成，输出由 `ElmMgrResponseHeader` 加可选 payload 组成。所有跨边界结构必须是固定布局，不包含内核指针。

管理通道当前稳定边界：

- 输入 payload 上限由模型层常量 `ELM_MGR_MAX_PAYLOAD` 固定为 4096 字节。
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
| `LoadCell` | 2 | 已实现边界 | 接收 EBI Source 请求，当前支持 EKI 元数据展开为 EBI |
| `DetachCell` | 3 | 已实现部分 | 支持动态单元脱离和退役 |
| `PauseCell` | 4 | 已实现部分 | 支持动态、非原生单元暂停 |
| `ResumeCell` | 5 | 已实现部分 | 支持动态、非原生单元恢复 |
| `ReplaceCell` | 6 | TODO(elm) | 保留命令号，返回结构化预检 |
| `QueryTopology` | 7 | 已实现 | 返回父子、依赖、拓展点和拓展项关系 |
| `QueryPolicy` | 8 | 已实现 | 返回策略能力、支持动作和阻断位 |
| `PreflightLifecycle` | 9 | 已实现 | 生命周期操作预检 |
| `QueryAudit` | 10 | 已实现 | 返回管理操作审计环 |
| `QueryNexusBindings` | 11 | 已实现 | 返回能力绑定快照 |
| `PreflightBind` | 12 | 已实现 | 能力绑定预检 |
| `CommitBind` | 13 | 已实现部分 | 已支持三个已实现端口 |
| `PreflightUnbind` | 14 | 已实现 | 能力解绑预检 |
| `CommitUnbind` | 15 | 已实现 | 能力解绑、租约撤销和菜单项移除 |
| `SubmitRuntimeLog` | 16 | 已实现 | 通过 `core.log@1` 提交运行时日志 |
| `ReadRuntimeEvent` | 17 | 已实现 | 通过 `core.event@1` 读取运行时事件 |
| `AckRuntimeEvent` | 18 | 已实现 | 通过 `core.event@1` 确认事件游标 |
| `QueryRuntimePorts` | 19 | 已实现 | 返回运行时端口绑定统计 |
| `RegisterProviderPort` | 20 | 已实现 | 注册动态 provider 端口声明，真实执行器未接入时标记为 TODO |
| `UnregisterProviderPort` | 21 | 已实现 | 注销无活跃 binding 的动态 provider 端口 |
| `QueryProviderPorts` | 22 | 已实现 | 返回 provider 端口、访问策略、调用统计和绑定数量 |
| `InvokeProvider` | 23 | 已实现边界 | ABI 和校验路径已稳定，真实后端未接入时返回 TODO/UNSUPPORTED |
| `QueryProviderStats` | 24 | 已实现 | 返回 provider 端口统计记录 |
| `QueryHealth` | 25 | 已实现 | 返回 Core 结构健康记录，用于发现 graph、cell、port、provider、binding、runtime port、menu、event 和 audit 不变量破坏 |
| `SubmitProviderCall` | 26 | 已实现 | 提交异步 provider 调用，成功后返回 ticket |
| `PollProviderReply` | 27 | 已实现 | 按 ticket 查询并领取异步 provider 结果 |
| `CancelProviderCall` | 28 | 已实现 | 取消尚未执行的排队 provider 调用 |
| `QueryProviderQueue` | 29 | 已实现 | 返回 provider 异步队列、运行中数量、结果保留和拒绝统计 |
| `QueryApiRegistry` | 30 | 已实现 | 返回 `elm-mgr` API 注册表，供 ELM 框架发现可用 API |
| `SubscribeEvent` | 31 | 已实现 | 创建事件订阅租约，返回订阅 ID、租约 ID 和初始游标 |
| `UnsubscribeEvent` | 32 | 已实现 | 撤销事件订阅租约并移除订阅记录 |
| `QueryEventSubscriptions` | 33 | 已实现 | 返回当前事件订阅快照 |
| `ReadSubscribedEvents` | 34 | 已实现 | 按订阅 ID 和游标读取事件，`ADVANCE` flag 控制是否推进订阅游标 |

阻断位用于把策略拒绝转化为可观测原因：

- 内建单元受保护。
- 目标单元不存在。
- 当前状态不允许操作。
- 原生代码生命周期执行器未完成。
- 存在子单元、依赖者或拓展项。
- 租约忙碌。
- 热替换尚未完成。
- 绑定图不一致。
- 装载来源不支持或仍缺少对应 EBI Source 实现。
- 端口不存在、契约不匹配、绑定重复或端口尚未实现。
- 绑定不存在或受保护。
- provider 不存在或 provider 仍有活跃 binding。

## 13. EBI、EBI Source、EKI 与 soyo

EBI 的全称是 ELM Binary Interface，中文称为 **ELM 二进制装载接口**。

EBI 不是文件格式。ELM Core 不理解镜像布局、容器布局或外部输入格式，也不应该把某种磁盘格式写进核心。EKI、未来的 soyo profile、启动期内建对象、内存测试对象或远程下发对象都可以作为 EBI Source 产出 EBI 协议对象。ELM Core 只消费 EBI 协议对象。

EKI 是 ELM 原生镜像格式。它的产生目标是让 ELM 在通用 soyo 文件类型进入内核上游前拥有稳定、直接、强 EBI 贴合的镜像承载方式。EKI 不需要模拟通用容器，它应当把 target、manifest、menu、entry、segment、依赖、拓展点和能力声明自然展开为 EBI。

soyo 是未来可拓展文件类型。soyo 本身不实现 EBI，也不应成为 ELM Core 的硬依赖；它只是具备通过某个 ELM profile、对象或 section 组合实现出 EBI 的能力。未来 soyo 可以产出 EBI，也可以内嵌 EKI，但 ELM Core 仍只认 EBI。

当前 EBI 对象包含：

- `ElmEbiTarget`：目标架构、EBI ABI 版本和最低 Core 版本。
- `ElmEbiArch`：`Any`、`Riscv64`、`LoongArch64`。
- `ElmEbiUnit`：清单、目标、菜单声明、段声明、入口声明、依赖声明、拓展点声明、拓展声明、provider port 声明、imports 和 exports 元数据。
- `ElmEbiSegment`：段类型、大小、权限 flags、file size、mem size、对齐、EBI Source block 索引、Source 偏移和内容 hash。
- `ElmEbiEntry`：未来原生入口符号名。
- `ElmEbiMenuDecl`：菜单项 kind、flags、label、description 和 route。
- `ElmEbiDependencyDecl`：依赖的目标单元名和契约。
- `ElmEbiExtensionPointDecl`：当前单元开放的拓展点名和契约。
- `ElmEbiExtensionDecl`：当前单元挂接的目标单元名、拓展点名和契约。
- `ElmEbiProviderPortDecl`：当前单元声明的 provider port 契约、访问策略、方向和模式。
- `ElmEbiImportDecl`：当前单元需要的原生能力入口元数据，不表达传统符号链接。
- `ElmEbiExportDecl`：当前单元开放的原生能力入口元数据，不表达 Linux LKM 符号表。
- `ElmEbiLifecycleHooks`：当前单元必须声明的 Rust 生命周期钩子。
- `ElmLoadCellResponse`：装载结果、单元 ID、最终状态和原因。

当前 EBI 校验规则：

- ABI 版本必须匹配 `ELM_EBI_ABI_VERSION`。
- 目标架构必须匹配当前内核架构，或使用 `Any`。
- `min_core_version` 不能为 0。
- 段数量不能超过 `ELM_EBI_MAX_SEGMENTS`。
- 段大小、内存大小不能为 0，`file_size` 不能大于 `mem_size`，`align` 非 0 时必须是 2 的幂。
- 代码段必须可执行且不可写；只读数据段不可写不可执行；数据段可写不可执行；BSS 必须 `file_size == 0` 且带零填充语义；重定位段必须标记为重定位输入。
- 原生入口符号不能为空，并且必须通过 EBI 符号名校验。
- 菜单 label 和 route 不能为空，并且各字段不能超过固定长度。
- 依赖、拓展点、拓展项和 provider port 声明数量不能超过固定上限。
- 依赖和拓展目标使用 manifest name；provider port 复用现有访问策略、方向和模式枚举。
- provider port 声明的 flags 当前必须为 0。
- imports 和 exports 数量不能超过固定上限，flags 当前必须为 0，契约名必须通过能力契约校验。
- 动态 EBI 单元必须声明 `on_initialize` 和 `on_finalize` 两个生命周期钩子。
- 生命周期钩子当前只接受 Rust ABI v1，签名语义固定为 `fn(&mut ElmContext) -> ElmResult<()>`。
- 生命周期钩子的符号名必须精确等于 `on_initialize` 和 `on_finalize`，flags 必须为 0。

生命周期执行语义：

- `on_initialize` 是单元被加载后的前钩子，负责单元自定义初始化、服务注册、事件订阅、工具能力发布或数据结构准备。
- `on_finalize` 是单元被卸载前的前钩子，负责撤销自定义状态、注销服务、解除订阅和释放由单元持有的资源。
- 两个钩子是 ELM 的强制契约，不代表传统模块 `init/exit` ABI；它们由 ELM Rust 框架生成或显式实现。
- 一个 ELM 可以只是工具单元，只导出函数、类型描述或数据结构定义，但仍必须具备两个生命周期钩子。
- 当前阶段已具备 `ElmContext` 和受控测试执行器骨架；生产路径仍不执行钩子，只记录声明、校验元数据，并把需要执行钩子的动态单元停在 `Loaded`。
- 测试执行器只允许无原生 entry、无代码段、无重定位段的声明式 ELM 完成 `on_initialize`，用于验证菜单、拓扑和 provider 激活链路。
- `on_initialize` 失败时不会激活拓扑，单元进入 `Faulted -> Quarantined`，返回 hook failed reason。
- 已初始化单元卸载前必须执行 `on_finalize`；失败时保留资源和单元用于诊断，成功后才撤销租约、菜单、binding、provider 和图节点。
- 内建 `elm-mgr` 目前使用启动期合成生命周期状态，后续会收敛到同一套执行模型。

当前装载语义：

- 动态 EBI 协议对象必须先完成生命周期钩子校验；在原生执行器接入前，合法对象会登记为单元并停在 `Loaded`，返回 `NativeCodeTodo`。
- 菜单拓展 EBI 协议对象会被解析和预检，但在 `on_initialize` 成功执行前不会挂接到 `elm-mgr` 的 `menu.item` 拓展点，也不会创建菜单租约和菜单项。
- 声明式拓扑 EBI 会在改状态前预检 manifest name 唯一性、依赖目标存在性、拓展点存在性、契约匹配和 provider port 契约冲突。
- 在生产生命周期执行器接入后，预检通过且 `on_initialize` 成功的单元才会把依赖、拓展点、拓展项和 provider port 登记到 BindingGraph、PortRuntime 和 ProviderRuntime；普通能力绑定仍由 `PreflightBind/CommitBind` 完成，不在装载时自动创建。
- provider port 声明会在激活阶段注册为动态 provider；真实 ELM 原生 provider backend 接入前仍保持 TODO 边界。
- 含代码段、重定位段、原生入口或生命周期钩子的 EBI 都会登记为单元并停在 `Loaded`，返回 `NativeCodeTodo`，不执行代码；Core debug dump 会记录 native segment、import、export 和生命周期状态。
- `MGR_CALL(LoadCell)` 接收 `ElmEbiSourceRequest + source payload`，当前只支持 `Eki` Source。
- EKI Source 会校验 `Code`、`ReadOnlyData`、`Data`、`Bss`、`Relocation`、`Notes` payload block 与 `Segments` 表逐项一致，再展开为 EBI segment 元数据。
- EKI Source 已支持 `LifecycleHooks` 元数据 block，并要求它展开成完整 EBI 生命周期钩子声明。
- EKI Source 已支持 imports 和 exports 元数据 block，但这些元数据只为后续原生执行器准备，不触发链接或调用。
- 空 payload 或尚未实现的 Source kind 会返回 `TODO(elm)` 并记录 `LOAD_REQUIRES_EBI_SOURCE` 审计。
- 非法 EBI Source 请求、损坏 EKI 或未知 Source kind 会返回 `INVALID`。

尚未完成：

- TODO(elm)：未来 soyo ELM profile 的 EBI 产出层。
- TODO(elm)：代码段映射、重定位、只读页和可执行页权限。
- TODO(elm)：指令缓存同步。
- TODO(elm)：真实原生生命周期钩子调用、原生入口调用、暂停回调、静默化回调和卸载执行器。

## 14. 模型层模块设计：`libs/elm`

`libs/elm` 是纯模型层。它是 `no_std` crate，只描述架构无关、内核无关的协议和模型，不能依赖 `kernel`、`general` 或 `arch`。它的目标是让内核、用户态管理工具、测试和未来 Rust ELM 框架共享同一套稳定数据结构。

### `lib.rs`

职责：

- 声明模型层 crate 的模块边界。
- 统一 re-export 控制面、EBI、错误、事件、绑定图、ID、租约、清单、菜单、管理通道、能力织网、端口、快照、状态机和拓扑模型。
- 保持 `libs/elm` 对内核实现无依赖，使模型层可以被内核、host 单测和未来用户态工具复用。

设计细节：

- crate 使用 `#![no_std]` 和 `alloc`。
- `lib.rs` 是 ELM 稳定模型 API 的聚合出口。
- 后续新增模型必须先判断是否属于稳定协议层；如果只服务某个内核执行器，不应放入 `libs/elm`。

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
- 校验目标架构、ABI 版本、段声明、入口声明、菜单声明和生命周期钩子声明。

设计细节：

- `ElmEbiArch::Any` 可用于架构无关的声明和工具单元。
- `ElmEbiLifecycleHooks` 固定要求 `on_initialize` 和 `on_finalize`。
- 生命周期钩子当前只定义 Rust ABI v1，不定义 C/C++ ABI。
- 生命周期钩子签名语义固定为 `fn(&mut ElmContext) -> ElmResult<()>`。
- `Code` 和 `Relocation` 段会触发原生装载器需求。
- `entry` 存在时也视为需要原生装载器。
- `lifecycle_hooks` 存在时也视为需要原生执行器，因为当前阶段不能假装执行初始化逻辑。
- 当前 `NativeCodeTodo` 是有意的边界，不允许假装执行原生代码。

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

- 定义能力织网同步调用帧。
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

- 当前命令号已扩展到 34，覆盖动态 provider 注册、注销、查询、同步调用、异步提交、轮询、取消、队列统计、Core 健康查询、API 注册表和事件订阅。
- `ElmMgrPolicyInfo` 暴露支持动作、策略 flags、阻断位 mask 和审计容量。
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
- `ElmProviderPortRecord` 和 `ElmProviderPortStatsRecord` 暴露 provider 绑定数量和调用统计。
- provider 观测 flags 已稳定为 `DYNAMIC`、`KERNEL_BACKEND` 和 `TODO_BACKEND`，用于区分动态声明端口、内核内建后端和等待 ELM 原生执行器的 TODO 后端。
- `PROVIDER_CALL_FAILED` 阻断位用于审计 provider transport 成功但 `ElmReplyFrame.status` 失败的业务调用。
- `ElmCoreHealthHeader` 和 `ElmCoreHealthRecord` 暴露 Core 自检结果；每类检查通过时也会输出 OK 记录，失败时携带对象 ID 和 detail。
- API 注册表和事件订阅命令是 `elm-mgr` 作为 API 网关的第一组外部可发现能力。
- `ElmMgrApiRegistryHeader` 和 `ElmMgrApiDescriptor` 是 `elm-mgr` API 网关的发现 ABI，描述 API 命名空间、名称、契约、类型、flags、命令号和 owner。
- `ElmMgrEventSubscribeRequest` / `ElmMgrEventSubscribeResponse` 负责创建事件订阅；订阅本身由 `EventSubscription` 租约保护。
- `ElmMgrEventSubscriptionHeader` / `ElmMgrEventSubscriptionRecord` 返回订阅快照，包含过滤器、游标、投递计数和丢弃计数。
- `ElmMgrSubscribedEventReadRequest` / `ElmMgrSubscribedEventReadHeader` 负责按订阅读取事件；`ELM_MGR_EVENT_READ_FLAG_ADVANCE` 为 0 时只读取不推进订阅游标，为 1 时读取后推进游标。

### `mgr/api.rs`

职责：

- 定义 `elm-mgr` 对未来 Rust ELM 框架公开的稳定 API 协议。
- 把 `elm-mgr` API 注册表、事件订阅和订阅读取结构从管理通道主文件中拆出，形成 `elm::mgr::api::*` 风格的使用边界。
- 为后续子系统 API 接入保留统一描述格式，而不是让 ELM 直接依赖 `general::*`、`kernel::*` 或某个子系统 crate。

设计细节：

- API 描述使用固定长度命名空间、名称和契约字段，不携带指针。
- API 类型分为 Control、Snapshot、Event、Provider 和 Subsystem。
- API flags 区分 Stable、TODO、Syscall、Sysfs 和 ProviderOps。
- 事件订阅支持按事件类型、cell、port、binding 和 lease 过滤。
- 事件订阅读取和 `core.event@1` 运行时端口读取是两条路径：前者是 `elm-mgr` API 网关能力，后者是能力织网端口能力。

### `nexus.rs`

职责：

- 定义能力织网模型。
- 定义流契约、能力意图、能力提供、方向、模式、并发和背压。

设计细节：

- `FlowContract` 是稳定兼容边界。
- `NexusIntent` 表达 Consume、Offer、Extend、Observe 和 Control。
- `NexusOffer` 表达契约和流模式。
- 并发和背压先进入模型层，真实调度由后续端口执行器实现。

### `ports.rs`

职责：

- 定义内建织网端口描述。
- 为启动期 ELM Core 提供端口列表。

设计细节：

- 当前有 15 个内建端口。
- 已实现端口只有 `core.log@1`、`core.event@1` 和 `mgr.menu.item@1`。
- 其它端口保留契约和方向，绑定预检会返回 `PORT_TODO`。
- 端口描述已包含访问策略和是否可调用标记。

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

- 初始化时注册内建 `elm-mgr`。
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
- `ProviderBackend::Kernel(kind)` 表示已接入的内核 provider 执行器；当前第一个真实执行器是 `MgrActionInvoke`。
- 事件环和审计环容量当前均为 128。
- 动态 cell ID 从 100 开始，避免与内建 ID 冲突。
- 动态 port ID 从 100 开始，避免与内建端口冲突。
- 内建 `elm-mgr` ID 为 1，动态单元 ID 从 100 开始分配。
- 绑定提交会先做 preflight，再分派到具体端口 attach 路径。
- `core.log@1` 创建 `RuntimePort` 写租约。
- `core.event@1` 创建 `RuntimePort` 读租约。
- `mgr.menu.item@1` 创建菜单租约和菜单项。
- `elm-mgr` 启动时会注册一个内建健康检查菜单动作，该动作不带 TODO 标志，通过 `mgr.action.invoke@1` 调用。
- 动态 provider 端口在真实 ELM 原生执行器接入前只登记声明，不创建可调用后端。
- `ProviderBackend::ElmNativeTodo` 明确标记未来由真实 ELM 原生代码提供的执行器边界。
- 异步 provider 队列由 `ProviderAsyncJob` 和 `ProviderAsyncResult` 分离建模；job 表示仍等待执行的调用，result 表示已完成、失败或过期并等待外部领取的终态。
- 提交异步 job 时会给 binding lease 增加 active ref；job 被取消、result 被 poll、result TTL 过期或结果环淘汰时释放 active ref，因此 `PreflightUnbind` 和生命周期预检能观察到真实忙碌状态。
- 队列容量按 provider 端口计算，默认上限为 64；`Exclusive` 端口上限为 1，`Ordered` 端口上限为 32，`Shared`、`Pipeline` 和 `Broadcast` 使用默认上限。
- provider worker 从队列中取出可运行 job，复用同步 provider 后端执行逻辑，并把 `ElmReplyFrame.status != OK` 计为业务失败和 `PROVIDER_CALL_FAILED` 审计。
- 排队超时会生成 `Expired` 结果并保留到 poll 或 TTL 清理；当前不强抢正在执行的 provider 调用，运行中调用若完成时已经超过 deadline，会被标记为 `Expired`。
- `ProviderRuntime::record_flags()` 是 provider 观测 flags 的唯一派生入口，`QueryProviderPorts` 和 `QueryProviderStats` 使用同一套 flags 语义。
- detach 会阻断仍有子单元、依赖者、拓展项或 busy 租约的目标单元。
- provider owner detach 会阻断仍有活跃 binding 的 provider 端口。
- owner detach 会移除 owner 持有的事件订阅记录，并通过租约撤销链路释放订阅资源。
- `register_builtin_mgr_api()` 会注册启动期 `elm-mgr` API 集合：policy、health、menu、topology、bindings、audit、runtime ports、providers、provider stats、provider queue、api registry、event subscribe、event unsubscribe、event subscriptions 和 event read。
- VFS、device、network、IRQ、DMA、MMIO 当前已经作为 TODO 子系统 API 进入 API 注册表；它们只表达未来可接入能力，不假装已有真实执行器。
- `ElmMgrProviderOps` 是后续子系统接入 `elm-mgr` 的统一 Ops 形状，负责 descriptor、invoke、ready、snapshot 和 revoke 回调；当前作为设计边界保留。
- `health_bytes()` 会输出 graph、cells、ports、providers、bindings、runtime_ports、menu、events 和 audits 九类结构健康记录，并校验 provider 后端、动态端口和观测 flags 的一致性。
- `health_bytes()` 同时校验事件订阅 owner、事件租约类型和订阅游标，避免订阅表与租约表脱节。
- `sysfs_text()` 为 `/sys/kernel/elm` 提供只读文本渲染，覆盖 core、policy、health、menu、topology、ports、providers、bindings、events、audit 和 api。
- debug dump 会输出 cells、ports、bindings、leases、runtime_ports 和 health 摘要，并保留未完成边界说明。

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
- `LoadCell` 当前接收 EBI Source 请求，支持 EKI 元数据解析并通过 EBI 装载入口注册单元；空 payload 和未实现 Source 仍记录 `LOAD_REQUIRES_EBI_SOURCE` 审计并返回 TODO。
- `ReplaceCell` 当前只执行结构化预检并记录审计。
- 运行时日志和事件命令会校验 binding 是否存在、端口是否匹配、租约和状态是否允许。
- provider 注册命令会校验 owner、契约、方向、模式、访问策略和保留 flags。
- provider 调用命令会校验 binding、租约、端口可调用性和 payload 长度。
- provider transport 错误通过 `MGR_CALL` response status 返回；provider 业务结果通过 `ElmReplyFrame.status` 和 reply payload 返回，业务失败会计入 provider failed_calls 并写入 `PROVIDER_CALL_FAILED` 审计。
- provider 异步提交命令会校验 binding、租约、端口可调用性、后端状态和队列容量；成功提交后返回 ticket，并唤醒 provider worker。
- provider 异步轮询命令会先清理过期结果和超时 job；终态结果被领取后立即释放租约活跃引用。
- provider 异步取消命令当前只取消仍在队列中的 job；运行中调用的协作式取消需要未来 provider 执行器协议支持。
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

- `/sys/kernel/elm` 当前包含 `core`、`policy`、`health`、`menu`、`topology`、`ports`、`providers`、`bindings`、`events`、`audit` 和 `api`。
- sysfs 只承担观测，不承担控制；所有写入、加载、绑定、订阅和卸载仍必须走 `sys_elm_ctl(MGR_CALL)`。
- sysfs 输出是文本诊断面，不替代固定布局 ABI；外部工具需要稳定机器解析时仍应使用 `MGR_CALL`。

## 16. 观测、审计与调试

ELM 的可观测性由四条路径组成：

- Core query：快速获取 Core 能力和数量。
- Snapshot：获取 cells 和 ports 的固定布局快照。
- Events：按序列读取拓扑变化。
- Audit：读取管理操作审计环。
- Sysfs：通过 `/sys/kernel/elm/*` 读取只读文本诊断快照。

审计记录覆盖：

- 生命周期操作。
- 绑定和解绑。
- 运行时日志提交。
- 运行时事件读取和确认。
- 装载被缺失或未实现的 EBI Source 边界阻断。
- 热替换 TODO 阻断。

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
- `ports`：织网端口。
- `providers`：provider 后端、统计和异步队列摘要。
- `bindings`：能力绑定。
- `events`：事件环和事件订阅。
- `audit`：管理审计环。
- `api`：`elm-mgr` API 注册表。

这套观测机制的目标是让外部工具不需要解析内核日志，也能判断当前 ELM 拓扑、策略阻断原因和端口运行状态。需要稳定机器解析或执行控制动作时，仍然必须使用固定布局的 `sys_elm_ctl(MGR_CALL)`；sysfs 不提供写入口。

## 17. 当前实现进度

已完成：

- `libs/elm` 已提供纯模型层，包括清单、状态机、绑定图、资源租约、事件、快照、菜单和管理通道固定布局。
- 内核启动时已注册内建 `elm-mgr`。
- 内核启动拓扑只包含根管理单元和内建端口，不再注入演示单元。
- `sys_elm_ctl` 已支持 `CORE_QUERY`、`SNAPSHOT_READ`、`EVENT_READ`、`EVENT_ACK`、`MGR_CALL` 和 `DEBUG_DUMP`。
- `MGR_CALL(QueryMenu)` 已返回固定布局的菜单快照。
- `MGR_CALL(QueryPolicy)` 已返回当前单元管理器策略能力、支持动作、阻断位和审计容量。
- `MGR_CALL(QueryTopology)` 已返回父子、依赖、拓展点和拓展项组成的关系快照。
- `MGR_CALL(PreflightLifecycle)` 已支持暂停、恢复、脱离和替换的策略预检。
- `MGR_CALL(QueryAudit)` 已返回管理操作审计环，包括动作、状态、阻断位和最终状态。
- `MGR_CALL(QueryNexusBindings)` 已返回能力织网绑定快照。
- `MGR_CALL(PreflightBind/CommitBind)` 已支持内建菜单端口 `mgr.menu.item@1` 的绑定预检和提交。
- `core.log@1` 和 `core.event@1` 已支持真实绑定、租约登记、查询快照和撤销。
- `MGR_CALL(SubmitRuntimeLog)` 已支持通过 `core.log@1` 绑定提交固定长度日志 payload。
- `MGR_CALL(ReadRuntimeEvent/AckRuntimeEvent)` 已支持通过 `core.event@1` 绑定按游标读取和确认 ELM 事件。
- `MGR_CALL(QueryRuntimePorts)` 已返回运行时端口绑定统计，包括日志提交数、事件投递数和丢弃事件数。
- `MGR_CALL(RegisterProviderPort/UnregisterProviderPort)` 已支持动态 provider 端口声明注册和注销；动态端口合约由运行时自有字符串保存，不再泄漏为静态字符串。
- `MGR_CALL(QueryProviderPorts/QueryProviderStats)` 已返回 provider 端口、访问策略、绑定数量、调用次数、失败次数、撤销次数和 provider 观测 flags。
- `MGR_CALL(InvokeProvider)` 已保留 256 字节 `ElmCallFrame` / `ElmReplyFrame` ABI；`mgr.action.invoke@1` 已接入 kernel-backed 执行器，管理通道已覆盖健康动作调用和业务失败审计，动态 ELM 原生 provider 仍返回 `TODO(elm)`。
- `MGR_CALL(SubmitProviderCall/PollProviderReply/CancelProviderCall/QueryProviderQueue)` 已形成真实异步队列闭环，支持 ticket、队列容量、超时、结果 TTL、取消、队列统计和租约 active ref 保护。
- `MGR_CALL(QueryApiRegistry)` 已返回 `elm-mgr` API 注册表，覆盖已实现管理 API、事件 API、provider API 和 TODO 子系统 API。
- `MGR_CALL(SubscribeEvent/UnsubscribeEvent/QueryEventSubscriptions/ReadSubscribedEvents)` 已形成事件订阅闭环，支持订阅租约、独立游标、过滤器、只读不推进和读取后推进两种模式。
- `/sys/kernel/elm` 已提供只读文本观测面，覆盖 core、policy、health、menu、topology、ports、providers、bindings、events、audit 和 api。
- `MGR_CALL(QueryHealth)` 已返回结构化 Core 健康记录，可定位 graph、cell、port、provider、binding、runtime port、menu、event 和 audit 不变量破坏。
- `MGR_CALL` 管理通道已收口输入上限、请求头构造器、保留位零值策略和无 payload 查询命令校验；格式错误统一返回 `INVALID`，未知命令号返回 `UNSUPPORTED`。
- 动态 provider 端口已支持 `Public`、`ExtensionOnly` 和 `Internal` 三类访问策略。
- 动态 provider 端口在真实 ELM 原生执行器接入前会被预检阻断为 `PORT_TODO`，已纳入 provider busy、注销和观测链路。
- `MGR_CALL(PreflightUnbind/CommitUnbind)` 已支持动态能力绑定的预检和撤销；内建保护绑定不可撤销。
- 绑定图已记录真实能力绑定边，并将绑定、菜单租约和菜单项纳入同一条撤销链路。
- EBI 已重构为稳定装载协议对象，包括目标架构、ABI 版本、清单、菜单声明、段声明、入口声明、声明式拓扑声明和生命周期钩子声明。
- `MGR_CALL(LoadCell)` 已接收 EBI Source 请求；当前支持 EKI 元数据展开为 EBI，并拒绝旧裸 EBI 字节格式。
- 动态 EBI 单元已强制要求 `on_initialize` 和 `on_finalize`，钩子 ABI 暂定为 Rust ABI v1。
- EKI Source 已支持依赖、拓展点、拓展项、provider port、原生 payload segment、imports、exports 和生命周期钩子元数据；装载时会完成解析和预检，但在执行器接入前不会激活拓扑。
- 含原生代码段、原生入口标记或生命周期钩子的 EBI 会登记为单元并停在 `Loaded`，响应 `TODO(elm)` 状态，不执行代码。
- Core 已记录动态单元生命周期声明、测试执行器可用性、初始化状态、完成卸载状态和 pending EBI load 计划；启动期内建 `elm-mgr` 当前使用合成生命周期状态。
- `ElmContext` 已作为 Rust 生命周期上下文进入模型层，当前不进入 `sys_elm_ctl` 字节 ABI。
- kernel-tests 已具备受控生命周期测试执行器：可验证无原生镜像单元初始化后激活菜单、拓扑和 provider，初始化失败隔离，卸载前 finalize，以及 finalize 失败保留资源。
- 受控测试执行器不会激活带原生 entry、代码段或重定位段的单元，这类单元仍停在 `Loaded + NativeCodeTodo`。
- `PauseCell` 和 `ResumeCell` 已通过统一预检策略支持动态、非原生单元的真实状态切换。
- `DetachCell` 已通过统一预检策略支持动态单元的资源租约撤销、菜单项移除、绑定图摘除和退役；尚未激活的原生 TODO 单元可作为元数据直接摘除。
- `DetachCell` 会阻断仍有子单元、依赖者、拓展项或忙碌租约的目标单元，避免破坏当前拓扑。
- `ReplaceCell` 已保留稳定命令号，当前返回结构化预检结果并记录 `REPLACE_TODO` 审计。
- `kernel-tests` 已覆盖启动期 `elm-mgr` 健康检查、内建健康菜单动作、`mgr.action.invoke@1` 的 Core 直调和 `MGR_CALL(InvokeProvider)` 字节路径、provider 业务失败审计、异步 provider 提交、完成、轮询、取消、超时、队列满、队列查询、API 注册表、事件订阅、订阅读取游标推进语义、菜单 EBI 等待生命周期执行器、测试执行器激活菜单 EBI、EKI Source 装载、EKI 声明式拓扑在执行器前不激活、测试执行器激活 EKI 声明式拓扑、hook failed 隔离、原生 EBI 停在 `Loaded + NativeCodeTodo`、动态 provider 可观测、绑定预检 `PORT_TODO` 阻断，以及管理通道字节协议的成功路径和 malformed 请求拒绝。

## 18. 运行期 smoke 测试链路

当前已经具备从用户态进入 `elm-mgr` 的完整运行链路：

```text
用户态 /bin/elmctl-smoke
    -> syscall(SYS_ELM_CTL = 509)
        -> ElmCtlCommand::MgrCall
            -> elm-mgr 管理通道
                -> ELM Core
                    -> 菜单、策略、健康检查、API 注册表、事件订阅、能力绑定、provider 调用、审计、EKI 装载边界
```

`elm-mgr` 本身是启动期内建 ELM。它在 `sched::boot_init()` 之后、用户态 init 进程启动之前完成初始化，地位类似用户态的 init 进程：后续所有动态 ELM 都挂在 `elm-mgr` 管理树下，所有外部管理工具都通过 `elm-mgr` 暴露的控制面进入 ELM Core。未来 VFS、调度、网络、设备等子系统提供给 ELM 的 API 也应注册为 `elm-mgr` 可发现、可绑定、可转发的能力织网端口，而不是为每个子系统新增私有 ELM syscall。

仓库提供 `userland/elmctl-smoke/elmctl_smoke.c` 作为最小用户态 smoke 工具。该工具不依赖 Linux 模块 ABI，不使用 ioctl，不读取 `/proc` 或 `/sys`，只直接调用私有系统调用 `SYS_ELM_CTL`。工具会执行以下检查：

- `CoreQuery`：确认 ELM Core magic、ABI、能力位、cell 数量和端口数量。
- `QueryPolicy`：确认 `elm-mgr` 支持 provider invoke、健康检查、异步 provider 和 lifecycle hook failed blocker。
- `QueryMenu`：确认内建健康检查菜单项 `elm/mgr/health` 存在，并读取其 action id。
- `QueryHealth`：确认 Core 结构化健康检查为 OK。
- `QueryNexusBindings` / `CommitBind`：复用或创建 `elm-mgr -> mgr.action.invoke@1` 能力绑定。
- `InvokeProvider`：通过 `ElmCallFrame` 调用健康检查 action provider。
- `QueryApiRegistry`：确认 `elm-mgr` API 网关至少公开当前已知管理 API、事件 API、provider API 和 TODO 子系统 API。
- `SubscribeEvent` / `QueryEventSubscriptions`：为内建 `elm-mgr` 创建事件订阅租约，并确认订阅快照可读。
- `LoadCell`：构造一个最小 EKI Source，经 `elm-mgr` 装载为 EBI 协议对象，预期停在 `Loaded + NativeCodeTodo`。
- `DetachCell`：摘除上一步尚未激活的 EKI 元数据单元，确认元数据路径可清理。
- `ReadSubscribedEvents` / `UnsubscribeEvent`：读取 EKI 装载和脱离产生的订阅事件，然后撤销订阅租约并确认没有订阅泄漏。
- `QueryAudit`：确认管理审计流可读。

构建方式：

```sh
docker run --rm -it -v "$PWD":/work -w /work zhouzhouyi/os-contest:20260510 bash
make kernel-rv
make kernel-la
```

`make kernel-rv` 会把 RISC-V64 静态链接版本安装到 `userland/rootfs-rv/bin/elmctl-smoke`，并重新打包到 `build/initramfs-rv.cpio`。`make kernel-la` 同理安装 LoongArch64 版本到 `userland/rootfs-la/bin/elmctl-smoke`。

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

预期输出包含：

```text
[elm-smoke] core query ok
[elm-smoke] policy query ok
[elm-smoke] menu query ok
[elm-smoke] health query ok
[elm-smoke] bind mgr action provider ok
[elm-smoke] invoke health action ok
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

尚未完成：

- TODO(elm)：EBI 重定位、代码页权限、真实生命周期钩子调用、入口调用和指令缓存同步。
- TODO(elm)：未来 soyo ELM profile 的 EBI 产出层。
- TODO(elm)：热替换、影子绑定、状态迁移和回滚。
- TODO(elm)：原生代码单元的暂停、恢复、静默化回调和卸载执行器。
- TODO(elm)：设备、VFS、网络、IRQ、DMA、MMIO 等端口的真实绑定执行器和提供者。
- TODO(elm)：由真实 ELM 原生代码提供 provider backend 的执行器。
- TODO(elm)：端口级故障隔离、运行中调用协作式取消和真实 ELM 原生 provider backend。
- TODO(elm)：面向 Rust ELM 的开发框架和构建链路。

阶段收束结论：

- 当前 ELM 已经从“模型草案”进入“管理运行时可验证闭环”阶段：`elm-mgr` 可以作为外界入口管理菜单、策略、拓扑、审计、provider、事件订阅、API 注册表和 EBI/EKI 元数据装载边界。
- 当前阶段不再继续把零散能力堆进 `MGR_CALL`，后续新增能力必须归入明确主线：原生执行器、子系统 provider 接入、热替换与故障隔离、Rust 开发框架、网络栈 ELM 化。
- `elm-mgr` 是所有 ELM 通向内核能力的唯一网关；后续 VFS、设备、调度、网络、IRQ、DMA、MMIO 等能力只通过 `elm-mgr` API 注册表和 provider Ops 暴露，不新增私有 ELM syscall。
- `EKI` 是近期原生 ELM 镜像承载方式，`soyo` 仍保持后置；ELM Core 继续只消费 EBI 协议对象，不绑定具体文件格式。
- 当前所有原生代码、重定位、代码页权限和真实生命周期调用都必须继续停在 `TODO(elm)` 边界，不能用测试执行器或声明式拓扑伪装为已经实现。

## 19. 后续阶段路线

第一阶段：模型和管理闭环稳定。

- 固化 `libs/elm` 固定布局结构。
- 保持 host 单测覆盖模型约束。
- 保持内核 `kernel-tests` 覆盖第一阶段管理闭环，不让 provider flags、health、管理通道 ABI、EBI 装载状态和动态 provider TODO 边界退化。
- 保持 `sys_elm_ctl`、`MGR_CALL`、快照、事件和审计 ABI 稳定。
- 完成 `core.log@1`、`core.event@1`、`mgr.menu.item@1` 和 `mgr.action.invoke@1` 四个基础端口的闭环。
- 完成动态 provider 端口声明注册、访问策略、同步调用帧 ABI、撤销和统计闭环；真实 ELM 原生执行器接入前绑定提交保持 TODO 阻断。
- 完成 provider 异步队列、ticket、poll、cancel、timeout、result TTL、队列统计和租约 active ref 保护闭环；运行中协作式取消和真实 ELM 原生后端留到执行器阶段。
- 完成 `elm-mgr` API 注册表、事件订阅、订阅读取和 `/sys/kernel/elm` 只读观测闭环，使外部工具和未来 Rust ELM 框架都能以 `elm::mgr::api::*` 作为入口。

第二阶段：EBI Source 与 EKI 深化。

- 稳定 EBI Source 输入 ABI。已完成：`ElmEbiSourceRequest + payload`。
- 补充 EKI 依赖、拓展点、拓展项和 provider port 元数据。已完成：声明式拓扑预检和登记。
- 扩展 EKI 段 payload 表达和校验。已完成：payload block 与 `Segments` 表一致性校验、权限元数据、Source 偏移和内容 hash 展开。
- 补充 EKI imports、exports 和原生符号元数据。已完成：作为 EBI 元数据进入协议对象，不执行传统符号链接。
- 接入未来 soyo ELM profile 的 EBI 产出层。
- 继续保持 ELM Core 只消费 EBI 对象。

第三阶段：原生代码执行器。

- 已完成前置骨架：`ElmContext`、pending load 计划、受控测试执行器、初始化成功后激活、卸载前 finalize、hook failed 隔离。
- 实现架构相关段映射。
- 实现重定位。
- 实现代码页、只读页、数据页权限。
- 实现指令缓存同步。
- 实现入口调用约定。
- 实现暂停、静默化、卸载和故障回调。

第四阶段：端口提供者体系。

- 为每个端口提供者定义注册、预检、提交、撤销、静默化和统计接口。
- 将设备发现、设备声明、IRQ、DMA、MMIO、块 I/O、网络包流和 VFS 控制面接入能力织网。
- 支持 ELM 自身注册新端口类型，让设备类型和子系统拓展不写死在 Core 中。

第五阶段：热替换和故障隔离。

- 实现影子绑定。
- 实现状态迁移协议。
- 实现切换代回滚。
- 实现流排空和旧代退役。
- 实现故障单元隔离态和受限诊断通道。

第六阶段：用户态工具和 Rust 开发框架。

- 提供 `elm-mgr` 外部管理工具。
- 提供菜单、拓扑、审计和运行时端口查看命令。
- 提供 Rust ELM 开发框架。
- 提供声明式 manifest、端口契约生成、EKI 打包和本地测试工具。

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
- 不把 `TODO(elm)` 的原生代码边界伪装成已经完整实现。

唯一允许的类比表述：

> ELM 不采用传统动态内核模块的符号链接模型，而采用能力织网、流契约和资源租约模型。
