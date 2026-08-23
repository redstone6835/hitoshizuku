# CFI NOR Flash 平台驱动

`platform.cfi-flash` 把固件声明的线性 CFI NOR Flash MMIO 窗口登记到
`general::dev::flash`。当前实现是识别与只读传输层，不是完整的 CFI 命令集驱动。

## 职责与边界

- 匹配 platform 设备的 `compatible = "cfi-flash"`。
- 读取 `bank-width`；未声明时取 `1`，只接受 `1`、`2`、`4`、`8`。
- 映射全部非空 MMIO resource，并按固件顺序把多个窗口视为一段连续地址空间。
- 通过 `FlashDevice` 提供跨窗口、带溢出和边界检查的易失字节读取。
- 向 flash registry 登记设备，并把 registry handle 交给 PnP 设备持有。

当前能力明确为 `readable = true`、`writable = false`、`erasable = false`。本 crate
不执行 CFI query，不识别命令集，也不实现写入、擦除、磨损管理、分区或块设备投影。
这些能力需要单独的 command-set 层，不能通过普通 MMIO 写入补齐。

## 平台与模块选择

该 crate 的 Rust 代码没有架构专用分支，模块清单也没有限制 target；是否存在匹配设备
取决于 DT/ACPI 枚举结果。

| 项目 | 值 |
| --- | --- |
| Cargo 包 | `platform-cfi-flash` |
| ELM 名称 | `platform.cfi-flash` |
| ELM 模式/阶段 | `m` / `device` |
| 配置项 | `CONFIG_CFI_FLASH`，默认 `m` |
| 构建顺序 | `platform.firmware-bus` 之后 |
| 匹配 ID | `cfi-flash` |

## 对象与生命周期

`CfiFlashFactory` 在 ELM `initialize` 时注册一个 PnP driver factory。设备 probe 后，
`CfiFlashPlatformDriver` 创建 `CfiFlash`，保存固件名、bank width 和映射窗口，并向
flash registry 注册。registry 资源由 `PnpDevice::own_resource` 接管；绑定失败会立即
撤销刚完成的注册。ELM `finalize` 按逆序注销 factory，忙碌时返回错误而不伪装成功。

`remove` 本身为空，设备资源的注销由 PnP 资源所有权完成。任何新增资源也应在 probe
阶段交给 `PnpDevice`，或在失败路径中显式回滚。

## 资源、并发与安全

- probe 拒绝空窗口和零长度窗口，映射通过 `DevInitContext::device_mmio_to_virt` 完成。
- 总长度、偏移加法和跨窗口定位均使用 checked arithmetic；越界返回
  `FlashError::OutOfRange`。
- 读取使用 `read_volatile`。其安全前提是固件声明的物理窗口真实、持续有效且可按字节
  访问；本驱动不探测或扩大该范围。
- 当前只有只读操作和不可变窗口表，不需要设备内 RMW 锁。以后加入命令状态机时必须
  为写入/擦除和读状态序列增加设备级串行化，不能沿用当前无锁假设。

## 依赖关系

实现直接依赖 `general::dev::{platform,pnp,flash}`、`elm` 生命周期、`allocator` 和
`log`。`Cargo.toml` 中其余内核 façade crate 属于 Kernel API Profile 的统一链接闭包，
不表示本驱动承担网络、文件系统或 ACPI 业务。

## 检查与构建

在内核仓库根目录检查任一受支持目标：

```sh
cargo check -p platform-cfi-flash --lib --target loongarch64-unknown-none
cargo check -p platform-cfi-flash --lib --target riscv64gc-unknown-none-elf
```

仓库内的标准入口由 [`../Modules.toml`](../Modules.toml) 和 `.config` 决定 `y/m/n`：

```sh
cargo xtask config
cargo xtask modules --target loongarch64-unknown-none
```

只检查或构建这个受管 ELM 时可直接使用：

```sh
cargo elm check drivers/cfi-flash --arch loongarch64
cargo elm build drivers/cfi-flash --arch loongarch64 --unsigned
```

`--unsigned` 仅用于本地测试；发布镜像应使用 `--key` 与非零 `--epoch`。
