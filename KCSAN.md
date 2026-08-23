# KCSAN 数据竞争检测

KCSAN 是面向调试内核的采样式数据竞争检测器。编译器为普通 Rust 内存访问插入 hook，运行时短暂建立 watchpoint，并在其他执行路径发生重叠且至少一侧为写操作时记录报告。检测器在所有 AP 启动完成后启用，不覆盖早期启动竞争。它不保证单次运行发现所有竞争，应在 SMP 和受限宿主 CPU 环境下重复运行目标负载。

## 构建与运行

使用当前系统的默认 Rust 工具链。`kcsan` feature 只把运行时链接进 kernel；编译器插桩还
必须通过仓库提供的 wrapper 显式启用：

```sh
RUSTC_WRAPPER="$PWD/scripts/kcsan-rustc-wrapper.sh" \
  cargo xtask build --target loongarch64-unknown-none --features kcsan
RUSTC_WRAPPER="$PWD/scripts/kcsan-rustc-wrapper.sh" \
  cargo xtask build --target riscv64gc-unknown-none-elf --features kcsan
```

`xtask` 使用按架构隔离的 Cargo 目录，产物分别位于
`target/loongarch64/loongarch64-unknown-none/release/kernel` 和
`target/riscv64/riscv64gc-unknown-none-elf/release/kernel`。RISC-V 产物是带调试信息的
ELF；LoongArch64 启动镜像是 PE。当前构建入口不会自动生成 `kernel.map` 或 KCSAN
快照目录。KCSAN 构建会显著改变时序和性能，不得用于性能基准或发布镜像。

例如 LoongArch64 可将普通 QEMU 命令中的 `-kernel` 参数替换为对应 KCSAN 构建产物，保留 `-smp 8`。慢宿主复现可在运行环境中限制 CPU 数量。

## 阅读与定位报告

竞争报告包含冲突地址以及两侧的访问类型、宽度、CPU、任务 ID 和 PC：

```text
[kcsan] data race seq=7 address=0x... first(kind=write size=8 cpu=1 task=42 pc=0x...) second(kind=read size=8 cpu=6 task=65 pc=0x...)
```

必须使用与运行镜像同一次构建的符号产物解析两个 PC。RISC-V64 镜像是保留符号的 ELF，可直接执行：

```sh
riscv64-linux-gnu-addr2line \
    -e target/riscv64/riscv64gc-unknown-none-elf/release/kernel -f -C -p \
    0xFIRST_PC 0xSECOND_PC
```

也可使用 `llvm-addr2line`；当前 LLVM 版本可能对部分 DWARF range 发出警告，但仍能给出
函数和源码行。LoongArch64 的可启动产物是 PE，不能直接交给常见的 `addr2line`。若调用方
在同一次链接中另行保存了 LLD map、kernel 副本及哈希清单，可用仓库脚本做函数级定位：

```sh
scripts/kcsan-symbolize.py build/kcsan/loongarch64/kernel.map \
    0xFIRST_PC 0xSECOND_PC
```

保留原始 PC，以便后续配合 DWARF sidecar 做行号定位。

wrapper 自身的代码生成约束可以独立验证；该测试要求 `llvm-nm`，也可通过环境变量指定
兼容的 `nm`：

```sh
LLVM_NM=/usr/bin/nm scripts/test-kcsan-codegen.sh
python3 -m unittest scripts.tests.test_kcsan_symbolize
```

`report ring overwritten` 表示报告产生速度超过 100 ms 排空速度；`report publication busy` 表示多个 CPU 同时提交报告时，为保证检测器不阻塞而丢弃了少量报告。两者都应优先处理已保留的报告，再缩小负载或拆分复现阶段。

## 覆盖范围与限制

- 核心内核 Cargo 构建图中的普通 Rust 读写自动覆盖，无需在每个字段上手工标注。
- 配置为 `y` 的集成 ELM 和配置为 `m` 的 EKI 由独立流程编译，当前不自动插桩；其私有状态与跨 ELM 竞争不在完整覆盖范围内。
- 自动插桩不跟踪 Rust 原子操作，检测器也不把两个显式原子访问视为冲突；volatile/MMIO 访问同样默认忽略。KCSAN 不验证内存序选择是否正确。
- 架构优化的 `memcpy`/`memmove`/`memset` 及 LLVM memory intrinsic 不会被逐字节替换，因此共享缓冲区上仅经这些入口发生的冲突可能漏报。
- 未报告不等于无竞争。采样、调度时序、报告去重和未插桩路径都可能导致假阴性；修复后还应使用定向回归测试验证具体不变式。
