#import "../config.typ": project-name
#import "../styles/diagram.typ": flow-arrow, flow-node, layer-card
#import "../styles/figure.typ": continued-table, figure-caption, pseudo-sample
#import "../styles/tokens.typ": handoff-fill, soft-fill, stable-fill, warm-fill

= 第三章 设备模型与驱动框架

在第二章中，内存管理子系统围绕物理页和虚拟地址空间展开，并进一步讨论了堆分配与 Slab 缓存。那一章的核心问题，是如何在相对同构的内存资源之上建立稳定的分配与回收机制。本章问题的性质发生了根本性变化。设备抽象面对的是硬件异构性。串口使用字节流，块设备使用可寻址块，网卡使用数据包，RTC 设备则围绕时间和中断工作。PCI 主桥与 IRQ 控制器又承担平台基础设施的角色。它们的访问单位不同，控制命令不同，生命周期也不同。中断、DMA、MMIO 和固件描述对每类设备的约束也不相同。若设备模型过早收敛到单一 I/O 接口，很多设备的真实能力会被压缩。若完全放开，每个驱动自行维护注册、命名、热插拔和用户态投影，全局生命周期又会难以推理。

我们在设备模型中采用了以设备能力（`DeviceFunction`）为中心的多轨结构。这里的设备能力表示驱动向内核开放出来的能力，并不等同于某一种固定设备文件。字符设备、块设备、网络设备和 RTC 设备都是设备能力的具体落实。UART 设备、VirtIO 块设备、VirtIO 网络设备与 LS7A RTC 设备等驱动，则继续把这些设备能力落实到具体硬件上。这样一来，设备模型形成了从抽象到具体的层次关系。最上层关注开放能力和生命周期。中间层保留各类设备的类型化语义。最下层处理总线、寄存器、中断和队列等硬件细节。

本章主要说明这一结构的设计目标、核心机制和关键路径。为了保持叙述边界清晰，设备管理被拆成四个问题来讨论。第一，硬件如何被发现并绑定驱动。第二，驱动如何把能力登记到全局设备能力注册表。第三，各类设备能力如何保留自己的类型化接口。第四，设备能力如何被投影到 `/dev`、sysfs 系统文件系统和 procfs 进程文件系统这样的用户可见视图中。这四个问题之间存在明确的信息流。底层硬件事实向上被整理为 PnP 设备。PnP 设备经驱动探测发布设备能力。设备能力再被观察者投影为用户态名字空间中的节点。

== 设计目标与约束

设备模型需要同时满足三类约束。第一类约束来自硬件。不同设备对资源的依赖差异很大。串口可能只需要 MMIO 和中断。块设备通常需要队列、DMA 或轮询推进机制。网卡需要与协议栈建立接口关系。RTC 需要同时表达时间读写、告警和周期中断。第二类约束来自内核内部。文件系统希望直接拿到块设备对象，网络协议栈希望直接拿到网络接口对象，VFS 希望把部分设备映射为文件节点。第三类约束来自用户态兼容。用户态仍然按照 `/dev`、设备号、`ioctl` 系统调用和 sysfs 系统文件系统属性等传统方式观察设备。设备模型必须提供这些视图，同时避免让这些视图反向决定底层设备身份。

我们将设计目标整理为以下几点。

#continued-table(
  "3-1",
  [设备模型的设计目标],
  (1.1fr, 2.1fr, 2.1fr),
  (
    table.cell(fill: soft-fill)[#text(weight: "bold")[目标]],
    table.cell(fill: soft-fill)[#text(weight: "bold")[含义]],
    table.cell(fill: soft-fill)[#text(weight: "bold")[工程约束]],
  ),
  (
    table.cell(fill: warm-fill)[生命周期统一],
    table.cell(fill: warm-fill)[不同总线和不同设备类型都进入统一的发现、探测、移除、失效流程。],
    table.cell(fill: warm-fill)[驱动移除时必须先阻止新访问，再排空旧请求，最后释放硬件资源。],
    table.cell(fill: soft-fill)[能力表达开放],
    table.cell(fill: soft-fill)[字符、块、网络、RTC 等设备能力通过同一个设备能力注册表发布。],
    table.cell(fill: soft-fill)[核心注册表只理解类别、名称和生命周期入口，不嵌入每类设备的 I/O 细节。],
    table.cell(fill: handoff-fill)[类型语义保留],
    table.cell(fill: handoff-fill)[每类设备能力保留自己的类型化对象和控制请求。],
    table.cell(fill: handoff-fill)[网络设备不被强制映射为字符流，块设备不丢失异步提交语义。],
    table.cell(fill: stable-fill)[用户视图解耦],
    table.cell(fill: stable-fill)[`/dev`、sysfs 系统文件系统、procfs 进程文件系统观察设备能力事件后生成视图。],
    table.cell(fill: stable-fill)[投影失败只能影响用户可见性，不能反向破坏底层探测事务。],
  ),
  kind: "表",
  continuation-kind: "续表",
  align: (left, left, left),
)

这些目标决定了设备模型不能以传统的字符设备和块设备作为根抽象。字符和块仍然重要，但它们只是两类已经落实的设备能力。网络设备和 RTC 设备也需要同等地位。未来可能加入的输入设备、图形设备和声音设备，也应当能够在不修改 PnP 核心与 devtmpfs 设备文件系统核心的情况下接入。我们因此把设备管理的根概念从“设备文件类型”上移到“开放能力类型”，用设备能力表达所有已发布能力的共同边界。

== 总体分层结构

设备模型可以分为五层。发现层把 DTB、ACPI 和 PCI 等来源中的硬件事实整理出来。PnP 层负责设备身份、驱动匹配、资源归属、状态转换和移除顺序。设备能力层维护全局开放能力注册表。类型化设备层保存字符、块、网络和 RTC 等具体 I/O 语义。用户态投影层把设备能力映射为 `/dev` 节点、sysfs 条目和 procfs 诊断视图。

#figure(caption: figure-caption("图", "3-1", [设备模型的单向信息流]))[
  #layer-card("用户态投影层", "devtmpfs、sysfs、procfs 订阅设备能力事件并生成用户可见视图", fill: stable-fill)
  #flow-arrow(label: "观察注册和注销事件")
  #layer-card("设备能力层", "设备能力注册表维护开放能力快照和生命周期事件", fill: soft-fill)
  #flow-arrow(label: "探测成功后发布能力")
  #layer-card("类型化设备层", "字符、块、网络、RTC 设备对象保存具体 I/O 语义", fill: handoff-fill)
  #flow-arrow(label: "驱动根据硬件资源构造")
  #layer-card("PnP 与发现层", "固件和总线创建 PnP 设备，PnP 负责匹配、状态机和资源归属", fill: warm-fill)
]

这条信息流具有两个重要性质。其一是单向依赖。PnP 层不调用 devtmpfs 设备文件系统。设备能力注册表不依赖 sysfs 系统文件系统或 procfs 进程文件系统。用户态投影只是观察设备能力事件。其二是事务边界清晰。驱动探测的成功条件只覆盖硬件初始化、类型化对象构造和设备能力注册表注册。`/dev` 节点创建属于后续投影动作。这样可以避免一个用户态视图错误破坏底层设备生命周期，也可以允许设备先于 devtmpfs 设备文件系统挂载完成注册。

我们在实现中故意让注册表保存 `Arc<dyn DeviceFunction>` 类型表达式。这意味着 PnP 核心不需要知道字符设备对象、块设备对象、网络设备对象或 RTC 设备对象的内部结构，只需要对所有设备能力调用同一组生命周期方法。需要具体类型的上层模块通过 `as_any` 方法和 `function_as` 方法显式恢复类型。这种显式恢复机制使通用路径保持稳定，也让专用路径在代码层面暴露自己的类型依赖。

== 设备能力核心抽象

设备能力是设备模型中最小的开放能力接口。它不定义读写方法，也不定义 `ioctl` 命令，只定义设备能力类别、内部名称、生命周期标记、I/O 排空和类型恢复。这样设计的关键原因，是读写语义并非所有设备共有。字符设备适合流式读写，块设备适合异步块请求，网络设备适合数据包队列，RTC 设备适合时间与告警控制。把这些语义放入同一个特征接口，会使核心接口不断膨胀。

#pseudo-sample("3-1", [设备能力核心接口], kind: "代码")[
  ```rust
  trait DeviceFunction: Send + Sync {
      fn class_id(&self) -> DeviceClassId;
      fn dev_name(&self) -> &str;
      fn mark_gone(&self);
      fn drain_io(&self) {}
      fn as_any(&self) -> &dyn core::any::Any;
  }
  ```
]

`class_id` 方法返回设备能力的类别标识。当前内置类别包含 `char`、`block` 和 `rtc`，网络设备在网络子系统中通过 `DeviceClassId::new("net")` 注册为独立类别。`dev_name` 方法返回设备能力注册表的内部名称，它与 `/dev` 下的文件名没有必然对应关系。`mark_gone` 方法用于热插拔移除阶段，调用后旧句柄仍可能因为引用计数继续存在，但新 I/O 应当尽快返回设备不可用。`drain_io` 方法用于排空已经提交的异步 I/O，字符设备和 RTC 这类没有异步请求队列的设备能力可以使用默认空实现。`as_any` 方法是类型恢复入口，新设备类型通过它接入通用注册表。

设备能力注册表使用 `class_id + dev_name` 作为唯一键。这个选择比单纯使用名称更稳健，因为不同类别的设备可以拥有相同的自然编号，例如 `net0`、`rtc0`、`tty0` 的命名空间本来就属于不同类别。它也比在注册表中维护多个具体类型列表更开放，因为新增设备能力类别时不需要修改核心结构。

#pseudo-sample("3-2", [设备能力注册的核心流程], kind: "代码")[
  ```rust
  fn register_function(func: Arc<dyn DeviceFunction>) -> Result<(), FunctionRegistryError> {
      let key = (func.class_id(), func.dev_name());
      let mut list = FUNCTIONS.lock();

      if list.iter().any(|existing| {
          existing.class_id() == key.0 && existing.dev_name() == key.1
      }) {
          return Err(FunctionRegistryError::NameExists);
      }

      list.try_push(func.clone())?;
      drop(list);

      publish_function_event(DeviceFunctionEventKind::Registered, func);
      Ok(())
  }
  ```
]

这里有一个容易忽略的细节。事件发布必须发生在注册表锁释放之后。devtmpfs 设备文件系统、sysfs 系统文件系统和 procfs 进程文件系统都可能在回调中读取设备能力快照、创建索引节点或记录投影状态。如果注册表在回调过程中仍然持锁，观察者一旦回到设备核心查询状态，就可能形成锁顺序反转。我们因此选择了快照式遍历和锁外回调。这个选择增加了少量复制成本，但换来了更清晰的并发边界。

== 设备能力多轨落实

设备能力多轨结构的核心，是把通用生命周期和类型化 I/O 分开。所有设备能力都有类别、名称和移除入口，只有具体类别才拥有读写、控制、队列、统计等语义。表 3-2 列出了当前已经落实的主要轨道。

#continued-table(
  "3-2",
  [设备能力轨道与类型化对象],
  (1fr, 1.4fr, 2.4fr, 2.2fr),
  (
    table.cell(fill: soft-fill)[#text(weight: "bold")[轨道]],
    table.cell(fill: soft-fill)[#text(weight: "bold")[类型化对象]],
    table.cell(fill: soft-fill)[#text(weight: "bold")[核心语义]],
    table.cell(fill: soft-fill)[#text(weight: "bold")[主要消费者]],
  ),
  (
    table.cell(fill: warm-fill)[字符],
    table.cell(fill: warm-fill)[`CharDevice`],
    table.cell(fill: warm-fill)[非阻塞读写、刷新、`poll` 系统调用、TTY 控制、控制台语义。],
    table.cell(fill: warm-fill)[devtmpfs 设备文件系统、TTY 层、控制台和通用字符设备适配器。],
    table.cell(fill: handoff-fill)[块],
    table.cell(fill: handoff-fill)[`BlockDevice`],
    table.cell(fill: handoff-fill)[Bio 请求异步提交、几何信息、能力限制、同步等待适配。],
    table.cell(fill: handoff-fill)[文件系统、块设备挂载路径、devtmpfs 块设备节点。],
    table.cell(fill: soft-fill)[网络],
    table.cell(fill: soft-fill)[`net::NetDevice`],
    table.cell(fill: soft-fill)[接口编号、链路状态、MTU、MAC、收发统计和协议栈轮询。],
    table.cell(fill: soft-fill)[网络协议栈、套接字层、网络控制接口。],
    table.cell(fill: stable-fill)[RTC],
    table.cell(fill: stable-fill)[`RtcDevice`],
    table.cell(fill: stable-fill)[时间读写、告警配置、周期中断、RTC `ioctl` 兼容。],
    table.cell(fill: stable-fill)[时间子系统、devtmpfs RTC 适配器、用户态 RTC 程序。],
  ),
  kind: "表",
  continuation-kind: "续表",
  align: (left, left, left, left),
)

=== 字符设备能力

字符设备能力面向顺序流式设备。串口、控制台、TTY、随机数设备都属于这一类。字符设备的关键设计点，是把非阻塞 I/O、阻塞全量写、刷新和 `poll` 系统调用分开。控制台输出需要尽量避免长时间阻塞，TTY 输入需要支持等待队列和事件唤醒，用户态文件接口又需要把返回值和错误码映射到 POSIX 语义。把这些语义混在一个方法里会增加调用方判断成本，因此我们在字符设备驱动接口中保留了相对清晰的分工。

#pseudo-sample("3-3", [字符设备驱动接口], kind: "代码")[
  ```rust
  trait CharDriver: Send + Sync {
      fn write(&self, buf: &[u8]) -> Result<usize, CharIoError>;
      fn read(&self, buf: &mut [u8]) -> Result<usize, CharIoError>;
      fn poll_read(&self) -> bool;
      fn poll_add_waiter(&self, task: &Arc<Task>, read: bool, write: bool) -> bool;
      fn poll_remove_waiter(&self, task: &Arc<Task>);
      fn flush(&self) -> Result<(), CharIoError>;
      fn control(&self, req: CharControlRequest) -> Result<CharControlResponse, ControlError>;
      fn as_any(&self) -> &dyn Any;
  }
  ```
]

字符设备对象自身保存状态位。设备仍处于 `active` 状态时，读写请求会转发到底层驱动。进入 `gone` 状态后，新请求返回 `Unavailable`。这里没有要求旧句柄立即失效，因为 devtmpfs 设备文件系统索引节点、TTY 对象或正在进行的系统调用都可能持有 `Arc` 强引用。我们采用“先标记，后回收”的顺序，使对象内存由引用计数自然收束，底层硬件访问由状态检查提前阻断。

字符设备能力与 `/dev` 字符设备文件之间存在一层适配。字符设备能力发布的是内核内部能力，devtmpfs 投影器才决定它是否生成 `/dev/console`、`/dev/ttyS0` 或其它节点。这样同一个字符设备可以被投影成多个用户可见入口，也可以先被内核作为启动控制台使用，稍后再补齐用户态节点。

=== 块设备能力

块设备能力面向可寻址 I/O。它的自然请求单位是 Bio 请求，其中包含操作类型、块范围、缓冲区和完成状态。驱动接收 Bio 请求后将请求放入硬件队列或软件队列，随后返回。完成路径再写回结果并唤醒等待方。同步文件读写通过 `submit_bio_wait` 函数建立在异步请求之上。

#pseudo-sample("3-4", [块设备同步等待适配], kind: "代码")[
  ```rust
  fn submit_bio_wait(dev: &BlockDevice, range: BlockRange, buffer: BioBuffer) -> BioResult {
      let completion = BioCompletion::new();
      let bio = Bio::new(BioOp::Read, range, buffer, completion.clone());

      dev.queue_bio(bio)?;
      loop {
          if let Some(result) = completion.try_take() {
              return result;
          }
          dev.drain();
          scheduler::yield_now();
      }
  }
  ```
]

这个同步适配保留了底层异步语义。文件系统和块设备文件可以获得阻塞式接口，驱动仍然按队列模型工作。热插拔移除时，`BlockFunction::drain_io` 方法会调用 `BlockDevice::drain` 方法，尽量推进已经提交的请求完成。这样 PnP 核心不需要理解 Bio 请求的内部字段，也能在移除阶段给块 I/O 留出收束窗口。

块设备还维护几何信息、限制信息、能力标志和 I/O 统计。几何信息描述逻辑块大小、物理块大小和容量，限制信息描述最大请求大小、对齐要求和丢弃范围限制，能力标志描述只读、刷新、丢弃、写零等操作是否可用。这些信息只对块轨道有意义，因此保存在块设备对象内部，由文件系统和块适配层按需读取。

=== 网络设备能力

网络设备能力的主要消费者是协议栈。它的核心数据单位是数据包，控制语义围绕接口状态展开。网络设备能力发布网络设备对象，同时实现网络控制请求，如查询接口编号、链路状态、MAC 地址、MTU、收发统计，或设置管理启用状态。网络设备可以不投影为 `/dev` 节点，因为它已经通过协议栈和套接字层进入用户态。

#pseudo-sample("3-5", [网络设备能力的控制请求分发], kind: "代码")[
  ```rust
  fn control_net_device(dev: &Arc<NetDevice>, req: NetControlRequest)
      -> Result<NetControlResponse, ControlError>
  {
      if !dev.is_active() {
          return Err(ControlError::NoDevice);
      }

      match req {
          NetControlRequest::GetInterfaceId => Ok(NetControlResponse::U32(dev.id().raw())),
          NetControlRequest::GetMacAddress => Ok(NetControlResponse::MacAddress(dev.driver().mac_address())),
          NetControlRequest::GetMtu => Ok(NetControlResponse::Usize(dev.mtu())),
          NetControlRequest::SetMtu { mtu } => {
              dev.set_mtu(mtu)?;
              Ok(NetControlResponse::Done)
          }
          NetControlRequest::SetAdminUp { up } => {
              net::stack().set_iface_admin_up(dev.id(), up)?;
              Ok(NetControlResponse::Done)
          }
          _ => query_driver_or_stack(dev, req),
      }
  }
  ```
]

这里体现了设备能力多轨结构的一个实际价值。网卡驱动只需要把硬件收发能力落实为网络设备对象和网络设备能力，协议栈直接消费类型化对象。VFS 兼容层只在确有需要时参与，例如用户态查询接口属性或修改 MTU。核心设备模型不要求网卡先伪装成字符设备文件。

=== RTC 设备能力

RTC 设备能力把时间设备从字符设备历史接口中拆出。用户态仍可通过 `/dev/rtc0` 和 `ioctl` 系统调用访问 RTC 设备，但内核内部使用 RTC 设备对象表达时间读写、告警配置、周期中断和功能查询。`RtcFunction` 注册为 `DeviceClassId::RTC` 类别，VFS 投影器再生成 `DevNodeSpec::Custom` 规范和必要的符号链接。

RTC 设备的实现说明了兼容层与核心层的边界。我们可以保留用户态 ABI 的历史形式，同时让内核内部保持类型化结构。这样新增 RTC 驱动时，只需要实现 `RtcDriver` 驱动接口和注册 `RtcFunction` 设备能力，不需要在 devtmpfs 设备文件系统核心或系统调用层增加硬件类型特判。

== PnP 设备与驱动绑定

PnP 层负责管理硬件实体。它保存硬件身份、总线信息、状态机、父子拓扑、已发布设备能力、已拥有资源和绑定驱动。驱动通过驱动工厂创建实例，再通过 PnP 驱动接口参与匹配和探测。匹配策略先考虑总线归属，再考虑优先级。同级同优先级的多个驱动同时匹配时，PnP 核心返回歧义错误，避免注册顺序成为隐藏策略。

#pseudo-sample("3-6", [PnP 驱动接口], kind: "代码")[
  ```rust
  trait PnpDriver: Send + Sync {
      fn name(&self) -> &'static str;
      fn bus_type(&self) -> PnpBusType;
      fn priority(&self) -> PnpDriverPriority;
      fn matches(&self, dev: &PnpDevice) -> bool;
      fn probe(&self, dev: &Arc<PnpDevice>) -> Result<(), PnpError>;
      fn remove(&self, dev: &Arc<PnpDevice>);
  }
  ```
]

驱动探测的典型流程可以分为五步。第一步，从 PnP 设备对象中读取硬件身份和资源描述。第二步，申请或映射 MMIO、中断、DMA 等资源。第三步，初始化硬件并构造驱动私有对象。第四步，创建类型化设备和对应设备能力。第五步，调用 `PnpDevice::register_function` 方法完成事务式注册。任何一步失败都应回滚已完成的资源申请，或者把资源交给 PnP 的已拥有资源机制统一释放。

#pseudo-sample("3-7", [探测与设备能力注册事务], kind: "代码")[
  ```rust
  fn probe(dev: &Arc<PnpDevice>) -> Result<(), PnpError> {
      let resources = dev.platform_resources()?;
      let mmio = map_mmio(resources.mmio)?;
      let irq = request_irq(resources.irq)?;
      dev.own_resource(PnpOwnedResource::Irq(irq.clone()))?;

      let driver = Arc::new(ConcreteDriver::new(mmio, irq)?);
      let typed = Arc::new(TypedDevice::new(driver)?);
      let func: Arc<dyn DeviceFunction> =
          Arc::new(ConcreteFunction::new("device0", typed));

      dev.register_function(func)?;
      Ok(())
  }
  ```
]

`register_function` 方法的事务边界只覆盖 PnP 设备自身和全局设备能力注册表。它会先把设备能力挂到 PnP 设备，再尝试推入全局注册表。如果全局注册失败，已经挂接到 PnP 设备的设备能力会被撤销并标记失效。devtmpfs 设备文件系统、sysfs 系统文件系统和 procfs 进程文件系统的投影结果不进入这个事务。投影失败会记录为用户视图失败，底层硬件不因此回滚。

驱动依赖尚未满足时，探测过程可以返回延迟依赖。这个机制主要用于中断控制器、MSI 控制器、系统控制器和 PCI 主桥等基础资源的初始化顺序。我们记录缺失的依赖键，并在对应资源登记完成后重试受影响设备。事后回顾，这个机制比简单的全局重扫更容易定位启动问题，因为日志能够指出阻塞探测过程的具体资源。

== PnP 生命周期与热插拔顺序

PnP 设备状态机约束设备在不同阶段允许执行的操作。`Discovered` 状态表示设备已发现但未绑定驱动。`Probing` 状态表示正在尝试绑定。`Bound` 状态表示驱动已绑定并可能发布设备能力。`Removing` 状态表示移除流程已开始，此时新的绑定和新的 I/O 都应被阻止。`Gone` 状态表示设备生命周期结束。

#figure(caption: figure-caption("图", "3-2", [PnP 状态转换]))[
  #flow-node("Discovered", fill: warm-fill)
  #flow-arrow(label: "选择匹配驱动")
  #flow-node("Probing", fill: soft-fill)
  #flow-arrow(label: "探测成功并发布设备能力")
  #flow-node("Bound", fill: stable-fill)
  #flow-arrow(label: "移除请求")
  #flow-node("Removing", fill: handoff-fill)
  #flow-arrow(label: "排空并释放资源")
  #flow-node("Gone", fill: warm-fill)
]

移除流程必须固定顺序。我们先把设备状态切到 `Removing`，阻止新的绑定。随后递归移除子设备，避免父设备资源提前释放。接着对已经发布的设备能力调用 `mark_gone` 方法，让新 I/O 尽快失败。之后调用 `drain_io` 方法，推动已经提交的请求完成。只有在访问路径和在途请求都被收束之后，驱动的 `remove` 方法才能关闭硬件，PnP 核心才能释放已拥有资源并注销设备能力。

#pseudo-sample("3-8", [PnP 移除流程], kind: "代码")[
  ```rust
  fn remove_device(dev: &Arc<PnpDevice>) {
      dev.transition(PnpState::Removing)?;

      for child in dev.children_snapshot() {
          remove_device(&child);
      }

      for func in dev.functions_snapshot() {
          func.mark_gone();
      }

      for func in dev.functions_snapshot() {
          func.drain_io();
      }

      if let Some(driver) = dev.bound_driver() {
          driver.remove(dev);
      }

      dev.release_owned_resources_lifo();
      dev.unregister_functions();
      dev.transition(PnpState::Gone)?;
  }
  ```
]

这个顺序的安全性来自明确的依赖关系。`mark_gone` 方法处理新访问，`drain_io` 方法处理旧请求，`driver.remove` 方法处理硬件关闭，资源释放处理内核对象归还。若先释放资源，旧句柄仍可能访问已经归还的 MMIO 或 IRQ 句柄。若先关闭硬件，块设备队列中仍可能存在没有完成的请求。我们把这些阶段固定下来，使每个驱动不必单独证明同一组竞态条件。

== 用户态投影与设备文件

设备能力注册表发布的是内核内部能力，用户态看到的是经过投影的名字空间。devtmpfs 设备文件系统订阅设备能力事件，调用 VFS 设备文件层的投影器生成设备节点集合（`DevNodeSet`），再根据设备节点规范创建索引节点。sysfs 系统文件系统和 procfs 进程文件系统也通过同一套投影快照生成诊断信息。这样用户态视图集中在 VFS 设备文件层解释，devtmpfs 设备文件系统核心不需要直接理解 RTC 设备、loop 控制设备接口或其它专用节点。

#pseudo-sample("3-9", [设备文件投影器], kind: "代码")[
  ```rust
  type DeviceFileProjectorBuild =
      fn(&dyn DeviceFunction) -> Result<Option<DevNodeSet>, VfsError>;

  struct DeviceFileProjector {
      owner: &'static str,
      name: &'static str,
      build: DeviceFileProjectorBuild,
  }

  fn devnodes_for_function(func: &dyn DeviceFunction) -> Result<Option<DevNodeSet>, VfsError> {
      for projector in projector_snapshot()? {
          if let Some(nodes) = (projector.build)(func)? {
              return Ok(Some(nodes));
          }
      }
      Ok(None)
  }
  ```
]

投影状态独立于底层生命周期。一个设备能力注册成功后，投影可能处于待处理、已绑定、失败或已解绑。待处理表示事件已被观察到但节点尚未完成绑定。已绑定表示节点已经进入 devtmpfs 设备文件系统。失败表示节点创建失败，错误码会被记录并暴露给诊断视图。已解绑表示设备能力注销后节点已解除。这个状态属于用户 ABI 视图，不参与 PnP 探测的成败判断。

设备节点规范的载荷直接携带打开节点时需要的类型化对象。字符节点携带字符设备对象，块节点携带块设备强引用，自定义节点携带类型化端点。devtmpfs 设备文件系统创建索引节点后，`open` 路径可以直接恢复对象引用，不需要再通过主次设备号查全局表。设备号仍然存在，但它属于 POSIX 兼容视图，主要服务于 `stat` 系统调用、`mknod` 系统调用和 sysfs 展示。

== 初始化顺序与典型路径

设备初始化依赖 VFS、分配器、固件解析和地址转换能力。启动早期先注册核心文件系统和设备文件投影器，再创建 devtmpfs 设备文件系统超级块，并安装设备能力投影订阅。随后设备初始化上下文把 `device_mmio_to_virt` 回调、中断域、DMA 入口、实时时钟钩子等能力交给驱动注册过程。最后固件或总线扫描创建 PnP 设备，驱动工厂尝试绑定，探测成功后发布设备能力。

DTB 路径和 ACPI 路径的发现来源不同，后续设备模型保持一致。DTB 路径从设备树节点中解析平台设备、中断控制器、PCI 主桥等结构。ACPI 路径从 RSDP、MADT、SPCR、命名空间等来源中整理串口、virtio-mmio 设备和平台资源。两条路径最终都转化为 PnP 设备和设备能力发布事件。这样固件格式差异被限制在发现层，生命周期和投影规则保持统一。

#continued-table(
  "3-3",
  [设备初始化的关键顺序],
  (1.2fr, 2.2fr, 2.3fr),
  (
    table.cell(fill: soft-fill)[#text(weight: "bold")[阶段]],
    table.cell(fill: soft-fill)[#text(weight: "bold")[主要动作]],
    table.cell(fill: soft-fill)[#text(weight: "bold")[顺序原因]],
  ),
  (
    table.cell(fill: warm-fill)[VFS 准备],
    table.cell(fill: warm-fill)[注册 tmpfs、devtmpfs、procfs、sysfs 和设备文件投影器。],
    table.cell(fill: warm-fill)[投影规则需要先于设备能力事件订阅安装。],
    table.cell(fill: soft-fill)[投影订阅],
    table.cell(fill: soft-fill)[创建 devtmpfs 设备文件系统超级块，订阅设备能力注册表事件。],
    table.cell(fill: soft-fill)[已经注册或后续注册的设备能力都能被统一投影。],
    table.cell(fill: handoff-fill)[驱动上下文],
    table.cell(fill: handoff-fill)[注入 MMIO 映射、中断域、DMA、RTC 钩子等能力。],
    table.cell(fill: handoff-fill)[驱动探测需要这些平台能力才能初始化硬件。],
    table.cell(fill: stable-fill)[设备发现],
    table.cell(fill: stable-fill)[从 DTB、ACPI、PCI 等来源创建 PnP 设备并尝试绑定驱动。],
    table.cell(fill: stable-fill)[此时分配器、VFS 投影和平台能力均已可用。],
  ),
  kind: "表",
  continuation-kind: "续表",
  align: (left, left, left),
)

以 VirtIO 块设备为例，固件或 PCI 扫描创建 PnP 设备并记录 MMIO 或 BAR 资源。PnP 核心选择 VirtIO 块驱动并进入探测流程。驱动映射寄存器，协商设备特性，初始化虚拟队列，构造块设备对象，再发布块设备能力。设备能力注册表发布 `Registered` 事件后，devtmpfs 投影器生成块设备节点。根文件系统选择逻辑可以在后续阶段从已注册块设备中寻找根盘。

== 类型安全控制

设备控制命令是设备模型中容易失控的部分。传统 `ioctl` 系统调用使用整数命令码和原始指针，编译器无法检查命令与参数类型是否匹配。我们在内核内部更倾向类型化控制，将请求、响应和错误类型绑定在特征接口的关联类型上。用户态仍可通过 `ioctl` 系统调用进入系统，但无类型命令只停留在 VFS 兼容层，核心设备模型使用结构化请求。

#pseudo-sample("3-10", [类型安全控制接口], kind: "代码")[
  ```rust
  trait DriverControl {
      type Request;
      type Response;
      type Error;

      fn control(&self, req: Self::Request) -> Result<Self::Response, Self::Error>;
  }
  ```
]

类型化控制的价值在于把接口错误前移。网络设备的 `SetMtu` 请求、RTC 设备的 `ReadTime` 请求、字符设备的 TTY 控制，都可以在内核内部用枚举和结构体表达。调用方构造请求时已经受到类型系统约束，驱动返回的响应也有明确类型。对于通用路径无法表达的专用能力，调用方可以通过 `function_as::<T>` 方法恢复具体设备能力类型，然后调用对应类型化控制。这个入口要显式使用，避免核心特征接口不断加入可选方法。

== 工程设计总结

在工程推进的过程中，我们通过多种方式实现了竞态条件和各种边界条件的规避。例如，设备注册和移除最容易出现的问题，通常不在正常路径，而在错误回滚和热插拔边界。我们在 `PnpDevice::register_function` 方法中把事务边界限定在 PnP 设备和全局设备能力注册表之间。驱动构造好类型化设备能力后，先挂入 PnP 设备，再注册到全局注册表。如果全局注册失败，已经挂入 PnP 设备的设备能力会被撤销并标记失效。这样可以避免 PnP 设备认为自己已经暴露能力，但全局注册表中不存在对应记录。移除路径也被拆成固定阶段。先进入 `Removing` 状态，递归移除子设备。再对设备能力调用 `mark_gone` 方法，阻止新的 I/O。随后调用 `drain_io` 方法，尽量收束旧请求。之后才调用驱动 `remove` 方法并释放已拥有资源。这个顺序对应明确的依赖关系。`mark_gone` 方法处理新访问。`drain_io` 方法处理在途请求。`remove` 方法处理硬件关闭。资源释放处理内核对象归还。我们把这些动作固化到框架中，驱动作者不需要在每个驱动里重新证明同一组竞态条件。

我们还通过类型化控制降低了设备专用命令的风险。传统 `ioctl` 系统调用的优点是兼容范围广，问题是命令码和参数指针之间缺少编译期约束。对于简单字符设备，这个问题尚可通过少量分支控制。对于网络设备、RTC 设备和块设备这类带有复杂状态的对象，无类型命令很容易扩散到驱动内部。我们采用类型化控制，把请求、响应和错误类型绑定在具体设备轨道中。网络设备的 `SetMtu` 请求、RTC 设备的读写时间和告警配置、字符设备的 TTY 控制，都可以在内核内部用枚举和结构体表达。用户态仍然通过 `ioctl` 系统调用进入系统，但解析和兼容停留在 VFS 适配层。设备核心只接收结构化请求。这个设计还有一个附加收益，即专用能力的入口更加显式。通用路径面向设备能力。专用路径通过 `function_as::<T>` 方法恢复具体设备能力类型。调用方必须明确声明自己依赖哪个设备轨道，核心特征接口因此保持稳定。

除此之外，我们还把用户态投影集中在 VFS 设备文件层。早期设备模型很容易把 `/dev` 节点创建逻辑分散到不同驱动或不同文件系统中。短期实现较快，长期会造成投影策略不一致。我们把投影规则收束到设备文件投影器和设备节点规范。投影器根据类型化设备能力生成节点集合。设备节点规范携带打开节点所需的类型化载荷。devtmpfs 设备文件系统只负责创建索引节点和维护目录树。sysfs 系统文件系统与 procfs 进程文件系统使用同一套投影快照生成诊断信息，避免各自向下转换底层设备能力。设备号也被放在投影侧处理。它服务于 POSIX 兼容和用户态观察，不参与内核内部的设备查找。这样一来，用户态 ABI 的演进被限制在 VFS 设备文件层，底层设备能力生命周期不受设备号、节点名或符号链接策略影响。

设备管理子系统充分利用了 Rust 语言的各种核心优势，将传统 POSIX 系统极其容易触发的各种漏洞在编译期就直接规避，大大提高了设备管理子系统的稳定性与驱动中逻辑漏洞的可复现性。对于我们的项目来说，要编写一个驱动程序，只需要实现对应的特征接口并放进注册表，在 PnP 层就可以通过自动认领的机制自动启用设备及其驱动，甚至可以通过设备能力机制做到自定义设备类型，这大幅增加了设备管理子系统的可拓展性与驱动开发的便捷性，大大便利了社区对操作系统本身生态的完善工作以及设备驱动程序的维护工作。

设备管理子系统具备以下创新。

第一是通过分层抽象与实现机制，把抽象和具体的辩证关系落实到设备模型中。这看似是一个悬空的哲学命题，但在工程实现中，我们发现它对应着非常实际的约束。设备抽象过早统一，会损失硬件能力的真实语义。设备抽象过度贴近每个驱动，又会让生命周期、资源归属和用户态视图分散到各处。在项目早期，我们曾经考虑过接近 POSIX 传统设备模型的路径。这个路径把字符设备和块设备作为基础分类，再用设备号和 `/dev` 节点串联用户态访问。它的兼容语义清晰，VFS 适配也直接。问题出现在网络设备、RTC 设备和 loop 控制设备进入系统之后。网络设备的自然消费者是协议栈。RTC 设备的自然语义是时间、告警和中断。loop 控制设备更接近控制端点。若全部压入字符设备，大量语义会通过 `ioctl` 系统调用旁路表达，内核内部也会被用户态兼容形式牵引。我们也尝试过更极端的类型化对象路径，让每类设备都直接暴露给各自子系统。这个方向保留了能力差异，却分散了全局生命周期。最终我们选择收敛到设备能力。它只抽象所有设备必须共享的内容，例如类别、内部名称、失效标记、I/O 排空入口和类型恢复入口。真实 I/O 语义则留给类型化对象。抽象并非脱离具体的理想模型，具体也不是抽象的被动实例化。两者在工程推进中持续相互塑造。抽象从具体硬件差异中提取共性，具体则依循抽象框架有序展开。

第二是设备管理子系统通过设备能力机制实现了前所未有的高度可拓展性。这里的可拓展性不只体现为可以多注册一个驱动。更重要的含义是新增设备类型时，核心生命周期规则无需重写。我们把拓展点拆成三个层次。第一层是设备类别标识和设备能力，用于让新设备能力进入统一注册表。第二层是类型化对象，用于保存新设备自己的 I/O 语义和控制请求。第三层是 VFS 投影器或类型化消费者，用于决定这类设备能力是否需要用户态文件节点，也可以决定它是否由网络栈、时间子系统等内核模块直接消费。这个结构使网络设备、RTC 设备和 loop 控制设备这类非传统的或设备本身不属于传统 POSIX 建模形式的字符块设备能够沿着同一条路径接入。以 RTC 设备为例，底层驱动发布 RTC 设备能力。devtmpfs 设备文件系统通过投影器生成 `/dev/rtc*` 兼容入口。时间相关操作仍保留在 RTC 设备对象的类型化控制中。新增设备能力轨道时，PnP 核心仍然只处理探测、移除、失效和资源释放。设备能力注册表仍然只处理类别、名称和事件。devtmpfs 设备文件系统核心仍然只消费设备节点规范。这种扩展方式把变化限制在新增类型周围，避免修改全局主流程。

第三是硬件生命周期、开放能力和用户态命名空间三者通过 POSIX 兼容层的接入、底层设备抽象的无设备号设计以及零成本抽象三位一体结构实现分离。这个设计最初来自一个实际问题。底层设备探测成功以后，是否必须等 `/dev` 节点也创建成功，才能认为设备注册成功。若把两者绑在一起，devtmpfs 设备文件系统的命名冲突或临时内存不足就会导致硬件探测回滚。若完全分开，又需要能够诊断用户态节点缺失的原因。当前设计把 PnP、设备能力注册表和投影层分成三个阶段。PnP 设备表达硬件身份、资源归属和驱动绑定。设备能力注册表表达驱动向内核开放的能力。devtmpfs 设备文件系统、sysfs 系统文件系统和 procfs 进程文件系统表达用户态可见视图。设备能力注册成功后，底层设备已经可以被内核内部消费。投影失败只记录在投影状态中，供 procfs 进程文件系统和 sysfs 系统文件系统诊断。这个边界让启动顺序更宽松。设备可以早于 `/dev` 挂载完成注册。devtmpfs 设备文件系统可以在订阅安装后补齐已有设备能力。用户态名字空间也可以在后续挂载阶段接入。单向依赖因此得到保持，设备核心不需要反向依赖 VFS。

从工程实践看，这些创新共同支撑了设备子系统的长期演化能力。设备数量增加时，核心生命周期规则保持稳定。新增设备能力轨道时，已有字符和块设备路径不需要重写。用户态 ABI 需要补充时，修改集中在投影层。单向依赖避免了循环依赖。类型化接口降低了控制命令的误用概率。分阶段移除让热插拔和错误回滚具备清晰的分析边界。设备管理层的可扩展性并不是单个特征接口带来的结果，而是抽象边界、事务边界和投影边界共同作用的结果。
