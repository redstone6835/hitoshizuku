#import "../config.typ": project-name
#import "../styles/diagram.typ": flow-arrow, layer-card
#import "../styles/figure.typ": continued-table, figure-caption, pseudo-sample
#import "../styles/tokens.typ": handoff-fill, soft-fill, stable-fill, warm-fill

= 第十二章 可拓展内核模块（ELM）

前面的章节讨论了内存、设备、文件系统、进程和系统调用等内核能力。本章进一步回答一个与这些能力同样重要的问题：当一项能力不再由内核镜像中的常驻代码直接提供，而需要在构建期选择、运行期装入、暂停、替换或退出时，内核怎样保持边界清楚、状态一致和行为可追踪。

ELM 的正式全称是 Extensible Loadable Module，中文称为“可拓展内核单元”。这里的 Module 不只表示一个二进制文件，而表示一项进入统一治理体系的内核责任。一个 ELM 具有运行身份、实现代际、父子归属、依赖关系、能力契约、资源预算、生命周期状态和运行证据。代码能够被装入只是起点；只有当它取得的资源可以收回、旧引用可以拒绝、失败位置可以定位、生命周期操作可以预检和回滚时，这项扩展才真正成为内核可管理的一部分。

传统动态内核模块已经具有符号解析、依赖、签名、引用保护和观测工具，不能把它们概括成“完全没有管理能力”。ELM 的差异在于，它把原本散落在装载器、子系统注册表、引用计数、日志和运维工具中的约束，统一收敛到同一个单元模型与事务边界。管理路径因此能够用同一组身份回答“谁提供能力、谁正在使用、当前是哪一代、能否暂停、有哪些资源尚未释放、失败发生在哪里”。数据热路径则可以在装载期完成证明后采用直接调用，不必为了可治理性而一律经过通用消息分发。

== 12.1 设计动机与适用边界

独立构建并不等于可拓展。如果装入代码可以直接保存任意内核地址、把回调注册到多个子系统、创建后台任务而不登记所有者，那么装载本身很容易，安全卸载却几乎不可能。一个看似已经退出的模块仍可能被定时器、设备中断或工作队列调用；一次替换也可能让旧对象误用新实现。问题的根源不是文件格式，而是扩展缺少统一身份和可计算的责任范围。

ELM 首先要求扩展在执行前完成声明。清单说明单元名称、种类、接口、依赖、拓展关系、目标架构和预算；装载对象说明代码段、数据段、重定位、入口和来源证明。Core 在执行模块代码之前验证这些信息，使“不兼容”“越权”“超额”和“来源不可信”成为可以明确拒绝的状态，而不是等到模块初始化一半后再依靠错误路径清理。

其次，ELM 要求运行期事实归属于确定的单元和代际。端口、绑定、租约、原生镜像、调用栈、动态分配、审计记录以及由子系统托管的长期资源，都能回到一个 `ElmId` 和 `Generation`。模块退出不再只调用一个 `exit` 函数，而是先停止新工作，检查调用与租约，排空异步资源，再撤销公开关系并回收镜像。任何不能安全完成的步骤都可以成为明确的阻断原因。

最后，ELM 不把治理成本强加给所有热路径。需要运行时发现、授权、审计、异步流控或取消语义的服务进入 Provider 枢纽；需要精确固定 Rust ABI 的 ELM 间热路径使用 `direct-pinned`；调用常驻内核实现时使用经过登记和证明的 `kernel-symbol`。这种分流使控制面完整、数据面直接，避免把“可管理”误解为“每次调用都要经过管理器”。

#continued-table(
  "12-1",
  [传统模块关注点与 ELM 责任范围],
  (1.15fr, 2.15fr, 2.35fr),
  (
    table.cell(fill: soft-fill)[#text(weight: "bold")[问题]],
    table.cell(fill: soft-fill)[#text(weight: "bold")[常见模块机制]],
    table.cell(fill: soft-fill)[#text(weight: "bold")[ELM 的统一约束]],
  ),
  (
    table.cell(fill: warm-fill)[装入],
    table.cell(fill: warm-fill)[解析镜像、符号和依赖，执行初始化入口。],
    table.cell(fill: warm-fill)[先形成 EBI 对象并验证来源、架构、ABI、预算和关系，再执行任何原生代码。],
    table.cell(fill: soft-fill)[调用],
    table.cell(fill: soft-fill)[按子系统约定直接调用或使用各自注册接口。],
    table.cell(fill: soft-fill)[Provider、direct-pinned 与 kernel-symbol 分流，各自保留契约和代际边界。],
    table.cell(fill: handoff-fill)[退出],
    table.cell(fill: handoff-fill)[依赖模块引用保护和每个子系统自己的注销顺序。],
    table.cell(fill: handoff-fill)[生命周期预检汇总图、租约、执行和受托资源，阻断仍不安全的退役。],
    table.cell(fill: stable-fill)[证据],
    table.cell(fill: stable-fill)[日志、跟踪和子系统统计分别描述局部事实。],
    table.cell(fill: stable-fill)[快照、事件、审计、Trace 与 Journal 共同使用单元身份和序列。],
  ),
  kind: "表",
  continuation-kind: "续表",
  align: (left, left, left),
)

ELM 也有明确的非目标。它不兼容 Linux 的 `init_module`、`modprobe` 或 `ko` ABI，当前也不提供 C/C++ 模块兼容。它不是用户进程式地址空间隔离，更不是面向恶意代码的完整沙箱。它不要求把 VFS、设备或网络的每个函数都包装成 RPC，也不承诺任意模块能够无条件、无损地热替换。当前实现面向与目标内核严格匹配的 Rust 构建环境，精确 Rust ABI 只有在编译器、目标特性和接口摘要都通过验证时才成立。

== 12.2 ELM Core、管理单元与责任 Cell

ELM 运行时分为常驻 Core、内建管理单元和普通 Cell 三个层次。ELM Core 位于内核常驻代码中，维护权威状态机、关系图、端口、绑定、租约、预算、执行记录和审计事实。它负责最终合法性判断与状态提交，不依赖某一种设备、文件系统或网络协议的业务结构。这样，新增子系统能力不需要反向修改 Core 的基本对象模型。

`elm-mgr` 是启动期创建的根管理单元，保留身份为 `ElmId(1)`。它接收外部管理请求，组织菜单、策略、依赖选择、事件订阅和生命周期操作，但它不能绕过 Core 直接改写权威状态。管理器提出意图，Core 执行预检并提交事实。这个分工避免把所有硬约束放进一个可替换策略组件，同时让管理策略仍能通过统一单元接口演进。

`eki` 是 `elm-mgr` 的内建子单元，保留身份为 `ElmId(2)`。它提供 EKI 到 EBI 的 Projection Source，也就是把当前原生镜像格式投影成 Core 能消费的装载对象。两个内建单元的来源显示为 `<builtin>`，启动时直接处于第一代活动状态，不依靠 EKI 文件完成自举。这里的“管理自举”指管理器和格式投影本身也进入拓扑、身份和观测模型；真正不可替代的状态校验仍留在常驻 Core 中。

普通 Cell 是最小受管责任单元。一个 Cell 保存清单、父单元、当前状态、当前代际、策略、预算和装载来源等事实。它可以是协议服务、设备驱动、诊断组件或其他扩展，但业务名称并不是安全主键。名称用于声明、发现和兼容性检查；运行时关系则使用强类型 ID 和代际，以防止同名新实现继承旧实现的悬空引用。

#figure(caption: figure-caption("图", "12-1", [ELM 启动期管理结构]))[
  #layer-card("常驻 ELM Core", [状态机、关系图、装载证明、资源账本、执行边界和证据链], fill: soft-fill)
  #flow-arrow(label: "创建并约束")
  #layer-card("elm-mgr · ElmId(1)", [根管理单元；编排外部请求、策略、菜单和生命周期], fill: handoff-fill)
  #flow-arrow(label: "内建子单元")
  #layer-card("eki · ElmId(2)", [EKI Projection Source；把具体镜像投影为 EBI], fill: warm-fill)
  #flow-arrow(label: "同一管理树")
  #layer-card("普通 ELM Cell", [身份、代际、状态、能力、资源和运行证据的归属单位], fill: stable-fill)
]

这个结构有意区分“策略入口”和“事实所有者”。如果 `elm-mgr` 同时持有所有底层状态，一次管理器故障就可能破坏全局拓扑；如果 Core 同时理解每个子系统协议，它又会退化成不断增长的中心化分支。当前实现把通用不变量放在 Core，把策略编排放在管理单元，把具体能力语义留给提供者，从依赖方向上维持可拓展性。

== 12.3 身份、代际与关系拓扑

ELM 的 ID 类型在底层都使用 `u64` 表示，但 `ElmId`、`PortId`、`BindingId`、`ActionId` 和 `LeaseId` 不能互换，零值统一保留为“无对象”或“尚未分配”。这些标识只在当前启动实例及对象生命周期内有效，不能当作内核地址，也不保证跨启动稳定。强类型的意义在于让错误关系尽量在接口边界暴露，而不是把多个注册表中的数字混成一个无类型句柄。

`ElmId` 标识逻辑 Cell，`Generation` 标识该 Cell 当前采用的具体实现。动态 Cell 首次成功装载使用第一代；热替换提交后，Cell ID 保持不变，代际递增。长期句柄、导入、绑定和租约必须同时匹配 `ElmId + Generation`。因此，一个在旧镜像中取得的引用不会仅因名称相同就自动指向新镜像。代际检查把传统的“地址是否还有效”转化为显式的协议条件。

关系拓扑由 `BindingGraph` 统一维护，但其中的关系不能混为一谈。父子关系表示管理归属和预算委派；依赖关系表示一个单元激活前需要另一个单元存在；拓展点与拓展项表示目标单元主动开放的结构化扩展位置；能力绑定表示消费者与端口之间已经提交的调用连接。管理父节点不一定是业务依赖，业务依赖也不意味着可以取得管理权限。

关系提交会检查端点存在、名称和契约一致、重复关系以及有向环。父子图和依赖图的无环要求，使生命周期可以得到稳定的遍历顺序。暂停或退役时，Core 可以从关系图计算依赖者、子单元、拓展项和有效绑定，而不需要逐个询问未知的子系统注册表。图上的每条边仍只是结构事实；真正提交生命周期变更时，还要继续检查租约、执行、资源和模块钩子。

#pseudo-sample("12-1", [身份与关系的最小判定], kind: "代码")[
  ```text
  逻辑身份      ElmId(42)
  当前实现      Generation(3)
  有效长期引用  owner = (ElmId(42), Generation(3))

  父子关系      manager -> cell
  依赖关系      consumer -> provider
  拓展关系      extension -> target.extension_point
  能力绑定      consumer -> PortId -> BindingId -> LeaseId
  ```
]

拓扑设计还带来一项重要的诊断能力。一次 Provider 调用失败时，可以从 `BindingId` 找到端口和消费者，从端口找到提供者，从两端身份找到代际、策略和资源用量，再把结果与同一序列附近的生命周期事件相互核对。故障不再只是某个地址上的异常，而成为关系图中一个可定位的责任事件。

== 12.4 能力契约、端口、绑定与租约

当一项能力需要运行时发现或动态连接时，ELM 使用枢纽连接层。能力首先用 `name@version` 形式的 Flow Contract 描述。契约名称只表达稳定业务语义，不使用 Rust 类型名或内部符号代替。版本是兼容边界，消费者和提供者必须对完整契约达成一致，不能只比较名称或一个未经校验的哈希。

Port 是能力的连接点。端口描述拥有者、契约、方向、模式、访问策略、是否具有可调用后端以及实现状态。方向可以表达 Source、Sink、Duplex 或 Control，模式用于表达共享、独占、有序、流水或广播等连接语义。访问策略区分公开、内部和仅拓展项可用。模型中还为并发和背压保留了 `Single/Parallel/Reentrant` 与 `Drop/Queue/Stall/Reject` 等描述，但这些策略尚未普遍进入所有 Provider 的实际调度，文档不能把模型枚举误写成已经完整生效的执行器。

Provider 是执行端口真实语义的实体。一个端口可以由常驻内核实现，也可以由原生 ELM 的 `handler_symbol` 实现。动态端口如果没有处理入口，会保持不可调用并返回明确的未实现状态，而不是把一个悬空声明当作有效服务。当前启动期内建端口只有 `core.log@1`、`core.event@1`、`mgr.menu.item@1` 和 `mgr.action.invoke@1`。它们分别承载日志提交、ELM 事件读取、管理菜单注册和内建管理动作调用。

Binding 表示消费者与端口之间已经提交的连接。绑定之前，Core 检查消费者状态和代际、端口存在性、契约、访问策略、配额以及重复关系；提交时生成 `BindingId`，并为可撤销使用权建立 `LeaseId`。Lease 保存所有者、代际、资源种类、读写控制权限、关联 Binding 和活动引用数。撤销先把租约变为 Revoking，阻止新引用进入，再等待活动引用归零，最后进入 Revoked。仍有活动引用时，解绑、暂停或卸载会得到 Busy，而不是回收仍在使用的对象。

#figure(caption: figure-caption("图", "12-2", [能力从声明到调用的闭环]))[
  #layer-card("Intent / Offer", [消费者声明意图，提供者声明能力及完整 Flow Contract], fill: soft-fill)
  #flow-arrow(label: "发现与策略选择")
  #layer-card("PortId", [方向、模式、访问策略、owner generation 与实现状态], fill: warm-fill)
  #flow-arrow(label: "预检并提交")
  #layer-card("BindingId + LeaseId", [固定连接关系；租约保护活动引用和撤销顺序], fill: handoff-fill)
  #flow-arrow(label: "执行真实语义")
  #layer-card("Provider", [常驻内核后端或通过原生调用门进入 ELM handler], fill: stable-fill)
]

Provider 调用使用 `ElmCallFrame` 和 `ElmReplyFrame`。第一版调用帧具有固定头部和 256 字节内联载荷，不携带裸指针；请求中的 Binding、Call ID、opcode、flags 与 payload 长度都要经过验证。这条规则适用于 Provider 和管理线协议，不能泛化为“所有 ELM 边界都不能使用指针”。后文所述 exact-Rust 直接路径在完整 ABI 和权限校验通过后，可以使用真实 Rust 类型；由它创建的长期对象仍然必须进入资源归属与退役协议。

== 12.5 四类运行接口与热路径选择

ELM 不是单一调用机制，而是根据稳定性、发现需求和性能目标选择边界。当前代码可以归纳为四类路径：普通运行时接口、Manager 管理接口、Provider 枢纽调用以及两种精确直接调用。后两种直接调用虽然都不经过通用调用帧，但目标和代际语义不同。

`elm::runtime` 面向普通 ELM，提供当前上下文、日志、终止以及受管调用等基础能力。它通过内核发布的根 API 取得命名空间，并验证版本、结构大小和当前调用上下文。普通单元不能由此取得管理分发入口。只有种类、信任、策略和代际均符合要求的 Manager Cell 才能通过 `elm::management::Client` 取得管理命名空间，而且每次 dispatch 都会重新鉴权。复制一个 Client 不会冻结权限；暂停、替换、策略收紧或信任撤销会立即影响下一次调用。

Provider 用于运行时发现、动态绑定、授权审计、固定线协议、异步队列和取消。其额外分发成本换来了提供者替换、连接撤销和队列治理能力，适合管理动作、事件流以及跨单元稳定服务。对低频控制路径而言，这些检查本身就是接口语义的一部分。

`direct-pinned` 用于 ELM 到 ELM 的精确热路径。装载器按名称、契约、版本和完整 Rust ABI 摘要解析 export，并把目标 Cell 的 Generation 固定到导入槽。调用时直接进入目标实现，不经过 `elm-mgr` 或 Provider frame；目标替换前必须处理仍然固定到旧代的 importer。因此它用生命周期约束换取接近普通函数调用的数据面成本。

`kernel-symbol` 用于 ELM 调用常驻内核的真实实现。符号必须位于内核登记目录中，装载阶段检查名称、契约、版本、权限、接口源码摘要、rustc 条件和目标 ABI，完成地址重定位后直接调用。它不会在 ELM exports 中选择目标，也不按 ELM Generation 路由。该路径适合稳定、类型精确的内核 API，但不允许扫描未登记符号或猜测私有布局。

#continued-table(
  "12-2",
  [ELM 调用路径及适用范围],
  (1.2fr, 1.7fr, 1.55fr, 2.25fr),
  (
    table.cell(fill: soft-fill)[#text(weight: "bold")[路径]],
    table.cell(fill: soft-fill)[#text(weight: "bold")[目标定位]],
    table.cell(fill: soft-fill)[#text(weight: "bold")[运行期边界]],
    table.cell(fill: soft-fill)[#text(weight: "bold")[适用场景]],
  ),
  (
    table.cell(fill: warm-fill)[Runtime / Management],
    table.cell(fill: warm-fill)[固定根 API 与受权命名空间。],
    table.cell(fill: warm-fill)[上下文和权限复核。],
    table.cell(fill: warm-fill)[日志、上下文、生命周期管理、查询与策略控制。],
    table.cell(fill: soft-fill)[Provider],
    table.cell(fill: soft-fill)[Port、Binding 和 Lease。],
    table.cell(fill: soft-fill)[固定帧分发与代际校验。],
    table.cell(fill: soft-fill)[可发现服务、异步任务、审计、取消和背压。],
    table.cell(fill: handoff-fill)[direct-pinned],
    table.cell(fill: handoff-fill)[ELM export 与固定目标代际。],
    table.cell(fill: handoff-fill)[精确 Rust ABI 直接调用。],
    table.cell(fill: handoff-fill)[网络 shard turn 等对延迟敏感的 ELM 间热路径。],
    table.cell(fill: stable-fill)[kernel-symbol],
    table.cell(fill: stable-fill)[常驻内核符号目录。],
    table.cell(fill: stable-fill)[精确 Rust ABI 直接调用。],
    table.cell(fill: stable-fill)[分配、设备和其他经过发布的常驻内核 API。],
  ),
  kind: "表",
  continuation-kind: "续表",
  align: (left, left, left, left),
)

除了上述路径，managed import 还用于每次调用都要核验双方当前代际的受管 ELM 接口。它在新镜像初始化时暂存，只有完整装载事务提交后才成为 Active；装载失败会丢弃暂存导入。它比 direct-pinned 多保留一次运行期代际检查，适用于引用关系需要随状态变化立即失效、但又不需要通用 Provider 线协议的接口。

生命周期钩子则由 Core 主动调用，不属于普通服务发现。它们统一通过原生调用门进入，并继承故障、超时和执行记账边界。把生命周期钩子单独看待很重要：模块不能把 `finalize` 当作普通 Provider 留给任意消费者调用，Core 也不能在没有状态迁移的情况下随意执行 `pause`。

这种路径分流是 ELM 性能设计的核心。管理器不参与每个网络包或每次设备访问，Provider 也不是强制 RPC 层。治理发生在装载、绑定、代际切换和资源生命周期边界；经过证明的热路径在这些边界之间直接执行。性能优化因此不需要删除安全检查，而是把昂贵检查移动到状态变化处，把运行期保留为最小的代际和执行许可约束。

== 12.6 EBI、EKI 与 Projection Source

ELM Core 不直接识别每一种镜像文件。它消费的是 EBI，即 ELM Binary Interface 所定义的装载协议对象。EBI 描述来源、清单、段、符号、重定位、导入导出、生命周期钩子、Provider 和 ABI 指纹等已经解析的事实，但它本身不是磁盘文件格式。把“Core 接受的对象”与“用户提供的容器”分离后，新增镜像格式不需要把解析逻辑写入 Core。

EKI 是当前原生 ELM 镜像格式。内建 `eki` 单元注册 Projection Source，将密封的 EKI 镜像解析并投影成 EBI。Projection Source 本身具有代际、引用计数、暂停和影子切换语义，因此格式处理器也能进入生命周期治理。设计文档中的 `soyo` 属于未来来源，当前仓库没有可用于生产的 soyo Projection Source，不能写成已支持格式。

EKI v1 使用固定魔数和 64 字节头部，对版本、保留字段、镜像长度、块数量、块类型、范围重叠和摘要进行检查。当前解析器限制最多 64 个块和 24 类块，并验证必需块与变体组合。上传通过镜像会话分段写入，Core 检查偏移、长度和重叠；Seal 后再核对整镜像 SHA-256。只有密封会话才能进入 Projection Source，避免解析过程中继续改变输入。

`elm-loader` 的 `prepare_ebi_load` 负责容器无关的预检和导入协商，不映射可执行内存。真正的宿主装载由内核完成：为段分配页，复制内容和零填 BSS，应用 EKI 支持的重定位，检查唯一 `__elm_module_descriptor_v1`，解析导入槽，最后把代码设置为只读可执行、数据设置为可读写不可执行，并同步指令缓存。两个目标架构都注册了镜像权限操作，因此当前原生装载链已经具备实际 W^X 权限切换，而不只是格式解析器。

#figure(caption: figure-caption("图", "12-3", [从镜像会话到活动 Cell 的装载链]))[
  #layer-card("Upload Session + Seal", [分段写入；检查范围、重叠、长度和整镜像摘要], fill: soft-fill)
  #flow-arrow(label: "具体容器")
  #layer-card("EKI Projection Source", [校验 EKI 头、块表和必需对象，产出 EBI], fill: warm-fill)
  #flow-arrow(label: "容器无关对象")
  #layer-card("EBI 预检", [目标、清单、关系、proof、ABI 指纹、预算和 imports], fill: handoff-fill)
  #flow-arrow(label: "宿主原生装载")
  #layer-card("映射与发布", [段复制、重定位、W^X、I-cache、描述符、initialize 与拓扑提交], fill: stable-fill)
]

装载过程把“准备”和“发布”分开。新 Cell 最初只存在于未公开的事务中，import、Provider、菜单和关系先进入暂存状态；描述符验证和 `create/initialize` 成功后，Core 才提交拓扑并把状态推进为 Active。中途失败会丢弃尚未公开的对象，不让其他单元观察到半初始化实现。这个顺序也解释了为什么 EBI 不是简单的函数表：它同时承担执行前证明和提交前计划的载体。

Projection Source 使格式扩展与 Core 不变量解耦，但并不意味着任意投影结果都会被接受。Source 只能产出候选 EBI；Core 仍要独立验证目标架构、清单一致性、proof、ABI、关系和预算。Source 的暂停或替换也要等待活动引用归零，避免某次装载使用一半旧解释器和一半新解释器。格式可拓展性因此没有削弱最终装载边界。

== 12.7 来源证明、Rust ABI 与装载信任

签名只证明来源和完整性，不能单独证明代码安全。ELM 的装载信任由多层条件共同组成。镜像摘要确认上传内容未变化；EBI 规范摘要确认投影结果与被证明对象一致；Ed25519 signer 和 Trust Anchor 表示来源身份；撤销状态和 `release_epoch` 防止已知旧版本回滚；目标架构与 ABI 指纹确认镜像能在当前内核执行。任何一层失败，都在调用模块入口之前拒绝装载。

Rust ABI 指纹不是一句“都是 Rust”就能满足。它覆盖目标三元组、panic 策略、代码模型、目标 feature、接口摘要和其他会影响调用约定或类型布局的条件。使用 exact-Rust 的 direct-pinned 或 kernel-symbol 时，装载器还要检查完整规范 ABI 和相应 rustc 条件。固定 Provider 线协议则与 Rust 内存布局解耦，通过明确的字节编码和结构版本传递，因此两类接口的兼容范围不同。

当前原生调用门只保存整数 ABI 所需的寄存器状态。若镜像声明 Float、Vector 或 SIMD 等尚未纳入调用门保存范围的 target feature，装载会在执行前拒绝。这种拒绝不是功能缺失被静默忽略，而是对当前故障恢复能力的诚实边界。只有调用门能完整保存和恢复新寄存器状态后，相关 feature 才能安全开放。

调用主体也进入信任判断。当前 principal 可以是常驻 Kernel、外部 UserAdmin，或带 Generation 的 ElmCell。管理命令、Provider 绑定和敏感接口根据主体、单元状态、能力策略和对象所有权共同决策。一个已签名模块不会自动获得管理权限；一个曾经取得权限的旧代际也不会在替换后继续使用。

#continued-table(
  "12-3",
  [装载证明的职责分层],
  (1.3fr, 2fr, 2.25fr),
  (
    table.cell(fill: soft-fill)[#text(weight: "bold")[证明层]],
    table.cell(fill: soft-fill)[#text(weight: "bold")[验证内容]],
    table.cell(fill: soft-fill)[#text(weight: "bold")[不能替代的检查]],
  ),
  (
    table.cell(fill: warm-fill)[摘要与 Seal],
    table.cell(fill: warm-fill)[输入范围、块内容和整镜像未被继续修改。],
    table.cell(fill: warm-fill)[不能证明发布者可信，也不能证明 ABI 可调用。],
    table.cell(fill: soft-fill)[签名与信任锚],
    table.cell(fill: soft-fill)[发布者身份、撤销状态和 release epoch。],
    table.cell(fill: soft-fill)[不能证明模块逻辑正确或没有内存错误。],
    table.cell(fill: handoff-fill)[结构与目标],
    table.cell(fill: handoff-fill)[EKI/EBI 结构、段权限、目标架构和入口范围。],
    table.cell(fill: handoff-fill)[不能自动授予运行能力或管理权限。],
    table.cell(fill: stable-fill)[Rust ABI],
    table.cell(fill: stable-fill)[接口摘要、编译条件、目标特性和规范签名。],
    table.cell(fill: stable-fill)[不能替代代际、策略、预算和生命周期检查。],
  ),
  kind: "表",
  continuation-kind: "续表",
  align: (left, left, left),
)

这一体系把“允许装载”拆成来源可信、结构合法、ABI 可调用、策略允许和资源可承受五个问题。每个问题都有独立失败原因，管理工具可以据此判断应当更新镜像、修正依赖、改变部署策略还是增加预算，而不是把全部失败压缩成一个模糊的加载错误。

== 12.8 生命周期状态机与两阶段操作

Cell 的状态不是展示字段，而是装载、调用、暂停、替换、卸载和故障隔离共同使用的门禁。正常装载依次经过 `Discovered -> Verified -> Loaded -> Linked -> Ready -> Active`。运行期还存在 `Quiescing`、`Paused`、`Detached`、`Retired`、`Faulted` 和 `Quarantined`。状态机只规定结构上允许的边，不能代替图、租约、策略、资源和钩子检查。

`Discovered` 表示来源或声明已经出现但尚未证明；`Verified` 表示 EBI、来源、目标和基础策略已通过；`Loaded` 表示镜像进入运行时所有权；`Linked` 表示段、重定位、根 API 和 import 已就绪；`Ready` 表示初始化成功、等待公开提交；`Active` 才允许普通调用和能力提供。把这些阶段拆开后，失败可以准确落在验证、映射、链接或初始化，而不是统一表现为“模块没有启动”。

`Quiescing` 停止接纳新工作并等待已有调用、租约和异步资源排空；完成后可以进入 `Paused` 或 `Detached`。Paused 保留镜像和状态，是可恢复事务；Detached 已从公开拓扑摘除，之后只能走向 Retired，不再恢复普通服务。故障先进入 Faulted，再由 Core 隔离为 Quarantined；隔离态只允许诊断和受控 Detach，不能重新获得普通原生能力。

Pause、Resume、Detach 和 Replace 都采用锁内预检、锁外钩子、锁内提交的结构。预检阶段读取一致的代际和策略 epoch，计算关系、租约、Provider 队列和资源 blocker；钩子阶段不持有 Core 全局锁，避免模块代码重入管理路径造成死锁；提交阶段重新校验关键版本，防止预检之后状态已被并发修改。若版本不再匹配，操作必须重新计划，而不能凭旧快照强行提交。

#pseudo-sample("12-2", [生命周期状态与事务边界], kind: "代码")[
  ```text
  Discovered -> Verified -> Loaded -> Linked -> Ready -> Active
                                                     |
                    +--------------------------------+
                    v
               Quiescing -> Paused -> Active
                    |          |
                    v          v
                Detached --> Retired

  允许阶段 -> Faulted -> Quarantined -> Detached -> Retired

  管理操作 = 锁内预检 -> 锁外 hook/排空 -> 锁内复核与提交
  ```
]

内建 `elm-mgr` 和 `eki` 受到额外保护，不能被普通请求当作一般动态单元退役。这个限制保证管理入口和当前镜像投影能力始终存在。普通单元的父子和依赖关系则决定操作顺序：依赖者、子单元或拓展项仍在活动时，目标单元的 Detach 计划会列出阻断项，而不是隐式级联销毁未知对象。

调用状态机时还要区分“允许迁移”和“能够提交”。例如 Active 到 Quiescing 是合法边，但如果策略不允许生命周期操作、当前 Generation 不匹配或执行 token 正被其他事务占用，预检仍会拒绝。状态枚举提供统一语言，事务条件提供真实安全性；二者缺一不可。

== 12.9 热替换、状态迁移与回滚

热替换不是把旧函数地址改成新地址，而是一项跨镜像、关系、资源和执行状态的事务。当前实现要求新旧单元名称和种类相符，公开 surface 与 imports/exports 满足兼容条件，并为新实现分配下一 Generation。外部 direct-pinned importer、无法迁移的资源、仍在执行的调用、繁忙租约或不兼容重定位都可能阻断替换。

替换先影子装载新镜像。新代际完成结构、证明、ABI、预算、重定位和模块描述符校验，import 暂存但不向其他单元公开。随后执行新代 `initialize`，再让旧代进入静默状态。若模块实现迁移钩子，旧代通过 `migrate_export` 导出状态，新代通过 `migrate_import` 接受；当前迁移状态硬上限为 64 KiB。开发框架的默认迁移钩子返回不支持，因此不能声称所有 ELM 天生具备状态迁移。

唯一提交点负责切换 Cell 的 Generation、Projection Source 引用、Provider backend、菜单、Binding 和 Lease 等运行事实。提交前失败时，新代被销毁，旧代恢复活动；迁移已开始但未提交时，可以调用 `migrate_abort` 撤销新代状态。提交后旧代不再可见，Core 执行旧代 `finalize` 并释放镜像。若旧代恢复本身失败，系统不会伪装成成功回滚，而会把单元转入隔离诊断。

#figure(caption: figure-caption("图", "12-4", [ELM 代际替换事务]))[
  #layer-card("旧代 Generation N", [仍对外服务；先检查依赖、租约、调用、资源和 direct-pinned importer], fill: soft-fill)
  #flow-arrow(label: "影子准备")
  #layer-card("新代 Generation N+1", [装载、链接、initialize 与可选 migrate_import，尚未公开], fill: warm-fill)
  #flow-arrow(label: "旧代 quiesce + 状态迁移")
  #layer-card("唯一提交点", [原子更新代际及公开关系；提交前可回滚，提交后旧代不可恢复服务], fill: handoff-fill)
  #flow-arrow(label: "收束旧实现")
  #layer-card("finalize 与镜像回收", [等待旧调用和受托资源闭合后释放，不让引用跨代延续], fill: stable-fill)
]

当前替换仍有刻意保守的限制。持有动态分配记账或 Owned Resource 的单元不能在无法证明迁移安全时直接替换；具有外部 direct-pinned 导入者时需要先解除或重建固定关系；设备对象的某些注册过程不可逆，也会阻断可恢复 Pause。网络栈替换后，旧 socket 进入稳定失效语义，而不是把 TCP 连接无损搬到新代。这样的“安全失败”比表面成功但留下跨代对象更符合内核生命周期要求。

热替换的价值也不只在不停机更新。它迫使接口设计明确哪些状态属于实现私有、哪些关系必须由 Core 切换、哪些对象不可跨代继承。即使实际部署很少执行 Replace，这套事务条件仍能暴露悬空回调、隐式全局状态和缺失资源所有权等工程问题。

== 12.10 同步与异步 Provider 执行

同步 Provider 在调用方上下文中完成验证和执行，适合短小、确定的管理动作。Core 根据 Binding 找到 Port 和后端，取得 Lease 活动引用，检查消费者与提供者代际，建立调用记账，再进入常驻回调或原生 handler。回复必须匹配 Binding ID、Call ID、状态和长度。执行结束后释放活动引用，确保撤销操作可以观察到调用已经退出。

原生 Provider 与常驻内核 Provider 使用同一调用帧，但执行边界不同。常驻后端由所属内核子系统直接负责；原生 handler 还要经过 ELM 调用门、独立栈、期限和 fault guard。注册原生 Provider 时，`handler_symbol` 必须能解析到当前镜像合法代码范围。没有 handler 的动态端口可以出现在拓扑和诊断视图中，但调用会返回 `ElmNativeTodo`，不会误入空函数地址。

较长或需要背压的工作进入异步 Provider executor。提交请求复用同一个 `ElmCallFrame`，只增加 timeout、结果保留 TTL 和 flags，不建立第二套业务 ABI。executor 分配 ticket，把任务置为 Queued；每 CPU worker 取得任务后转为 Running，完成后保存 Result。调用方可以按 ticket 轮询，领取结果后释放相应租约。结果长期无人领取时由 TTL 回收，结果环容量不足时也有明确淘汰和统计。

取消具有状态差异。仍在队列中的任务可以直接取消；已经运行的任务记录取消意图，由执行边界在支持的位置观察。timeout 既参与队列与结果管理，也能在原生调用超过期限时触发受控退出。取消并不等于任意时刻回滚模块已经完成的副作用，因此 Provider 契约仍需说明操作是幂等、可重试还是可能已经部分完成。

队列上限、单元 Provider queue 预算和并发调用预算共同形成背压。达到上限时提交被拒绝，而不是无界分配内存。异步任务持有 Provider 租约直到结果被领取、TTL 到期或被安全取消，因而卸载计划能够看到仍未闭合的工作。executor 的 per-CPU worker 减少单一管理线程瓶颈，但它不改变 Provider 自身的并发语义；需要串行的后端仍须在契约和实现中保持串行。

#continued-table(
  "12-4",
  [异步 Provider 任务的可见状态],
  (1.2fr, 2.1fr, 2.2fr),
  (
    table.cell(fill: soft-fill)[#text(weight: "bold")[状态]],
    table.cell(fill: soft-fill)[#text(weight: "bold")[运行时事实]],
    table.cell(fill: soft-fill)[#text(weight: "bold")[生命周期影响]],
  ),
  (
    table.cell(fill: warm-fill)[Queued],
    table.cell(fill: warm-fill)[ticket 已建立，调用帧和 provider lease 被队列持有。],
    table.cell(fill: warm-fill)[可以取消；队列占用与单元预算可见。],
    table.cell(fill: soft-fill)[Running],
    table.cell(fill: soft-fill)[worker 已进入后端，记录期限和取消意图。],
    table.cell(fill: soft-fill)[不能把取消等同于副作用回滚；排空必须等待执行边界退出。],
    table.cell(fill: handoff-fill)[Result],
    table.cell(fill: handoff-fill)[固定回复按 ticket 保留，并具有结果 TTL。],
    table.cell(fill: handoff-fill)[领取、过期或淘汰后才释放最后的队列资源。],
    table.cell(fill: stable-fill)[Canceled / Timed out],
    table.cell(fill: stable-fill)[记录终止原因和可观察状态。],
    table.cell(fill: stable-fill)[调用方可区分主动取消、期限到达和后端业务错误。],
  ),
  kind: "表",
  continuation-kind: "续表",
  align: (left, left, left),
)

当前模型已经定义更多并发和背压模式，但并非所有组合都由通用调度器自动执行。文档以真实 executor 的 ticket、队列、in-flight、取消、timeout 和 TTL 为实现范围，不把尚未接通的策略枚举当成现成功能。

== 12.11 策略、预算与资源所有权

ELM 的权限不是一个全局“可信模块”布尔值。Cell policy 按动作控制生命周期、绑定、Provider、事件、拓展、原生执行、资源与策略更新、观测和管理能力。父单元委派给子单元的是能力上限，子策略只能保持或缩小，不能自行扩大。management capability 不自动继承。`DENY_CHILD_ESCALATION` 阻止后续子级提升权限，`AUDIT_ALL` 要求更完整记录，`LOCKED` 一旦提交便不可逆地冻结敏感策略变化。

策略更新带有 Generation 和 policy epoch，用于乐观并发校验。更新期间 execution token 阻止状态与正在执行的原生调用发生撕裂。Core 仍然在每次敏感调用处检查当前主体和当前策略，因此一个早先取得的函数表或 Client 不能成为永久通行证。策略设计的重点不是增加更多位，而是让“谁在何时允许做什么”与代际和审计记录处于同一个事实域。

资源预算解决的是数量和容量上限。当前预算覆盖 Provider 端口、Provider 队列、事件订阅、pending load、原生镜像、原生 fault、审计记录、并发调用、镜像字节、原生栈字节和动态分配字节。父单元必须为仍存活的直接子单元保留预算，不能把同一份容量重复委派；缩减预算时也必须覆盖当前用量和子级保留量。硬资源在提交前拒绝超额请求，并形成 `RESOURCE_QUOTA` blocker。

动态内存记账通过 allocator 提供的通用回调表接入，allocator 不反向依赖 ELM。原生调用热路径更新独立的定长资源账本，不需要取得 Core 拓扑锁，也不在记账过程中继续分配内存。这个结构避免为了统计模块内存而让分配器与管理器形成循环依赖，同时使动态分配峰值和配额拒绝可以归属到当前 Cell。

CPU 时间采用不同语义。运行时记录每次调用和核算周期的 CPU 时间、超额次数与总量，但当前没有把超额 Cell 放入调度节流队列，也不会仅因软阈值超出就把模块判为故障。它是可观测和后续调度策略的依据，不应描述为已完成的硬 CPU 隔离。原生 stack、并发调用和动态内存则已经进入实际准入记账。

Lease、Budget 和 Owned Resource 分别回答不同问题。Lease 表示一项访问权当前是否仍被引用；Budget 表示一个单元最多能占用多少；Owned Resource 表示模块创建、但必须由常驻子系统协助停止和回收的长期异步对象。当前 Owned Resource 类别包括任务、定时器、工作项、回调、IRQ 回调、异步请求、设备和自定义对象。

#pseudo-sample("12-3", [长期资源的受控退役顺序], kind: "代码")[
  ```text
  Detach:
    stop admission -> quiesce -> cancel -> drain -> release

  Pause:
    按注册逆序 suspend
    任一步失败时，以对偶操作回滚已暂停对象

  Resume:
    按依赖正序 resume

  不变量:
    操作表位于常驻内核子系统，不能指向即将卸载的 ELM 镜像
  ```
]

Owned Resource 操作表包含 `suspend/resume/quiesce/cancel/drain/release`。暂停是可回滚事务，退役则按逆序停止和释放。回调入口必须由常驻内核子系统提供，因为把清理函数放在待卸载镜像中会形成“必须先调用它才能卸载，但调用前镜像可能已经失效”的循环。设备 kernel-symbol API 可以把长期对象登记到当前 Cell 和 Generation；然而部分设备注册尚不具备可逆 shadow registration，持有这类对象时 Pause 会明确失败，Detach 才能走不可逆回收链。

资源管理最终服务于可解释的拒绝。一个单元不能暂停时，管理面应当区分活动 Lease、运行调用、排队 Provider、不可逆设备对象、子单元依赖或预算事务冲突。把这些状态汇总成 blocker，既避免“一直 Busy 但不知道为什么”，也为后续自动编排提供可计算输入。

== 12.12 原生执行、故障恢复与隔离边界

每次原生 ELM 调用使用独立 64 KiB 栈，两端设置 guard page。进入调用前，`ElmGuard` 记录 Cell、执行阶段、期限、代码和镜像范围、宿主允许范围、固定恢复 PC 与恢复 SP。生命周期 hook、entry、Provider handler、snapshot、migration、managed call 和 Mixin 都通过原生调用门进入，从而共享故障记录与退出规则。

同步 fault 若命中活动 ELM guard，架构 trap 路径不会沿故障现场的 `ra` 返回模块，而是把 trap frame 的 PC、SP 和返回值改写为固定恢复出口。即使异常发生在模块深层调用中，也能回到常驻内核已知位置。模块 `panic!` 使用专用 panic 恢复出口。原生调用期间定时器仍可运行；超过执行 deadline 时，timer trap 可以请求中止并重定向到受控退出路径，避免无限循环只能依靠调用返回后的软计时发现。

故障记录保存 Cell、Generation、阶段、fault PC、地址、异常码、恢复 PC 和恢复 SP。Core 增加该单元的 native fault 用量，设置 isolated 标志并形成 blocker。隔离后的 Cell 不能继续注册或调用 Provider，也不能解析新的 native import，只保留诊断和退役能力。这样，局部故障不会继续通过普通能力路径扩散。

取消和 fault 都要从架构现场回到固定边界，但两者不是同一错误。取消来源于管理或 timeout 决策，fault 来源于同步异常，panic 来源于语言运行时。分别记录原因后，调用方才能判断是否可重试，管理器也能决定只终止当前调用、隔离整个 Cell，还是要求人工诊断。

这仍然是共享特权地址空间中的受控恢复边界，不是 MMU 沙箱。调用门可以收束受支持的同步异常、panic 和超时，却不能撤销模块在故障前已经完成的任意内存写入，也无法保证恶意代码不会访问同一地址空间中的其他对象。Kernel Provider 回调也不属于原生 ELM 调用门，其故障边界由所属常驻子系统负责。文档必须保留这一限制，否则会把“能从部分 fault 恢复”错误提升成“内存完全隔离”。

#figure(caption: figure-caption("图", "12-5", [原生调用门的故障收束路径]))[
  #layer-card("常驻内核调用方", [完成代际、策略、预算和调用准入检查，保存整数 ABI 边界], fill: soft-fill)
  #flow-arrow(label: "进入 64 KiB 隔离栈")
  #layer-card("ElmGuard + 原生 ELM", [双 guard page；记录代码、栈、期限和固定恢复上下文], fill: warm-fill)
  #flow-arrow(label: "正常返回 / fault / panic / timeout")
  #layer-card("架构固定恢复出口", [重写 PC、SP 和返回值，不依赖故障现场 ra], fill: handoff-fill)
  #flow-arrow(label: "形成运行事实")
  #layer-card("Fault dump + Quarantine", [记录故障位置、阶段和代际，阻断新原生能力并进入诊断], fill: stable-fill)
]

原生隔离与 Rust 类型安全互为补充。Rust 能减少普通代码中的悬空引用和数据竞争，但 ELM 包含 `unsafe`、汇编、设备访问和 FFI，不能只依靠语言保证。调用门处理控制流恢复，ABI 指纹处理调用约定，资源协议处理长期对象，策略处理授权，几者共同构成当前可达到的故障边界。

== 12.13 Mixin 与结构化拓展点

模块之间除了服务调用，还可能需要在既有流程的指定位置附加行为。ELM 用 Extension Point、Extension 和 Mixin 描述这类关系。目标单元主动公开拓展点及契约，拓展单元按声明挂接；BindingGraph 记录目标、拓展者、代际和契约。与扫描符号或任意改写指令相比，显式拓展点让可插入位置、调用顺序和撤销条件在执行前可见。

Provider 形式的 Mixin 可以在固定 frame 上组织 ingress、substitute、egress 和 observe 阶段。入口阶段可检查或调整请求，替代阶段可提供结果，出口阶段处理回复，观察阶段只记录而不改变主结果。链中每个成员都具有优先级、契约和所有者，挂接与卸载进入普通拓展关系和租约管理。一次调用因此可以还原经过了哪些拓展项，而不是只看到某个全局回调列表。

仓库还实现了直接挂接常驻内核导出站点的 kernel-symbol Mixin。装载和挂接时校验 API profile、源码摘要、函数、站点、frame 和 handler 摘要；热路径读取已经提交的原子不可变路由，不在每次调用时重新构建链。当前稳定范围主要是函数 `HEAD` 与 `RETURN` 站点，支持 inject、modify_arg、modify_return 和 overwrite。MIR 级的 modify_local、redirect 与 wrap_operation 尚未实现，宏会在编译期拒绝这些声明。

Mixin 的安全性仍来自受限位置和明确契约，而不是“钩子”这个名称本身。若允许扩展任意局部变量、任意控制流边或未声明内核地址，Core 就无法验证现场布局，也无法可靠回滚。当前实现宁可缩小可挂接范围，也不把设计文档中的完整目标当成现有能力。对于需要更复杂组合的功能，可以先用 Provider 或明确的内核 API 表达。

Mixin 与 Provider 也不能互相替代。Provider 表示一个可发现服务，消费者主动发起调用；Mixin 表示目标流程主动开放的插入位置，调用由宿主流程触发。两者都使用身份、契约、代际、策略和 Trace，但业务关系不同。把它们统一放入 ELM 图模型后，管理器可以同时看到“谁提供服务”和“谁改变了某条执行链”。

== 12.14 管理 ABI 与可复核证据

外部工具通过 `sys_elm_ctl` 进入 ELM 管理面。控制链为 `elmctl -> sys_elm_ctl -> MGR_CALL -> elm-mgr -> Core`。管理协议使用固定布局请求和回复，检查 ABI 版本、结构大小、保留字段、标志位、记录数、乘法溢出和总缓冲区长度。可变长查询由调用方提供工作缓冲区；容量不足时返回所需尺寸，不把截断或畸形的半页结果交给管理程序。

管理动作可以按职责分为拓扑与健康查询、生命周期与替换、Provider 与绑定、事件订阅、策略预算与信任、镜像会话与诊断六组。`elm::management::Client` 为 Manager 提供类型化包装，隐藏裸分发表和输出指针。当前 v1 是内核内部使用的固定布局协议，但尚未作为长期稳定的公共 ABI 正式冻结，后续演进仍要依靠版本协商、结构大小和保留字段维持兼容。

ELM 的观测面并非重复记录同一条日志。Snapshot 描述当前有哪些 Cell、关系、端口、执行和资源；Event 按序列描述状态变化；Audit 记录主体、管理动作、结果和阻断原因；Trace 覆盖 lifecycle、Provider、Mixin、Replace、policy 与 resource 等具体路径；Journal 保存需要校验顺序的关键管理事实。它们通过 Cell、Generation、Binding、ticket 和序列号彼此关联，能够从一次失败回到当时的拓扑和资源条件。

Journal 当前使用 240 字节固定记录、256 项内存环和 SHA-256 前后哈希链。接口支持可选或强制持久后端，也支持启动回放；但仓库中没有生产持久后端注册，默认仍是易失模式。现有回放主要恢复防回滚 trust epoch，不会重建 Cell、队列、模块内存或完整拓扑。Snapshot Read 同样是只读事实快照，不是可以恢复执行现场的持久检查点。

`/sys/kernel/elm` 提供只读诊断视图。当前目录实际挂载 19 个节点：`core`、`policy`、`health`、`menu`、`topology`、`ports`、`providers`、`bindings`、`events`、`audit`、`api`、`trust`、`projection-sources`、`journal`、`executions`、`owned-resources`、`resource-accounting`、`workers` 和 `diagnostics`。sysfs 不承担控制入口，避免文本写操作绕过固定管理协议；更细的 fault、native capability 和 TODO 事实通过管理 ABI 或综合诊断视图读取。

#continued-table(
  "12-5",
  [ELM 运行证据的职责],
  (1.15fr, 2.25fr, 2.25fr),
  (
    table.cell(fill: soft-fill)[#text(weight: "bold")[证据]],
    table.cell(fill: soft-fill)[#text(weight: "bold")[主要问题]],
    table.cell(fill: soft-fill)[#text(weight: "bold")[实现边界]],
  ),
  (
    table.cell(fill: warm-fill)[Snapshot],
    table.cell(fill: warm-fill)[当前有哪些对象、关系、状态和资源用量。],
    table.cell(fill: warm-fill)[瞬时只读视图，不授予所有权，也不是持久检查点。],
    table.cell(fill: soft-fill)[Event],
    table.cell(fill: soft-fill)[从游标之后发生了哪些拓扑与状态变化。],
    table.cell(fill: soft-fill)[订阅具有租约、容量、游标和丢失统计。],
    table.cell(fill: handoff-fill)[Audit / Trace],
    table.cell(fill: handoff-fill)[谁发起动作、为何拒绝、运行经过哪些阶段。],
    table.cell(fill: handoff-fill)[环形记录受预算约束，需要按身份和序列关联。],
    table.cell(fill: stable-fill)[Journal],
    table.cell(fill: stable-fill)[关键事实顺序与哈希链是否完整。],
    table.cell(fill: stable-fill)[默认易失；持久性取决于外部注册的后端。],
  ),
  kind: "表",
  continuation-kind: "续表",
  align: (left, left, left),
)

这套证据链的直接用途是解释拒绝。Replace 返回 Busy 时，管理工具可以继续读取计划 blocker，区分依赖者、租约、正在运行的调用、Provider 队列、Owned Resource 或 ABI 条件，再用 Trace 和资源快照核对判断依据。调试者得到的是能够复核的状态链，而不是只能根据一条错误日志猜测。

== 12.15 Rust 开发框架与构建部署

动态 ELM 通过 `ElmModule` trait 和 `#[elm::module]` 生成统一描述符。开发者必须提供 `create`、`initialize` 和 `finalize`，并可按需要实现 `quiesce`、`pause`、`resume`、迁移导出、迁移导入、迁移撤销和激活后 `entry`。默认迁移钩子返回不支持，因此框架不会把没有迁移逻辑的组件伪装成可热迁移组件。

`ModuleSlot` 串行化实例构造、活动借用和生命周期转换。实例只有在 `initialize` 成功后才以 Release 语义发布；初始化失败时先尝试 `finalize`，再恢复为空槽。转换期间新活动借用被拒绝，钩子结束后才恢复 Active 或完成销毁。这个局部状态机与 Core 的 Cell 状态机分工协作：Core 管全局拓扑事务，开发框架管一个 Rust 实例何时可被借用。

Provider、import、export、payload 和 Mixin 使用属性宏声明。固定 payload 产生确定的小端线编码，不依赖 Rust 内存布局；exact-Rust import/export 则生成规范签名和接口摘要。模块描述信息位于 `.elm.meta`，不进入可执行 `PT_LOAD` 段。统一的 `__elm_module_descriptor_v1` 提供实例大小、对齐与生命周期入口，宿主还会检查入口地址确实位于镜像代码范围内。

`cargo elm build` 负责构建位置无关镜像、收集 metadata、检查重定位、生成 EKI 和签名材料。RISC-V64 与 LoongArch64 产物必须分别匹配目标内核的 ABI 指纹，不能跨架构复用。外部 ELM 的完整调试符号归档和公共发布仓库仍是后续方向，当前不能描述为已经形成稳定生态。

`drivers/Modules.toml` 与配置系统定义 `y/m/n` 三种构建语义。`m` 生成受管 EKI，运行时具有 Cell、Generation、策略、审计和动态生命周期；`y` 把同一业务源码编译为集成归档，通过 initcall 进入常驻内核，不具有动态 Cell、Generation、Provider 或 Mixin 语义；`n` 不构建、不打包并清理陈旧产物。实际部署由 `.config` 和模块清单共同决定，不能仅凭 `Elm.toml` 推断组件必然动态运行。

#figure(caption: figure-caption("图", "12-6", [同一组件源码的构建分流]))[
  #layer-card("ElmModule 业务源码", [Rust 实现、生命周期、imports/exports、Provider 与 metadata], fill: soft-fill)
  #flow-arrow(label: "Modules.toml + .config")
  #layer-card("mode = m", [PIE -> EKI -> 签名与 EBI 投影；运行时受管 Cell], fill: handoff-fill)
  #flow-arrow(label: "或选择静态集成")
  #layer-card("mode = y", [集成归档 -> initcall；没有动态 Cell 和代际语义], fill: warm-fill)
  #flow-arrow(label: "未选择")
  #layer-card("mode = n", [不构建、不打包，并移除陈旧模块产物], fill: stable-fill)
]

开发框架让普通模块作者尽量使用常规 Rust 类型和方法，但便利性不能模糊边界。直接接口必须来自发布的 Kernel API Profile，动态服务必须声明契约，长期对象必须登记所有者，生命周期钩子必须允许失败。宏承担重复布局和证明材料生成，不会替开发者证明业务操作可回滚，也不会把不安全设备访问自动变成沙箱内操作。

== 12.16 工程落地与当前实现范围

网络组件是当前 ELM 机制最完整的工程落地之一。`drivers/Modules.toml` 把 `net.stack`、`net.loopback`、`net.virtio` 以及 VirtIO framework、block 等组件默认配置为 `m`；实际镜像仍以当前 `.config` 为准。在模块部署中，`net.stack` 持有 FlowShard、协议状态、控制面和定时器，常驻 Host 保留 socket facade 和 Generation 生命周期，通过私有 `direct-pinned` 的 shard-turn 与 local-turn 调用当前实现。

`net.stack` 初始化时建立按活动 CPU 分片的状态，构造两个带精确契约的 pinned endpoint，再向常驻 registrar 提交当前代际。调用 frame 自带 Generation 和结构校验，常驻 broker 为每 CPU 建立 pinned call slot，调用忙时返回明确状态，不把管理锁带入协议处理。静默钩子先设置拒绝新 turn 的标志；终结路径再调用 `begin_remove` 并销毁该代际的协议状态。

`net.loopback` 和 `net.virtio` 也实现 `ElmModule` 生命周期及私有 direct-pinned queue endpoint。VirtIO 网络模块初始化时注册 PnP driver，quiesce 停止活动队列，finalize 分离设备并注销驱动；回环模块用相同的队列边界处理本机报文。它们说明 ELM 不是只管理一个名称：协议状态、驱动注册、队列入口和退出顺序都落实到具体代际。

这些网络热路径目前不经过通用 `packet.rx/packet.tx` Provider，相关规格在网络库中仍标为 TODO。旧 socket 也不会在协议栈代际退出后无损迁移，而是进入稳定错误或挂断语义。外部 direct-pinned importer、动态分配和 Owned Resource 还会阻断通用 Replace，所以“网络以 ELM 运行”不等于“活动网络连接可以任意热替换”。第十三章将继续讨论 FlowShard、套接字、路由和报文所有权。

设备和 VFS 有部分 Provider 实现。`device.discovered@1`、`device.claim@1` 与 `vfs.lookup@1` 存在代码或测试路径，但生产启动流程没有统一调用 `register_kernel_provider_specs` 自动汇聚全部候选，准确表述应是“子系统显式注册后可用”。当前 VFS lookup 只覆盖有限 cwd 和路径规范，VFS read/write 尚未接入；IRQ、DMA、MMIO、块 I/O 与 packet Provider 仍是 TODO。

#continued-table(
  "12-6",
  [ELM 当前完成度],
  (1.25fr, 2.25fr, 2.2fr),
  (
    table.cell(fill: soft-fill)[#text(weight: "bold")[层级]],
    table.cell(fill: soft-fill)[#text(weight: "bold")[代表机制]],
    table.cell(fill: soft-fill)[#text(weight: "bold")[文档口径]],
  ),
  (
    table.cell(fill: stable-fill)[运行闭环],
    table.cell(fill: stable-fill)[Cell/Generation、状态机、EKI 投影、原生装载、调用分流、策略预算、故障恢复和网络 direct-pinned。],
    table.cell(fill: stable-fill)[可以描述实际对象、调用链和失败语义，同时保留共享地址空间限制。],
    table.cell(fill: warm-fill)[受限实现],
    table.cell(fill: warm-fill)[设备/VFS Provider、可逆资源暂停、持久 Journal 接口、并发与背压模型。],
    table.cell(fill: warm-fill)[需要显式注册或特定条件，不能描述为所有启动和所有对象普遍生效。],
    table.cell(fill: handoff-fill)[后续方向],
    table.cell(fill: handoff-fill)[packet/IRQ/DMA/MMIO/block Provider、soyo、完整 MIR Mixin、公共发布和调试体系。],
    table.cell(fill: handoff-fill)[只描述目标和已有接口，不使用“已经支持”或“当前热路径采用”。],
  ),
  kind: "表",
  continuation-kind: "续表",
  align: (left, left, left),
)

当前实现已经使装载、调用、暂停、故障和退役进入同一套责任模型，但完成度并不均匀。CPU 预算只记账而不节流，Journal 默认没有持久后端，设备 Pause 受不可逆注册限制，soyo 和完整 MIR Mixin 尚未实现。把这些边界与已运行机制并列写出，既避免用目标态替代现状，也使后续工作具有明确接口位置。

== 12.17 工程技术总结

ELM 的工程贡献不是增加一种模块后缀，而是把“扩展代码”改造成“受管内核能力”。代码进入前有来源、结构和 ABI 证明；运行时有 Cell、Generation、策略和预算；能力连接有契约、Binding 和 Lease；生命周期变化有预检、提交和回滚；故障与管理动作则留下能够互相核对的证据。这些对象共同承载可拓展性、安全性、可维护性、可追踪性与可验证性。

这套结构同时回答了治理与热路径性能之间的矛盾：动态服务使用 Provider，逐次核验的接口使用 managed import，固定 ABI 和代际的数据面使用 direct-pinned，常驻能力使用 kernel-symbol。管理器不进入每个网络包或普通函数调用，证明集中在装载、绑定和状态切换处。

ELM 没有消除扩展的固有风险：共享地址空间中的写入无法由调用门撤销，业务回滚和状态迁移也仍取决于组件实现。当前选择是在无法证明安全时明确拒绝，而不是用表面成功掩盖悬空引用和半提交状态。扩展因而能够被发现、约束、调用、观察和退役，未完成的能力也能被准确标为受限实现或后续方向。这是 ELM 从设计概念走向工程系统的核心结论。
