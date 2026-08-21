# 代码风格指南

---

## 目录

1. [文档与注释](#1-文档与注释)
2. [命名规范](#2-命名规范)
3. [格式化](#3-格式化)
4. [语句与表达式](#4-语句与表达式)
5. [不安全代码](#5-不安全代码)
6. [错误处理](#6-错误处理)
7. [原子操作与内存序](#7-原子操作与内存序)
8. [日志](#8-日志)
9. [Crate 组织](#9-crate-组织)

---

## 1. 文档与注释

### 1.1 模块文档

每个 `.rs` 文件以 `//!` 模块文档块开头。第一行为模块名或一句话概述，空行后接详细说明。中文编写。

```rust
//! 模块名或一句话概述。
//!
//! 本模块实现 ... 功能。详细描述在本段展开，说明设计意图、关键约定和边界条件。
//! 多段之间以空行分隔，每段专注一个主题。
```

**示例：**

```rust
//! 内核全局 console。
//!
//! console 本质上是一个 [`CharDriver`]，无需额外 trait 包装。
//! 注册后，[`print!`] / [`println!`] 宏通过 [`console_write`] 整串批量写入，
//! 充分利用驱动内部 FIFO，避免逐字节调用的开销。

//! 架构相关的分页操作接口。
//!
//! 该 trait 位于 `hal` 层，隔离分页通用逻辑与具体 ISA 页表格式。
//! 这里使用 `hal` 层 newtype 表达跨层边界上的地址语义，避免把上层
//! crate 的地址类型或裸 `usize` 暴露给架构实现。
```

### 1.2 节分隔符

模块内大节使用等宽分隔线，小节使用短分隔线。分隔线上方保留空行。

```rust
// ── 大节标题 ────────────────────────────────────────────────────────────────────

// 代码...

// ─ 小节标题 ───────────────────────────────────────────────────
```

**实际示例：**

```rust
// ── 稳定计时器频率（从 CPUCFG 读取，默认 100 MHz） ────────────────────────
//
// LoongArch64 提供了一个"稳定计数器"… 详细注释多行以 `//` 开头。

// ── 内核持有的 EFI 系统表副本 ─────────────────────────────────────────────
//
// UEFI 固件将系统表的指针… 详细注释。

// ─ 表头 ──────────────────────────────────────────────────────────
let sig_bytes = t.hdr.signature.to_le_bytes();
```

### 1.3 函数文档

使用 `///` 三斜线文档注释。复杂函数须包含多段：概述、`# 参数`、`# 返回值`、`# Safety`（若适用）、`# Panics`（若适用）。

```rust
/// 在页表中查找覆盖 `vaddr` 的现有叶子页表项。
///
/// 此函数从页表根开始遍历，逐层检查 PTE 直到找到叶子项（或遍历完所有层级）。
/// 与 [`walk_and_map`] 不同，它**不分配**新的页表页，仅查找现有映射。
///
/// # 参数
///
/// - `root_vaddr`: 页表根的虚拟地址。
/// - `vaddr`: 要查找的虚拟地址。
/// - `phys_to_virt`: 物理地址到虚拟地址的转换函数（架构提供）。
///
/// # 返回值
///
/// 成功时返回 `(level, pte_ptr, pte)` 三元组：
/// - `level`: 叶子项所在的层级（0 = 最大页）。
/// - `pte_ptr`: 叶子 PTE 的内存地址指针，可用于后续修改或清除。
/// - `pte`: 叶子 PTE 的当前值。
pub fn find_leaf<P: PagingArch>(...) -> Result<(usize, *mut usize, P::Pte), MapError> {
```

### 1.4 行内注释

使用 `//` 在代码上方或行尾写中文注释。注释描述 **why**，而非重复代码的 what。

```rust
let mut cursor = 0usize;
while cursor + entry_bytes <= reg.len() {
    let addr_end = cursor + addr_cells * 4;
    let size_end = addr_end + size_cells * 4;
    // DTB 的 reg 属性以 big-endian u32 存储
    let Some(start) = read_cells(&reg[cursor..addr_end], addr_cells) else {
        break;
    };
    let Some(size) = read_cells(&reg[addr_end..size_end], size_cells) else {
        break;
    };
    cursor = size_end;

    if size == 0 {
        continue;
    }
    segments.push(allocator::MemorySegment { start, size });
}
```

---

## 2. 命名规范

### 2.1 类型、Trait、枚举

全部 `CamelCase`。

```rust
pub trait PagingArch { }
pub struct BuddyAllocator { }
pub enum MapError { }
pub struct KernelMemorySubsystem { }
```

### 2.2 函数、方法、变量

全部 `snake_case`。

```rust
fn parse_memory_segments(dtb: Dtb<'_>) -> Option<Vec<allocator::MemorySegment>>
fn kernel_dtb() -> Option<Dtb<'static>>
let dtb_size = dtb_bytes.len();
let mut merged: Vec<allocator::MemorySegment> = Vec::with_capacity(segments.len());
```

### 2.3 常量与静态变量

**编译期常量** (`const`) 使用 `SCREAMING_SNAKE_CASE`：

```rust
const PAGE_SIZE: usize = 4096;
const MAX_TRACKED_ORDER: usize = usize::BITS as usize - PAGE_SHIFT;
const SINK_LINE_BUFFER_SIZE: usize = 1280;
const DTB_BUF_SIZE: usize = 4096 * 1024;
```

**运行时静态变量** (`static`) 使用 `SCREAMING_SNAKE_CASE`：

```rust
static CONSOLE_DATA_PTR: AtomicUsize = AtomicUsize::new(NULL_PTR);
static CONSOLE_VTABLE_PTR: AtomicUsize = AtomicUsize::new(NULL_PTR);
```

**可变的静态变量** (`static mut`) 使用 `SCREAMING_SNAKE_CASE`：

```rust
static mut KERNEL_EFI_TABLE: MaybeUninit<EfiSystemTable> = MaybeUninit::uninit();
static mut KERNEL_DTB_BUF: [u8; DTB_BUF_SIZE] = [0u8; DTB_BUF_SIZE];
```

### 2.4 关联类型

`PascalCase`，与 trait 泛型参数对齐：

```rust
pub trait PagingArch {
    type Pte: Copy;
    type Flags: Copy;
}
```

### 2.5 生命周期参数

单字母小写或描述性名称（罕见）：

```rust
fn parse_memory_segments(dtb: Dtb<'_>) -> Option<Vec<allocator::MemorySegment>>
```

### 2.6 未使用但有意保留的变量

前缀 `_`：

```rust
let _guard = self.init_lock.lock();
self.metadata.bind_boot_source(&self.boot);
// _guard 在此作用域结束时 drop，释放 init_lock
```

### 2.7 类型别名

`CamelCase`，多用于函数指针类型和具名导出：

```rust
pub type PhysToVirtFn = fn(paddr: usize) -> usize;
pub type VirtToPhysFn = fn(vaddr: usize) -> usize;
pub type CpuIdFn = fn() -> usize;
pub type PhysicalMemoryManager = BuddyAllocator;
pub type AddressSpaceManager = KernelAddressSpace;
```

---

## 3. 格式化

### 3.1 缩进与行宽

4 空格缩进。行宽不设硬限制，但优先可读性换行。

### 3.2 花括号

同一行开括号，下一行语句，单独一行闭括号。

```rust
impl SinkLineBuffer {
    const fn new() -> Self {
        Self {
            buf: [0; SINK_LINE_BUFFER_SIZE],
            len: 0,
        }
    }
}
```

### 3.3 match 分支

`match` 中每个分支的 `=>` 后使用大括号块（单表达式除外）：

```rust
match result {
    Ok(()) => {
        self.metadata.enable_dynamic();
        Ok(())
    }
    Err(err) => {
        log::info!("[alloc][init] init_vmem failed err={:?}", err);
        Err(match err {
            AddressSpaceError::MetadataOutOfMemory => InitError::MetadataOutOfMemory,
            _ => InitError::AddressSpaceInitFailed,
        })
    }
}
```

当 match 分支仅导致赋值时，可使用紧凑风格：

```rust
let hz = if cc_mul == 0 || cc_div == 0 {
    cc_freq as usize
} else {
    (cc_freq as u64 * cc_mul / cc_div) as usize
};
```

### 3.4 结构体初始化

紧凑风格：字段在同一行（字段少时），或每个字段独立一行（字段多时）。长结构体字段对齐。

```rust
// 短结构体：单行
self.active.store(true, Ordering::Release);

// 长结构体：每行一个字段
AllocStats {
    total_allocs: self.total_allocs.load(Ordering::Acquire),
    total_deallocs: self.total_deallocs.load(Ordering::Acquire),
    total_reallocs: self.total_reallocs.load(Ordering::Acquire),
    total_bytes_allocated: self.total_bytes_allocated.load(Ordering::Acquire),
    total_bytes_freed: self.total_bytes_freed.load(Ordering::Acquire),
    oom_count: self.oom_count.load(Ordering::Acquire),
    ownership_failures: self.ownership_failures.load(Ordering::Acquire),
    boot_used_bytes: boot.used_bytes,
    vmem_used_bytes: address_space.kernel.allocated_size,
}
```

### 3.5 函数参数

参数少且短时保持单行；参数多或表达式长时，参数列表与返回类型各占一行：

```rust
// 短参数：单行
pub fn free_physical(&self, allocation: PhysicalAllocation) -> bool

// 长参数：每行一个
pub fn alloc_kernel_backed_range(
    &self,
    order: usize,
    phys: &Mutex<BuddyAllocator>,
    page_policy: PagePolicy,
) -> Result<BackedRange, AddressSpaceError> {
```

### 3.6 trait 定义

关联类型、常量和方法之间不留空行（紧密组合）：

```rust
pub trait PagingArch {
    type Pte: Copy;
    type Flags: Copy;

    const PAGE_SIZE: usize;
    const LEVELS: usize;
    const ENTRIES_PER_TABLE: usize;

    fn is_canonical_vaddr(vaddr: usize) -> bool;
    fn level_index(vaddr: usize, level: usize) -> usize;
    fn invalid_pte() -> Self::Pte;
    fn pte_is_valid(pte: Self::Pte) -> bool;
}
```

### 3.7 属性

`#[inline]` 位于 `fn` 之前；`#[derive(...)]` 紧跟结构体/枚举定义之后：

```rust
#[derive(Debug, Clone, Copy)]
pub enum MapError { ... }

#[inline]
fn now_ns() -> u64 {
    log::get_timestamp_ns()
}
```

---

## 4. 语句与表达式

### 4.1 分号

表达式语句以分号结尾；`return` 语句可省略（使用尾表达式）。`if let` 中无分号表示返回值：

```rust
fn as_bytes(&self) -> &[u8] {
    &self.buf[..self.len]  // 无分号：返回表达式
}
```

### 4.2 空指针检查

`is_null()` 用于裸指针，`== 0` 用于 `usize` 地址：

```rust
if !t.con_out.is_null() { ... }
if ptr.is_null() { ... }
if ptr == 0 { return Ok(()); }
```

### 4.3 `let ... else`

用于条件解构的提前退出：

```rust
let Some(phys_to_virt) = self.load_phys_to_virt() else {
    return Err(InitError::MissingPhysToVirt);
};
```

### 4.4 链式调用长行处理

链式调用过长时合理使用中间变量，优先可读性：

```rust
let vendor_ascii = if t.firmware_vendor.is_null() {
    alloc::string::String::from("<null>")
} else {
    match unsafe { t.firmware_vendor_cstr16(256) } {
        Some(units) => {
            let bytes: alloc::vec::Vec<u8> = units
                .iter()
                .map(|&cu| if cu < 0x80 { cu as u8 } else { b'?' })
                .collect();
            alloc::string::String::from_utf8(bytes)
                .unwrap_or_else(|_| alloc::string::String::from("<encoding error>"))
        }
        None => alloc::string::String::from("<too long>"),
    }
};
```

### 4.5 整数后缀

当表达式右侧需要显式类型推断或语境不清时使用类型后缀：

```rust
let mut cursor = 0usize;
const DTB_BUF_SIZE: usize = 4096 * 1024;
self.buf[..0u8.len()]  // 明确无符号类型
```

### 4.6 `Self` 构造函数

实现 `new()` 时返回 `Self { ... }`：

```rust
impl KernelMemorySubsystem {
    pub const fn new() -> Self {
        Self {
            boot: BootAllocator::new(),
            phys: Mutex::new(BuddyAllocator::new()),
            ...
        }
    }
}
```

---

## 5. 不安全代码

### 5.1 安全注释

每个 `unsafe` 块内必须有 `// Safety:` 注释，解释：
- 操作为什么是 unsafe
- 前置条件是什么
- 当前上下文为什么满足这些条件

```rust
let ptr = CONSOLE_VTABLE_PTR.load(Ordering::Acquire);
if ptr == NULL_PTR {
    return None;
}
// Safety: ptr 非空时指向 'static 对象，注册后永不移动/释放。Acquire load
// 保证在见到非空 vtable 时，对应的 data 写入已完全可见。
Some(unsafe { (*ptr.as_ref()?).method() })
```

### 5.2 `static mut` 访问

**绝不** 在 `static mut` 上创建可变引用。使用裸指针 + `ptr::write` / `ptr::read`：

```rust
// 正确：通过 addr_of_mut! 获取裸指针后写入
unsafe {
    addr_of_mut!(KERNEL_EFI_TABLE).write(MaybeUninit::new(*fw_table));
    EFI_TABLE_VALID.store(true, Ordering::Release);
}

// 正确：通过 addr_of! 获取裸指针后读取
let slice = unsafe {
    core::slice::from_raw_parts(addr_of!(KERNEL_DTB_BUF).cast::<u8>(), len)
};
```

```rust
// 错误（禁止）：
let buf = &mut KERNEL_DTB_BUF;  // UB!
```

### 5.3 `asm!` 宏

仅在 `arch` crate 内使用。每个 `asm!` 周围必须有 `unsafe` 块，并必须在每段中给出寄存器语义的详细注释：

```rust
let cnt: u64;
unsafe {
    core::arch::asm!(
        // 将当前稳定计数器的值写入 cnt 中。由于我们不需要计数器编号，可以直接写入 $zero 扔掉。
        "rdtime.d {cnt}, $zero",
        cnt = out(reg) cnt,
    );
}
```

### 5.4 外部 FFI

`unsafe extern "C"` 函数声明用于内核入口和链接器符号：

```rust
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __kernel_start_main() -> ! {
    log::debug!("[main] jumped into __kernel_start_main()");
    main()
}

// 声明链接器脚本定义的符号（不调用，仅取地址）
unsafe extern "C" {
    fn sheap();
    fn eheap();
}
let heap_start = sheap as *const () as usize;
```

---

## 6. 错误处理

### 6.1 错误枚举

使用 `#[derive(Debug, Clone, Copy)]`。每个 variant 对应一种明确错误条件。中文注释。

```rust
#[derive(Debug, Clone, Copy)]
pub enum MapError {
    /// 物理内存不足，无法分配中间页表页。
    OutOfMemory,
    /// 虚拟地址或物理地址未按要求对齐。
    Misaligned,
    /// 目标地址已被映射，不支持覆盖已有映射。
    AlreadyMapped,
}
```

### 6.2 Result 使用

公开 API 返回 `Result<T, ErrorType>`。仅在逻辑上无失败可能时使用直接返回值。

```rust
pub fn allocate_physical(
    &self,
    request: PhysicalAllocRequest,
) -> Result<PhysicalAllocation, buddy::BuddyAllocError>

pub fn kernel_dtb() -> Option<Dtb<'static>>
```

### 6.3 错误传播

使用 `?` 运算符传播错误。在需要额外上下文时使用 `match` 转换错误类型：

```rust
let init_result = { ... };
match init_result {
    Ok(()) => Ok(()),
    Err(err) => {
        log::info!("[alloc][init] init_vmem failed err={:?}", err);
        Err(match err {
            AddressSpaceError::MetadataOutOfMemory => InitError::MetadataOutOfMemory,
            _ => InitError::AddressSpaceInitFailed,
        })
    }
}
```

### 6.4 panic

仅在不可恢复的不变量被违反时使用。消息格式 `[component][phase] ...`：

```rust
panic!(
    "[alloc][kheap] registry owned large allocation but kheap rejected free: \
     ptr={:#x} paddr={:?} order={} err={:?}",
    ptr, record.paddr, record.order, err
);
```

外部输入的严格验证失败也使用 `panic!`（因内核没有"调用者"可返回错误）：

```rust
.unwrap_or_else(|| panic!("[init] DTB magic/layout check failed at {:#x}", fdt_addr));
```

---

## 7. 原子操作与内存序

### 7.1 显式 Ordering

所有原子操作都必须显式传递 `Ordering`，绝不依赖默认值：

```rust
BACKEND.store(backend as usize, Ordering::Release);
BACKEND.load(Ordering::Acquire)
self.active.store(true, Ordering::Release);
STABLE_TIMER_HZ.store(hz, Ordering::Relaxed);
```

### 7.2 Acquire-Release 配对

写入端用 `Release`，读取端用 `Acquire`，保证 happens-before：

```rust
// 写入端
unsafe { addr_of_mut!(KERNEL_EFI_TABLE).write(MaybeUninit::new(*fw_table)); }
EFI_TABLE_VALID.store(true, Ordering::Release);

// 读取端
EFI_TABLE_VALID
    .load(Ordering::Acquire)
    .then(|| unsafe { (*addr_of!(KERNEL_EFI_TABLE)).assume_init_ref() })
```

### 7.3 适合 Relaxed 的场景

仅当不需要与其他内存位置建立 happens-before 关系时使用 `Relaxed`：

- 时间戳计数器频率
- per-CPU 统计计数器（不与锁交互时）
- 拆分 fat pointer 的 data 半（vtable 半负责 Release）

```rust
CONSOLE_DATA_PTR.store(data, Ordering::Relaxed);
CONSOLE_VTABLE_PTR.store(vtable, Ordering::Release);
```

---

## 8. 日志

### 8.1 日志宏

使用 `log` crate 的标准级别宏：`log::debug!`、`log::info!`。`log::info!` 也可使用 `printk!`：

```rust
log::debug!("[main] jumped into main()");
log::info!("[alloc][init] init_vmem failed err={:?}", err);
printk!("[init] boot allocator: {:#x}..{:#x} ({} MiB)", heap_start, heap_end, heap_size / (1024 * 1024));
```

### 8.2 日志前缀格式

`[component][phase]` 格式，方括号内用小写标识组件和阶段：

```
[main]
[boot]
[init]
[alloc][boot]
[alloc][init]
[alloc][phys]
[alloc][route]
[alloc][managed]
[alloc][reclaim]
[alloc][invariant]
[efi]
```

```rust
log::debug!("[alloc][phys] request size={} align={} page_policy={:?} placement={:?}", ...);
log::debug!("[alloc][route] domain=Kernel path=kheap size={} align={} cpu={}", ...);
log::info!("[alloc][managed] default heap enabled base={:#x} size={}", ...);
```

### 8.3 调试输出格式

键值对使用 `key={:?}` 或 `key={}` 格式，与 Debug/Display 对齐：

```rust
log::debug!(
    "[alloc][phys] success paddr={:#x} size={} order={} page_size={}",
    allocation.paddr,
    allocation.size,
    allocation.order,
    allocation.page_size,
);
```

---

## 9. Crate 组织

### 9.1 `lib.rs` 文档

每个 crate 的 `lib.rs` 以 `//!` 模块文档开头，描述 crate 的总体定位。

```rust
#![no_std]
//!
//! 分层内核分配器总入口。
//!
//! 这个 crate 不是单一算法的封装，而是一套"按职责分层"的内核内存子系统……
```

### 9.2 模块声明

`mod` 声明按层从上到下排列：

```rust
mod boot;
mod buddy;
mod error;
mod gc;
mod kheap;
mod managed;
mod metadata;
mod registry;
mod request;
mod slab;
mod space;
pub mod stats;
mod vmem;
```

### 9.3 重导出 (`pub use`)

分组导出，相同来源的条目放在同一行或相邻行。类型别名紧跟定义：

```rust
pub use buddy::{BuddyAllocator as PhysicalAllocator, BuddyStats, MemorySegment, PAGE_SIZE};
pub use error::{
    AddressSpaceError, AllocationError, DeallocationError, InitError, ManagedHandleError,
    OwnershipError, RegistryError, VmemError,
};
```

### 9.4 依赖声明

`Cargo.toml` 中依赖按路径分组，无额外注释：

```toml
[dependencies]
general = { path = "../general" }
allocator = { path = "../libs/allocator" }
log = { path = "../libs/log" }
spin = { version = "0.9", default-features = false, features = ["spin_mutex"] }
```

### 9.5 crate 属性

所有非顶层 kernel crate 声明 `#![no_std]`。sysroot crate（如 `allocator`）添加 `extern crate alloc`：

```rust
#![no_std]
extern crate alloc;
```

内核 crate（`kernel`）额外声明：

```rust
#![no_std]
#![no_main]

extern crate alloc;
extern crate allocator;
extern crate arch;
```

---
