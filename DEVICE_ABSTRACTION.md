# 设备抽象

本文描述 Hitoshizuku 内核设备层的稳定边界。它面向总线实现、内核驱动、ELM
模块和 VFS 投影层的开发者，内容以当前 `general::dev` 实现为准。设备层不是
Linux 设备号模型的复制品：硬件身份、驱动绑定、设备功能和用户可见节点是四种
不同的对象。

相关实现：

- [`general/src/dev/pnp.rs`](general/src/dev/pnp.rs)：PnP 身份、拓扑、驱动匹配、资源归属和生命周期；
- [`general/src/dev/function.rs`](general/src/dev/function.rs)：开放的 `DeviceFunction` 注册表；
- [`general/src/dev/enumerate.rs`](general/src/dev/enumerate.rs)：设备和 function 的全局事件与查询；
- [`general/src/dev/pci.rs`](general/src/dev/pci.rs)、[`general/src/dev/usb.rs`](general/src/dev/usb.rs)：总线级封装；
- [`general/src/dev/char.rs`](general/src/dev/char.rs)、[`general/src/dev/block.rs`](general/src/dev/block.rs)：字符和块 I/O 契约；
- [`general/src/dev/elm_lifecycle.rs`](general/src/dev/elm_lifecycle.rs)：ELM 设备资源的收口；
- [`drivers/Modules.toml`](drivers/Modules.toml)：驱动模块清单和配置依赖。

## 1. 设计目标与分层

设备对象沿着下面的方向流动。每一层只能依赖它下面的稳定契约，不能通过用户
可见的 `/dev` 名字反向寻找硬件对象。

```text
固件 / 总线枚举
        │  PnpId + PnpBusInfo + 资源描述
        ▼
PnpDevice（硬件身份、拓扑、状态、绑定驱动）
        │  PnpDriver::probe/remove
        ▼
DeviceFunction（一个物理设备提供的一个可用功能）
        │  生命周期事件 + typed downcast
        ├──────────────► VFS projection（/dev、sysfs 等视图）
        └──────────────► 内核消费者或 ELM 模块
```

- **总线层**负责枚举和标准化硬件信息。PCI、USB、platform 以及未来的动态总线
  都提交相同的 `PnpDevice`，PnP core 不解释总线私有寄存器。
- **PnP 层**负责设备唯一性、父子拓扑、驱动匹配、资源租约和热拔。它不负责
  把设备变成字符节点或块节点。
- **功能层**用 `DeviceFunction` 表达设备对内核开放的能力。一个设备可以同时
  暴露多个 function，也可以只提供不投影到 `/dev` 的内部 function。
- **VFS 投影层**订阅 function 生命周期事件，按 typed function 建立或撤销用户
  视图。投影创建失败不会回滚底层 probe。
- **ELM 管理层**负责镜像加载、符号契约、依赖、调用隔离和卸载收口；设备热路径
  仍然直接调用 Rust trait，不把每次 MMIO 或 I/O 包装成通用 ELM 消息。

## 2. 对象模型

### 2.1 `PnpId` 与 `PnpBusInfo`

`PnpId` 是设备在拓扑中的稳定硬件身份，不是设备文件名：

- `PnpId::Pci` 用 segment/bus/device/function（BDF）定位 PCI function；
- `PnpId::Usb` 用 bus、address 和可选 interface 定位 USB 对象；
- `PnpId::Platform` 保存固件路径、父路径、DTB compatible 或 ACPI HID/CID 以及
  标准化资源 tuple；
- `PnpId::Dynamic` 保存 fingerprint、动态 `BusType`、契约名和拥有副本的身份字节，
  允许新的发现源加入而不用修改 PnP 状态机。

`PnpBusInfo` 只提供总线种类、诊断名和 `as_any` 类型恢复。`PciInfo`、platform
信息等具体类型可以在驱动的 `matches`/`probe` 中通过 `as_any` 恢复；它们不应
把 ELM 镜像中的 `&str` 或裸指针存入长期设备对象。动态总线构造
`DynamicPnpBusInfo` 时会复制 bus name、contract、属性和资源。

### 2.2 `PnpDevice`

`PnpDevice` 是物理设备对象，主要字段和所有权如下：

| 部分 | 含义 |
| --- | --- |
| `id`、`name`、`info` | 硬件身份、稳定诊断名和总线私有描述 |
| `parent`、`children` | host bridge、controller、子 function 等真实拓扑 |
| `functions` | 该设备当前暴露的 `Arc<dyn DeviceFunction>` |
| `resources` | 由 PnP core 持有的 IRQ、MSI、DMA、controller handle 等租约 |
| `bound_driver`、`driver_data` | 当前绑定的驱动和驱动私有状态 |
| `state` | `Discovered` 到 `Gone` 的生命周期状态 |

设备通过 `PNP_DEVICES.get_or_insert` 进入全局表；相同 `PnpId` 的重复枚举会复用
既有对象。只有对象指针完全相同的 `remove_exact` 才能从全局表删除，避免旧的
热拔对象误删后来重新枚举的同身份设备。

### 2.3 `DeviceFunction`

`DeviceFunction` 是开放的、可查询的功能对象，而不是一个总线对象。它要求实现
`Send + Sync`，并提供：

- `class_id()`：功能类别。内建类别包括 `CHAR`、`BLOCK`、`RTC`，其它类别可通过
  `register_function_class` 运行期分配单调递增且不复用的编号；
- `dev_name()`：function registry 的内部唯一键。同一 `class_id + dev_name` 不得重复；
- `class_name()`、可选 `operation_contract()`/`invoke()`：通用契约入口；
- `dma_context()`：存在设备级 DMA 数据面时返回约束；
- `is_gone()`、`mark_gone()`、`drain_io()`：移除时阻止新 I/O 并排空旧 I/O；
- `as_any()`：为自定义 function 提供类型安全的 `function_as::<T>` 向下转型。

`CharFunction` 和 `BlockFunction` 是当前的两种适配器。它们分别包装
`CharDevice` 和 `Arc<BlockDevice>`，可以单独指定 VFS `projection_name`。这个投影
名只用于建立用户视图，不参与 PnP 匹配、资源所有权或硬件身份判等。

全局 `DEVICES.functions` 只保存 `Arc<dyn DeviceFunction>`。注册和注销会发布
`DeviceFunctionEventKind::{Registered, Unregistered}`；事件观察者负责维护自己的
缓存或节点，不得在回调中假设 `/dev` 一定存在。

## 3. 生命周期与事务边界

### 3.1 PnP 状态机

```text
Discovered ──► Probing ──► Bound ──► Removing ──► Gone
      ▲           │           │                     │
      └───────────┴───────────┘                     │
          probe 失败或驱动解绑                     终态
```

允许的状态转换由 `PnpState::can_transition_to` 固定：

1. 总线发现后进入 `Discovered`；驱动 registry 只从这个状态开始匹配。
2. 选中驱动后进入 `Probing`，调用 `PnpDriver::probe`。
3. `probe` 成功并完成提交后进入 `Bound`。驱动必须在这里之前注册需要对外提供的
   function，并把外部资源移交给 PnP core。
4. `probe` 返回任何错误都会回滚本次函数、子设备和资源，设备回到
   `Discovered`；`ProbeDeferred` 或 `DependencyNotReady` 还会记录精确依赖。
5. 驱动注销会走 `Removing`，但硬件仍在全局表中，清理完成后回到 `Discovered`，
   允许其它驱动重新匹配。
6. 真正热拔时走 `Removing` 到 `Gone`，从 `PNP_DEVICES` 移除，旧对象不能再次接受
   probe 或 I/O。

`PnpDevice::register_function` 的事务只覆盖 PnP 对象和全局 function registry。
如果外部 registry 插入失败，它会撤销设备内挂载、标记 function 为 gone 并返回
错误；`/dev`、`/sys` 等投影的成功与否不改变这个事务结果。

### 3.2 热拔和驱动解绑顺序

`remove_device` 使用原子移除锁防止并发重复清理，按以下顺序执行：

1. 标记 `Removing`，禁止新的 probe；
2. 深度优先移除子设备；
3. 对所有 function 调用 `mark_gone()`，阻止新的访问；
4. 调用 `drain_io()` 排空已有 I/O；
5. 调用驱动 `remove()` 关闭硬件并清理 `driver_data`；
6. 按 LIFO 释放 PnP-owned resources；
7. 注销 function 的外部 registry/投影；
8. 标记 `Gone`，从全局 PnP 表和父节点解除关系。

驱动解绑与热拔的区别是：解绑后设备对象仍然有效且回到 `Discovered`，热拔后
对象进入终态 `Gone`。驱动的 `remove` 不应尝试重新注册自身或继续提交新的硬件
I/O。

## 4. 资源、租约与 DMA

设备资源的释放所有权属于 PnP core，而不是 VFS、sysfs 或普通诊断代码。
驱动通过 `PnpDevice::own_resource` 或 `own_boxed_resource` 移交一个实现了
`PnpResource` 的对象。资源至少描述：

- `PnpResourceKind`：如 `Irq`、`Msi`、`Dma`、`Syscon`、`PciHostBridge`、`Function`；
- 稳定的静态 `label`：用于日志，不得引用可卸载镜像里的字符串；
- 可选的稳定 `identity`：供驱动主动撤销指定 handle；
- 消费 handle 的 `release`：重复或过期释放必须返回错误而不能触发未定义行为。

资源只能在 `Probing` 或 `Bound` 状态登记。PnP core 在 probe 回滚、驱动解绑和
热拔时以 LIFO 顺序消费资源；释放失败只记录诊断并继续完成 remove，避免一个坏掉
的 controller handle 阻塞其它资源的收口。需要登记多个外部 handle 的驱动应先用
`reserve_owned_resources` 预留容量，再创建和交接 handle，防止容量扩展失败造成泄漏。

IRQ、MSI、DMA 和 PCI host bridge 等更具体的 registry 仍通过自己的 typed API
创建；交给 `PnpResource` 的是释放闭包或 `PnpHandleResource` 包装器。DMA 地址、
一致性和 bounce 策略由 [`general/src/dev/dma.rs`](general/src/dev/dma.rs) 的平台
hook 统一管理，驱动不得自行猜测物理地址映射。

诊断层只能读取 `PnpOwnedResourceSnapshot` 的类别和标签，不能拿到底层 handle。
这保证 sysfs/procfs 或 ELM 管理面不能绕过 PnP remove 事务提前释放资源。

## 5. 驱动匹配与配置

### 5.1 `PnpDriver` 合约

驱动实现 `PnpDriver` 的 `name`、`bus_type`、`matches`、`probe` 和 `remove`：

- `bus_type` 必须返回 `BusType::PCI`、`USB`、`PLATFORM` 等精确总线，或显式返回
  `GENERIC` 作为兜底驱动；
- `matches` 只判断硬件身份和总线描述，不应在匹配阶段修改设备或分配持久资源；
- 需要父子关系、动态属性或设备状态时覆盖 `matches_device`；
- `priority` 只解决同一总线上的具体程度，不得依赖驱动注册顺序。总线精确匹配
  始终优先于 `GENERIC`，同层级同优先级的多个命中返回 `DriverAmbiguous`；
- `probe` 负责初始化硬件、声明资源、设置 `driver_data`、挂载子设备和注册
  function。返回 `PnpError` 时必须让 core 可以完整回滚；
- `remove` 负责停止 DMA/IRQ、关闭设备并释放未交给 PnP core 的私有状态。

驱动 factory 通过 `register_driver_factory` 创建实例。新驱动注册后会立即尝试认领
已经处于 `Discovered` 的设备；注销时先停止接受新 probe，再解绑已绑定设备，最后
从 registry 删除。驱动编号和资源句柄不会在生命周期内复用。

### 5.2 `Modules.toml` 与 Cargo

[`drivers/Modules.toml`](drivers/Modules.toml) 是声明式清单，不是运行时设备表。每
个条目描述：

| 字段 | 作用 |
| --- | --- |
| `name`、`path` | ELM/Cargo 模块的逻辑名和目录 |
| `config`、`prompt`、`default` | `CONFIG_*` 选项和 menuconfig 风格默认值 |
| `targets` | 允许构建的目标三元组 |
| `depends`、`after` | 配置依赖和加载/构建顺序 |
| `features` | 传给该 Cargo 包的 feature |

使用 `cargo xtask config` 选择配置，`cargo xtask oldconfig` 补齐新选项，
`cargo xtask defconfig` 恢复默认配置，再由 `cargo xtask modules --target <triple>`
构建选择的模块。清单中的 `y` 表示内建驱动，`m` 表示独立 ELM 模块；它不改变
PnP core 的运行时匹配规则。

## 6. ELM 边界与卸载

ELM 模块可以通过审核过的 kernel symbol 合约访问设备层，但每一个可调用入口都
必须显式使用 `kernel_symbols::export` 声明：名称、contract、版本、能力、状态
修改和所有权返回属性都属于接口的一部分。典型设备入口包括：

- `general.dev.pnp.register_driver_factory` / `unregister_driver`；
- `general.dev.pnp.PnpDevice.register_function` / `remove_device`；
- `general.dev.function.*` 的 function class 和类型擦除构造器；
- IRQ、MSI、DMA、PCI host bridge 等 typed registry。

设备相关注册会在 [`elm_lifecycle.rs`](general/src/dev/elm_lifecycle.rs) 中记录为
ELM-owned resource，包括动态 function class、driver、PnpDevice、DeviceFunction、
事件订阅以及 controller/ops hook。模块卸载时，ELM 管理器据此执行 quiesce、drain
和 release；模块不能依赖“忘记注销”来获得安全卸载。

当前设备回调资源的 `suspend` 路径明确返回“不支持”，因此扩展不得把设备功能
设计成可暂停后继续使用的隐含状态。可移植的卸载语义只有：停止新调用、标记
function gone、排空 I/O、调用 remove、释放资源，并等待仍持有的 `Arc`/租约收束。
保留在内核常驻区的 vtable 和 typed 构造器应来自 `general`，避免动态镜像卸载后
仍执行镜像代码。

ELM 不是设备热路径的消息总线。驱动 probe 后，字符读写、块 I/O、网络队列和
MMIO 访问通过直接 Rust trait 调用完成；ELM 只管理镜像、符号、依赖、权限、故障
隔离和生命周期。设备接口的稳定 ABI 细节见 [`SOYO_FORMAT.md`](SOYO_FORMAT.md)。

## 7. 错误与并发语义

### 7.1 错误分类

驱动应返回最具体的 `PnpError`，让启动路径和 deferred probe 做正确处理：

- `NoDriver`：没有任何接受该总线且匹配的驱动；
- `ProbeDeferred`、`DependencyNotReady(PnpDependency)`：依赖尚未就绪，设备保持
  `Discovered`。controller、syscon、fwcfg 等资源登记完成后调用
  `notify_dependency_ready`，只重试等待该依赖的设备；
- `MissingResource`、`MalformedResource`、`Unsupported`：设备描述或能力不满足，
  通常不应无条件重试；
- `DriverAmbiguous`：多个驱动以相同总线层级和优先级命中，应修正匹配条件或优先级；
- `FunctionExists`、`NameConflict`：注册表身份冲突；
- `InvalidState`、`InvalidTransition`：生命周期顺序错误；
- `OutOfMemory`、`RegistrationFailed`、`HardwareFailure`：分别表示分配、登记或硬件
  操作失败。

`DeviceFunction::invoke` 的通用调用面另有 `Invalid`、`Gone`、`Busy`、
`Unsupported`、`Fault` 和 `NoMemory`；具体字符/块驱动应继续返回各自 typed I/O
错误，不要把所有错误压成 `invoke` 的整数码。

### 7.2 锁和回调规则

- `PnpDevice` 内部状态、全局设备表、驱动表和 function 表分别由锁保护；共享
  function 必须满足 `Send + Sync`。字符驱动的读写方法以 `&self` 调用，驱动在
  内部负责 FIFO、队列或寄存器锁。
- PnP 匹配会先复制候选驱动 `Arc`，再调用不受信任的 `matches`/`probe`，不会在
  持有驱动表锁时进入回调；事件发布也在释放订阅表锁后调用回调。回调可以嵌套
  登记资源，但不能假设全局锁仍被持有。
- `removal_lock` 用原子比较交换保证同一设备只有一个 remove 流程。所有访问
  function 的代码都必须把 `is_gone`/底层 typed error 视为最终权威，不能仅依赖
  一个先前读取的 `PnpState` 快照。
- 不能在硬 IRQ 中调用可能分配、阻塞或进入 ELM 管理器的路径。网络栈等热路径的
  单写者约束由其自身队列和 runtime 契约保证，设备层只提供正确的资源和中断边界。

## 8. 最小驱动流程示例

下面是说明顺序的伪代码；具体总线资源构造以对应驱动为准：

```rust,ignore
impl PnpDriver for ExampleDriver {
    fn name(&self) -> &str { "example" }
    fn bus_type(&self) -> BusType { BusType::PCI }

    fn matches(&self, id: &PnpId, info: &dyn PnpBusInfo) -> bool {
        matches!(id, PnpId::Pci { .. })
            && info.as_any().downcast_ref::<PciInfo>()
                .is_some_and(|pci| pci.vendor == 0x1234 && pci.device_id == 0x5678)
    }

    fn probe(&self, dev: &Arc<PnpDevice>) -> Result<(), PnpError> {
        // 1. 从 PciInfo/PciDevice 取得 BAR、DMA 和 IRQ；不保存裸固件引用。
        // 2. 先预留资源归属容量，再把每个外部 handle 交给 PnP core。
        dev.own_resource(PnpHandleResource::new(
            PnpResourceKind::Irq,
            "example-irq",
            irq_handle,
            release_irq,
        ))?;

        // 3. 构造常驻类型定义的 function，并与硬件设备原子关联。
        let function: Arc<dyn DeviceFunction> = make_example_function()?;
        dev.register_function(function)?;
        Ok(())
    }

    fn remove(&self, dev: &Arc<PnpDevice>) {
        // 停止队列和中断；function、PnP-owned 资源由 core 按既定顺序收口。
        stop_hardware(dev);
    }
}
```

实际实现应优先复用 [`drivers/virtio-net`](drivers/virtio-net)、
[`drivers/virtio-blk`](drivers/virtio-blk) 和 platform 驱动中的匹配、资源登记与
ELM 导出模式。新增总线或 function 类型时，保持上面的对象边界，不把总线私有
字段塞进通用 `PnpDevice`，也不要用新的 `/dev` 名称取代既有 ABI 身份。

## 9. 扩展约束清单

提交新的设备或驱动前，至少确认：

1. 硬件身份可由 `PnpId` 和拥有数据的 `PnpBusInfo` 重复识别，不依赖节点名；
2. `matches` 无副作用，`probe` 的每一项副作用都有 PnP rollback 或 `remove` 路径；
3. IRQ、MSI、DMA、MMIO 映射和子设备都登记了明确的资源归属；
4. function 的 `class_id + dev_name` 唯一，投影名与底层身份分离；
5. `mark_gone` 后新 I/O 立即返回 typed 的不可用错误，旧 I/O 可被 `drain_io` 收口；
6. 所有 ELM 入口都有版本化 contract、最小能力集和审核过的所有权语义；
7. 没有把动态镜像中的字符串、vtable、回调或裸地址泄漏到常驻对象；
8. `drivers/Modules.toml` 的配置、目标、依赖和加载顺序与 Cargo 包保持一致；
9. 运行 `cargo fmt --all`、目标架构的 `cargo check`/`cargo xtask modules`，并覆盖
   probe 失败、deferred probe、重复枚举、驱动解绑和热拔测试。

