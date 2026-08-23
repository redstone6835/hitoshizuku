# Syscon 与固件电源驱动

`platform.syscon` 提供共享 MMIO syscon registry，并把 DT 风格的
`syscon-poweroff`/`syscon-reboot` 节点转换为统一的关机或重启寄存器写方法。

## 职责与边界

本 ELM 注册两个 PnP driver：

- `platform-syscon` 匹配 `syscon`，按 phandle 登记一个有边界和访问宽度检查的
  `SysconDevice`。
- `platform-syscon-power` 匹配 `syscon-poweroff` 或 `syscon-reboot`，通过 `regmap`
  phandle 查找已登记 syscon，解析 `offset`/`value`，向固件 power core 安装一次
  `RegisterWrite` 方法。

本 crate 不解释 syscon 内的任意业务位，不提供通用用户态寄存器访问，也不负责 ACPI
S5、PSCI、SBI 或 QEMU debug-exit 等其它电源机制。power 节点只描述最终写入，不实现
关机前的文件系统同步、CPU 停机和设备 quiesce。

## 平台与匹配条件

该实现可用于两个目标架构，前提是固件提供兼容节点和 SystemMemory MMIO：

| 项目 | 值 |
| --- | --- |
| Cargo 包 | `platform-syscon` |
| ELM 名称 | `platform.syscon` |
| ELM 模式/阶段 | `m` / `device` |
| 配置项 | `CONFIG_SYSCON`，默认 `m` |
| target 限制 | 无 |
| 构建顺序 | `platform.firmware-bus` 之后 |

syscon 要求 phandle 和第一段 MMIO；`reg-io-width` 默认为 4，只接受 1/2/4/8 字节；
`reg-shift` 默认为 0。power 节点要求 `regmap`、`offset` 和 `value`，其中后两者可由一个
或两个 u32 cell 表示。

## 对象与生命周期

ELM `initialize` 先注册 `SysconFactory`，再注册 `SysconPowerFactory`；第二步失败会回滚
第一步。syscon probe 创建 `MmioSyscon` 并把 registry handle 作为 PnP owned resource。
power probe 查找对应 registry 对象、验证偏移后，把物理寄存器描述安装进 power core，
同时保存 `SysconPowerBinding` 供日志和解绑使用。

`MmioSyscon` 的注销由 PnP resource 完成。当前 power driver 的 remove 只移除 driver data
并记录日志，**没有撤销已安装的 power core 方法**；因此在补齐 power method owner/
uninstall API 前，不应把带活动 power 节点的模块热卸载视为完整支持。

## 资源、并发与安全

- 每次 syscon 访问先应用 `reg-shift`，再检查加法溢出、窗口边界和自然对齐。
- 实际访问宽度由类型枚举约束，使用对应的 8/16/32/64 位易失读写。
- 物理和虚拟地址均来自固件声明的第一段 MMIO；power core 获得的是经过同样检查的物理
  地址和明确访问宽度。
- `MmioSyscon` 当前没有 RMW 或设备锁；单次 read/write 是易失访问，但多个消费者的
  read-modify-write 序列不会自动原子化。共享位域必须由更高层建立锁或专用 API。
- 对固件 phandle 和属性的结构检查不等同于允许任意不可信来宾写 syscon；平台固件仍是
  此安全边界的可信输入。

## 依赖关系

直接依赖 `general::dev::{platform,pnp,syscon}` 与 `general::firmware::power`，并使用
`elm`、`allocator` 和 `log`。power 节点通过 PnP dependency 等待指定 phandle 的
syscon registry 条目，不直接持有另一个 crate 的实现类型。

## 检查与构建

```sh
cargo check -p platform-syscon --lib --target loongarch64-unknown-none
cargo check -p platform-syscon --lib --target riscv64gc-unknown-none-elf

cargo xtask config
cargo xtask modules --target riscv64gc-unknown-none-elf

cargo elm check drivers/syscon --arch riscv64
cargo elm build drivers/syscon --arch riscv64 --unsigned
```

配置和装载顺序由 [`../Modules.toml`](../Modules.toml) 管理。未签名镜像只适合本地测试。
