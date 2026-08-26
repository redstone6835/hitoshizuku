# StarFive JH7110 MMC 驱动

`platform.jh7110-mmc` 为 VisionFive 2 的 DesignWare Mobile Storage Host 提供启动期
SD/eMMC 块设备支持，并把探测到的介质发布为 `/dev/mmcblkN`。

## 实现范围

- 匹配 `starfive,jh7110-mmc`，校验控制器 MMIO 窗口并根据版本选择 FIFO 数据口。
- 通过 clock DT provider 取得 `ciu` 频率；provider 未就绪时返回显式 PnP dependency。
- 支持 SD 的 CMD0/8/55/ACMD41 和 eMMC 的 CMD0/1 初始化，再通过 CMD2/3/9/7 取得
  RCA、CSD 和容量。
- 保守使用 1-bit、最高 25 MHz，总线读采用 JH7110 IDMAC 单描述符和 512 字节 bounce
  buffer，写采用轮询 PIO CMD24。
- 通过通用 `BlockDevice`/`BlockFunction` 接收 512 字节块的读写 BIO；支持分段 buffer
  的逐块散收以及空操作 flush。

驱动目前属于实验性板级实现。它只支持单块 CMD17/CMD24，不支持多块传输、IRQ、卡插拔、
电压切换、UHS/HS200 调谐、discard 或可靠 flush。IDMAC 只有 32 位寻址，并从一组固定的
低物理地址候选页中申请永久 bounce page；这要求 VisionFive 2 的内存布局保留这些区域，
后续应由通用 DMA constraint allocator 取代。设备移除不会回收该永久页。

迁移时已删除旧分支用于无串口调试的固定 LBA 签名写入。probe 和设备绑定过程不会修改
任意预设扇区；介质写入只会来自块层提交的正式 `BioOp::Write` 请求。

## 模块信息

| 项目 | 值 |
| --- | --- |
| Cargo 包 | `platform-jh7110-mmc` |
| ELM 名称 | `platform.jh7110-mmc` |
| ELM 模式/阶段 | `m` / `device` |
| 建议配置项 | `CONFIG_JH7110_MMC` |
| target | `riscv64gc-unknown-none-elf` |
| 前置条件 | firmware bus、`platform.jh7110-crg` |

ELM 生命周期注册和注销一个 factory。probe 获取的 clock lease 和块 function 都归 PnP
设备所有；初始化失败不会发布半初始化块设备。当前 I/O 路径以全局原子锁串行化共享的
IDMAC bounce page，因此同一时间只有一个 MMC 数据传输。

## 验证

```sh
cargo check -p platform-jh7110-mmc --lib --target riscv64gc-unknown-none-elf
cargo elm check drivers/jh7110-mmc --arch riscv64
```

上面的静态检查不能替代实机读写、掉电和边界 LBA 测试。发布板级配置前应至少验证 SD
与 eMMC 的只读启动路径，并在可恢复介质上单独验证写路径。模块默认应保持 `m`；板级配置
需要内置根设备时可显式选择 `y`。
