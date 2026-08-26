# Loongson LS2K I2C 驱动

`platform.ls2k-i2c` 为 LS2X I2C controller 发布 `mygo.device.i2c-bus@1` function，供
其它驱动执行基本的主模式读写。

## 实现范围

- 匹配 `loongson,ls-i2c`，要求至少 8 字节 MMIO；从首个 clock provider 取得 APB 速率，
  缺失/禁用时回退 50 MHz 输入假设。
- 以约 33 kHz 初始化 PRER，提供 write、read、write_regs、read_regs 四个同步 opcode。
- 支持 7-bit address、repeated START、ACK/NACK、arbitration-lost 与有限轮询超时。
- 以 `bus_id` 生成 `i2c-N` function 名；一个 spinlock 串行化整笔 transaction。

当前是窄的轮询主控制器实现，不使用固件 IRQ，也不支持 10-bit address、SMBus、multi-message
数组、DMA、bus recovery、slave mode 或动态频率。clock lease 在 probe 取得速率后没有作为
owned resource 长期保存或在 remove 显式 Disable，因此模块卸载语义仍需补强。

## 模块信息

| 项目 | 值 |
| --- | --- |
| Cargo 包 | `platform-ls2k-i2c` |
| ELM 名称 | `platform.ls2k-i2c` |
| ELM 模式/阶段 | `m` / `device` |
| 建议配置项 | `CONFIG_LS2K_I2C` |
| target | `loongarch64-unknown-none` |
| 前置条件 | firmware bus；推荐 Loongson clock provider |

PnP core 持有 function，解绑时会先使公开入口不可用；driver binding 仅保留 bus 对象，当前
remove 没有额外硬件 quiesce 或 clock 回收逻辑。

## 验证

```sh
cargo check -p platform-ls2k-i2c --lib --target loongarch64-unknown-none
```

该实验性板级总线驱动默认保持 `m`。
