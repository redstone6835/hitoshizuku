# QEMU fw_cfg MMIO 驱动

`platform.fw-cfg` 实现 QEMU fw_cfg 的 MMIO 只读传输，并把唯一活动实例安装到
`general::dev::fwcfg`。配置项的业务解释属于上层，不属于该驱动。

## 职责与边界

- 匹配 `compatible = "qemu,fw-cfg-mmio"`。
- 校验第一段 MMIO 窗口至少覆盖 data、selector 和 DMA 寄存器区域。
- 以大端写入 16 位 selector，再从 data 端口连续读取字节。
- probe 时读取 selector `0x0000` 并要求签名为 `QEMU`；revision 仅用于日志。
- 向全局 fw_cfg 服务安装 `FwCfgDevice`，并把安装 handle 交给 PnP 管理。

当前实现只提供 `read_item`。它不启用 DMA，不写配置项，也不解释 initrd、SMBIOS、
ACPI、文件目录或其它 selector 内容。虽然窗口尺寸检查覆盖 DMA 寄存器，本驱动并未访问
该寄存器。

## 平台与模块选择

MMIO 协议处理与 CPU 架构无关，可用于 QEMU 提供该节点的 LoongArch64 或 RISC-V64
机器。

| 项目 | 值 |
| --- | --- |
| Cargo 包 | `platform-fw-cfg` |
| ELM 名称 | `platform.fw-cfg` |
| ELM 模式/阶段 | `m` / `device` |
| 配置项 | `CONFIG_FW_CFG`，默认 `m` |
| 构建顺序 | `platform.firmware-bus` 之后 |
| 匹配 ID | `qemu,fw-cfg-mmio` |

## 对象与生命周期

`QemuFwCfgMmioFactory` 在 ELM `initialize` 注册。probe 创建一个保存物理地址、虚拟
基址和传输锁的 `QemuFwCfgMmio`，完成签名验证后调用 `fwcfg::install`。服务只允许一个
活动实例；重复安装会作为名称冲突失败。PnP 资源接管失败时会立即 `uninstall`。

设备 `remove` 无额外逻辑，安装 handle 的清理由 PnP 资源负责。ELM `finalize` 注销
driver factory，并在无法安全注销时返回错误。

## 资源、并发与安全

- selector 与后续数据流共享一个 `Spinlock<()>`；一次 `read_item` 不会与另一次选择操作
  交错，这是 fw_cfg 有状态传输的必要条件。
- MMIO 基址通过 `device_mmio_to_virt` 获得，访问使用 `read_volatile`/
  `write_volatile`。
- selector 显式调用 `to_be()`，revision 数据按小端解释。
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

cargo xtask config
cargo xtask modules --target riscv64gc-unknown-none-elf

cargo elm check drivers/fw-cfg --arch riscv64
cargo elm build drivers/fw-cfg --arch riscv64 --unsigned
```

仓库构建应优先使用 `cargo xtask modules`，由 `.config` 和
[`../Modules.toml`](../Modules.toml) 选择模式。发布 EKI 必须用签名参数替代
`--unsigned`。
