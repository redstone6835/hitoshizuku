# JH7110 TRNG

`platform.jh7110-trng` 在 VisionFive 2 设备探测阶段从 JH7110 TRNG 读取一次 256 bit
硬件种子，并提交给内核随机子系统。

## 实现范围

- 匹配 `starfive,jh7110-trng`，要求 MMIO 窗口覆盖 `0x64` 寄存器。
- 通过 DT provider 获取 `hclk`、`ahb` 和 reset，依次开时钟、解除复位、执行 reseed 和
  generate，完成后关闭两个时钟。
- 轮询 seed-done/random-ready，并显式识别 LFSR lockup 和超时。
- 把八个 32 位随机寄存器组成 32 字节种子，调用
  `general::dev::random::add_bootloader_randomness` 注入随机池。
- probe 失败时按已完成阶段回滚时钟，不发布字符设备或长期运行接口。

这是启动期一次性熵源，不是通用 `/dev/hwrng` 驱动。它没有 IRQ 模式、连续取数、运行时
健康统计、熵率校准或周期性重播种；当前日志中的 256 bit 表示提交的数据长度，不应独立
作为硬件最小熵证明。成熟度为“实现完整、仍需新树实机复验”。

## 模块信息

| 项目 | 值 |
| --- | --- |
| Cargo 包 | `platform-jh7110-trng` |
| ELM 名称 | `platform.jh7110-trng` |
| ELM 模式/阶段 | `m` / `device` |
| 建议配置项 | `CONFIG_JH7110_TRNG` |
| target | `riscv64gc-unknown-none-elf` |
| 前置条件 | firmware bus、`platform.jh7110-crg`、内核随机核心 |

`src/engine.rs` 将寄存器序列与 MMIO 分离，`src/status.rs` 负责状态判定；对应测试覆盖正常、
lockup 和超时语义。

```sh
cargo check -p platform-jh7110-trng --lib --target riscv64gc-unknown-none-elf
cargo test -p platform-jh7110-trng --test engine
cargo test -p platform-jh7110-trng --test status
cargo elm check drivers/jh7110-trng --arch riscv64
```

该 SoC 专属驱动默认保持 `m`；板级配置若要求在早期初始化随机池，可以显式选择 `y`。
