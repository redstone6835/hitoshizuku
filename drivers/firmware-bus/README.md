# 固件 Platform Bus 驱动

`platform.firmware-bus` 保存固件中简单地址总线的拓扑和地址转换描述，供通用设备层
查询。固件解析器已经把子节点标准化为 platform PnP 设备；本 crate 不再次扫描或创建
子设备。

## 职责与边界

- 匹配 `simple-bus`、`simple-mfd` 和 `qemu,platform` platform 节点。
- 读取子总线和父总线的 `#address-cells`、子总线的 `#size-cells`。
- 把 `ranges` 的大端 32 位 cells 解析为 `FirmwareBusRange`，并保留 phandle、总线名
  和 `dma-coherent` 属性。
- 把不可变的 `FirmwareBusDescriptor` 登记到 `general::dev::firmware_bus`。

本 crate 不枚举子节点，不分配 MMIO，不替设备驱动翻译其 `reg`，也不实现 ACPI/DTB
解析器。空 `ranges` 被保留为空映射；其具体地址语义由消费该 descriptor 的上层决定。

## 平台与模块选择

解析逻辑与架构无关，可用于仓库支持的 LoongArch64 和 RISC-V64 target。

| 项目 | 值 |
| --- | --- |
| Cargo 包 | `platform-firmware-bus` |
| ELM 名称 | `platform.firmware-bus` |
| ELM 模式/阶段 | `y` / `device` |
| 配置项 | `CONFIG_FIRMWARE_BUS`，默认 `y` |
| 模块顺序 | 基础 platform 驱动，无 `after` 依赖 |
| 匹配 ID | `simple-bus`、`simple-mfd`、`qemu,platform` |

## 对象与生命周期

ELM `initialize` 注册 `FirmwareBusFactory`。匹配设备 probe 后构造
`DtbFirmwareBus`，将其注册到 firmware-bus registry，并把 handle 作为 PnP 资源交给
设备持有；资源接管失败会撤销 registry 条目。descriptor 注册后只读，通过 `Arc` 与
消费者共享。

`remove` 不做额外操作，registry handle 随 PnP 资源释放。ELM `finalize` 注销 factory；
若仍有绑定阻止注销，会返回 busy 类 hook 错误。

## 解析、安全与并发边界

- `ranges` 每项宽度为 child address、parent address 和 size 的 cell 数之和；总长度必须
  是该宽度的整数倍。
- 单个 cell 值最多 128 位；parent address 和 size 还必须能转换为当前平台的 `usize`。
- 零长度、父地址加长度溢出、cell 数加法溢出都会拒绝 probe。
- descriptor 在注册后不可变，因此读取无需驱动内锁。registry 的并发和句柄生命周期由
  `general` 管理。
- 本驱动只验证描述的结构，不证明固件地址实际可访问；后续映射者仍需遵守各自的资源
  所有权与 MMIO 检查。

## 依赖关系

直接使用 `general::dev::{platform,pnp,firmware_bus}`、`elm`、`allocator` 与 `log`。
其它 façade 依赖来自统一 Kernel API Profile 闭包，不代表本 crate 实现对应子系统。
多数 platform 驱动在 [`../Modules.toml`](../Modules.toml) 中声明在本模块之后构建，
但设备实际 probe 仍由 PnP 依赖解析和固件拓扑决定。

## 检查与构建

```sh
cargo check -p platform-firmware-bus --lib --target loongarch64-unknown-none
cargo check -p platform-firmware-bus --lib --target riscv64gc-unknown-none-elf

cargo xtask config
cargo xtask modules --target loongarch64-unknown-none

cargo elm check drivers/firmware-bus --arch loongarch64
cargo elm build drivers/firmware-bus --arch loongarch64
```

`cargo xtask modules` 是仓库内按配置构建的首选入口；直接 `cargo elm build` 适合单模块
诊断。本模块默认以 `y` 模式集成，不生成需要签名的独立 EKI。
