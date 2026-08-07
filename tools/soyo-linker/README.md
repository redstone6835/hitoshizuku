# soyo-ld

`soyo-ld` 是 SOYO 的直接静态链接器。它读取 RV64 或 LA64 ELF64
little-endian `ET_REL` 对象，在内存中完成段布局、符号解析与重定位，
然后直接生成 SOYO。

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
ELF `ET_REL` 对象。链接器会在写入前用共享 SOYO parser 和当前内核
Native ABI policy 自检，并以原子替换方式写入输出。

在编译程序对象前，可从同一份 manifest 生成目标架构的 C ABI binding：

```bash
soyo-ld --target riscv64 --manifest app.json \
  --emit-c-header build/riscv64/native/include/mygo_program.h
```

生成文件包含 Native ABI registry、StartInfo/InitialHandle Wire 布局、程序
Call Slot、capability 要求和 runtime 限制。结构大小和字段偏移带有 C11
`_Static_assert`，编译与最终 SOYO 必须消费同一份 manifest。该模式只生成
header，不能与 `-o` 或对象输入混用。

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

## 程序契约

链接核心使用归一化的 `ProgramContract`。
operation、signature hash、requirement、interface 和 right 均来自
`libs/native-abi` registry。capability 必须显式声明，链接器不会猜测或
扩大授权。

```json
{
  "entry": "_start",
  "imports": [
    { "name": "PROCESS_EXIT", "required": true },
    { "name": "STREAM_WRITE", "required": true }
  ],
  "capabilities": [
    {
      "name": "SELF_PROCESS",
      "rights": ["TERMINATE_SELF"],
      "required": true
    },
    {
      "name": "STDOUT",
      "rights": ["WRITE"],
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
