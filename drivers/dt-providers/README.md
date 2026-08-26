# 标准 Device Tree provider 驱动

`platform.dt-providers` 把设备树固定时钟节点注册到通用 DT provider registry，使板级
consumer 可以通过 phandle 获取 clock lease。

## 实现范围

- 匹配 `fixed-clock` 与 `fixed-factor-clock`，严格要求 `#clock-cells = <0>` 和 phandle。
- `fixed-clock` 从单个 32 位 `clock-frequency` 返回速率；Enable/Disable 为无副作用成功。
- `fixed-factor-clock` 要求一个外部父 `clocks` 引用、`clock-mult` 和非零 `clock-div`，
  转发 Enable/Disable，并以向下取整且检查溢出的方式计算速率。
- 不提供门控、分频重编程、父选择、频率设置或其它 clock binding。

这是固定 clock binding 的基础实现，不应被理解为通用 clock framework。属性编码异常、
自引用父 clock、非空 specifier 或计算溢出都会拒绝 probe/acquire。

## 模块信息

| 项目 | 值 |
| --- | --- |
| Cargo 包 | `platform-dt-providers` |
| ELM 名称 | `platform.dt-providers` |
| ELM 模式/阶段 | `m` / `device` |
| 建议配置项 | `CONFIG_DT_PROVIDERS` |
| target | `loongarch64-unknown-none`、`riscv64gc-unknown-none-elf` |
| 前置条件 | firmware bus 已枚举 DT platform 节点 |

provider handle 和 fixed-factor 的父 clock lease 都由 PnP owned resource 管理；解绑先撤销
provider，再释放父 lease，probe 失败也沿同一路径回滚。

## 验证

```sh
cargo check -p platform-dt-providers --lib --target loongarch64-unknown-none
cargo check -p platform-dt-providers --lib --target riscv64gc-unknown-none-elf
```

通用 provider 仍默认保持 `m`，需要它供启动关键 consumer 使用的板型可显式内置。
