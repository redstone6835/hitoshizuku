# QEMU fw_cfg 驱动

`platform.fw-cfg` 实现 QEMU fw_cfg 的 MMIO 与传统 SystemIO 只读传输，并把唯一
活动实例安装到 `general::dev::fwcfg`。配置项的业务解释属于上层，不属于该驱动。

## 职责与边界

- 匹配 DT `compatible = "qemu,fw-cfg-mmio"` 或 ACPI HID `QEMU0002`。
- MMIO 校验 data/selector 窗口，以大端写 selector；窗口和 revision 都允许时启用 DMA。
- SystemIO 从 ACPI `_CRS` 获取 selector 窗口，要求偶数基址且至少覆盖两个端口；以
  16 位小端 I/O 事务写 selector，再从 `base + 1` 连续读取字节。
- probe 时读取 selector `0x0000` 并要求签名为 `QEMU`；revision 仅用于日志。
- 向全局 fw_cfg 服务安装 `FwCfgDevice`，并把安装 handle 交给 PnP 管理。

当前实现只提供 `read_item`，不写配置项，也不解释 initrd、SMBIOS、ACPI、文件目录或
其它 selector 内容。SystemIO 的 ACPI 资源通常只拥有 `0x510..0x511`，可选 DMA
doorbell 位于 `0x514`；驱动不会越过固件资源访问它，因此 SystemIO 始终是 PIO-only。

## 平台与模块选择

MMIO 协议处理与 CPU 架构无关，可用于 QEMU 提供该节点的 LoongArch64 或 RISC-V64
机器。x86 pc/q35 由 AML namespace 发现 `QEMU0002`，内核只把 `_CRS` 转换成通用
`DeviceResource::IoPort`；实际 `in`/`out` 实现沿
`arch -> StartAcpiIoOps -> DevInitContext -> driver` 注入。驱动和内核 ACPI 层都没有
架构条件编译。

| 项目 | 值 |
| --- | --- |
| Cargo 包 | `platform-fw-cfg` |
| ELM 名称 | `platform.fw-cfg` |
| ELM 模式/阶段 | `m` / `device` |
| 配置项 | `CONFIG_FW_CFG`，默认 `m` |
| 构建顺序 | `platform.firmware-bus` 之后 |
| 匹配 ID | `qemu,fw-cfg-mmio` / `QEMU0002` |

## 对象与生命周期

`QemuFwCfgFactory` 在 ELM `initialize` 注册。probe 按固件 ID 创建 MMIO 或 SystemIO
传输对象，完成签名验证后调用 `fwcfg::install`。服务只允许一个活动实例；重复安装会
作为名称冲突失败。安装 handle 由 PnP 设备拥有，接管失败时会立即 `uninstall`。

设备 `remove` 无额外逻辑，安装 handle 的清理由 PnP 资源负责。ELM `finalize` 注销
driver factory，并在无法安全注销时返回错误。

## 资源、并发与安全

- selector 与后续数据流由事务锁保护；SystemIO 使用跨实例全局锁，因此即使固件重复描述
  同一组端口，一次 `read_item` 也不会与另一次选择操作交错。这与 Linux
  `qemu_fw_cfg_select()` 的全事务锁语义一致。
- MMIO 基址通过 `device_mmio_to_virt` 获得，访问使用 `read_volatile`/
  `write_volatile`。
- MMIO selector 显式调用 `to_be()`；SystemIO selector 按 QEMU 规范把原始 `u16`
  交给 `write_u16`，不能预先交换字节；revision 数据按小端解释。
- SystemIO 缺少架构回调、窗口小于 2 字节、基址未按 16 位对齐或地址溢出时 fail-closed。
- 驱动信任固件将第一段 resource 映射到真实 fw_cfg；签名检查用于拒绝错误设备，但不是
  通用 MMIO 探测或隔离机制。
- 锁只保护设备传输顺序。上层对 selector 长度和数据格式的校验仍由对应消费者负责。

## 依赖关系

实现直接依赖 `general::dev::{platform,pnp,fwcfg}`、`vfs::sync::Spinlock`、`elm`、
`allocator` 和 `log`。清单中的其它 façade 是统一 Kernel API Profile 链接闭包。

## 检查与构建

```sh
cargo check -p platform-fw-cfg --lib --target loongarch64-unknown-none
cargo check -p platform-fw-cfg --lib --target riscv64gc-unknown-none-elf
cargo check -p platform-fw-cfg --lib --target x86_64-unknown-none
cargo test -p platform-fw-cfg --lib --features elm-integrated \
  --target x86_64-unknown-linux-gnu

cargo xtask config
cargo xtask modules --target riscv64gc-unknown-none-elf

cargo elm check drivers/fw-cfg --arch riscv64
cargo elm build drivers/fw-cfg --arch riscv64 --unsigned
```

仓库构建应优先使用 `cargo xtask modules`，由 `.config` 和
[`../Modules.toml`](../Modules.toml) 选择模式。发布 EKI 必须用签名参数替代
`--unsigned`。
