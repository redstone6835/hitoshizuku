# soyo-ld 直接链接器

`soyo-ld` 是 SOYO 的直接静态链接器。它读取 RV64 或 LA64 ELF64
小端 `ET_REL` 对象，在内存中完成段布局、符号解析与重定位，
然后直接生成 SOYO。

完整的 Core 对象容器规范和当前 Wire profile 线格式见根目录
[`SOYO_FORMAT.md`](../../SOYO_FORMAT.md)。本工具当前生成的是文档第 21 节所述的
Wire 可执行 profile，不应把 Wire Header 当作 Core 对象容器 Header。

## 构建

```bash
cargo build --manifest-path tools/soyo-linker/Cargo.toml \
  --target x86_64-unknown-linux-gnu --release
```

## 测试

```bash
cargo test --manifest-path tools/soyo-linker/Cargo.toml \
  --target x86_64-unknown-linux-gnu
```

## 使用

```bash
soyo-ld --target riscv64 --manifest app.json -o app.soyo mrt.o app.o
```

`--target` 可为 `riscv64` 或 `loongarch64`。所有输入必须是同一架构的
ELF `ET_REL` 对象。链接器会在写入前用共享 SOYO 解析器和当前内核
Native ABI 策略自检，并以原子替换方式写入输出。

在编译程序对象前，可从同一份 manifest 生成目标架构的 C 或 Rust ABI binding：

```bash
soyo-ld --target riscv64 --manifest app.json \
  --emit-c-header build/riscv64/native/include/mygo_program.h

soyo-ld --target riscv64 --manifest app.json \
  --emit-rust-module build/riscv64/native/include/mygo_program.rs
```

两种生成物都包含 Native ABI 注册表、程序 Call Slot、capability 要求和
运行时限制。C header 还包含 StartInfo/InitialHandle 布局，Rust module
包含 NativeCall/NativeResult 布局；结构大小和字段偏移分别带有 C11
`_Static_assert` 与 Rust const assertion。编译与最终 SOYO 必须消费同一份
manifest，两种 binding 模式都不能与 `-o` 或对象输入混用。

RV64 建议使用以下 freestanding 参数：

```bash
clang --target=riscv64-unknown-none-elf -ffreestanding -fno-pic -fno-pie \
  -fno-stack-protector -mno-relax -msmall-data-limit=0 -mcmodel=medany -c app.c
```

LA64 建议使用：

```bash
clang --target=loongarch64-unknown-none -ffreestanding -fno-pic -fno-pie \
  -fno-stack-protector -c app.c
```

Rust `no_std` 对象使用目标工具链的 `riscv64imac-unknown-none-elf` 或
`loongarch64-unknown-none` target，并通过 `--emit=obj` 生成 `ET_REL`。RV64
选择 soft-float target 是为了与现有 mrt C 对象保持相同的 psABI flags。

## 程序契约

链接核心使用归一化的 `ProgramContract`。
operation、signature hash、requirement、interface 和 right 均来自
`libs/native-abi` registry。capability 必须显式声明，链接器不会猜测或
扩大授权。

```json
{
  "manifest_version": 1,
  "abi_epoch": 1,
  "entry": "_start",
  "imports": [
    { "operation": "process.exit", "required": true },
    { "operation": "stream.write", "required": true }
  ],
  "capabilities": [
    {
      "requirement": "self_process",
      "rights": ["exit"],
      "required": true
    },
    {
      "requirement": "stdout",
      "rights": ["write"],
      "required": true
    }
  ],
  "runtime": {
    "stack_size": 65536,
    "stack_guard_size": 4096,
    "start_info_max_size": 4096
  }
}
```
