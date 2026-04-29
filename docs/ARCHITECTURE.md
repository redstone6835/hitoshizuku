# 架构设计文档

---

## 目录

1. [架构总览](#1-架构总览)
2. [层级定义](#2-层级定义)
3. [依赖规则](#3-依赖规则)
4. [能力注入机制](#4-能力注入机制)
5. [不安全代码治理](#5-不安全代码治理)
6. [并发模型](#6-并发模型)

---

## 1. 架构总览

本项目架构遵循以下核心原则：

1. **架构无关与架构相关彻底分离。** 通用算法和数据结构不包含任何 ISA 特定知识，仅通过 trait 契约与架构实现交互。

2. **hal 统一 arch 对外接口。** hal 层将不同架构（LoongArch64、RISC-V 64）的差异化实现收敛为统一的调用接口。kernel 只调用 hal，不直接接触 arch。

3. **general 提供通用算法与标准化接口。** general 层定义平台 trait（如 `PagingArch`），同时提供依赖这些 trait 的泛型算法（如页表遍历）。arch 实现 general 定义的 trait，并调用 general 的泛型算法。

4. **kernel 不直接接触 arch。** kernel 仅依赖 hal（统一接口）、general（通用算法）和 libs（算法库），不得直接依赖 arch crate。

5. **能力通过运行期注入。** 关键子系统后端（分配器、日志、控制台）通过全局原子指针在运行期注册，支持引导期到运行时的无锁后端切换。

6. **unsafe 代码收敛于架构层与底层原语。** 上层策略逻辑在安全 Rust 中编写。

---

## 2. 层级定义

```
┌──────────────────────────────────────────────────────────────────┐
│                        kernel（策略与集成）                      │
├──────────────────────────────────────────────────────────────────┤
│ hal（统一架构接口）general（通用算法/标准接口）libs（可复用算法）│
│      包装 arch 实现      · 架构 trait 定义     · 分配器  · VFS   │
│ 面向 kernel 的统一 API  · 泛型页表遍历 · DTB   · 日志 · 错误码   │
│                  · 控制台框架 · VFS 集成                         │
├──────────────────────────────────────────────────────────────────┤
│                       arch（架构实现）                           │
│         实现 general trait · 汇编引导 · 页表激活                 │
└──────────────────────────────────────────────────────────────────┘
```

### 2.1 `kernel` — 策略与集成层

**职责：** 内核入口点、子系统编排、系统调用分发、进程/线程管理、驱动注册、文件系统挂载策略。

**依赖约束：**
- 仅允许依赖 `hal`、`general`、`libs`
- 不得直接依赖 `arch` — 所有架构操作通过 hal 接口
- 不得包含 `asm!` 或直接操作硬件寄存器

### 2.2 `hal` — 统一架构接口层

**职责：** 包装 arch 的具体实现，将 `loongarch64::paging` 和 `riscv64::paging` 等不同实现统一为同一个对外接口。kernel 需要调用架构函数时，只调用 hal，不关心底层是哪个架构。

hal 是 kernel 与 arch 之间唯一的桥梁。它从 arch 获取各架构的实现，并向上层暴露统一 API。

**约束：**
- 可依赖 `arch`（获取架构实现）和 `general`（引用其定义的 trait 类型）
- 不包含直接的硬件操作或 `asm!` — 全部通过 arch 间接执行
- 每个对外接口对应一类硬件能力，内部根据目标架构路由到正确的 arch 实现

### 2.3 `general` — 通用基础设施与标准接口层

**职责：** 定义平台无关的 trait（如 `PagingArch`、平台选择逻辑等），同时提供依赖这些 trait 的泛型算法和基础设施。这些算法对所有架构通用。

**典型内容：**
- 架构 trait 定义（`PagingArch` 等）— hal 和 arch 都引用这些 trait 作为契约
- 泛型页表遍历与映射算法（`page_walk::walk_and_map<T: PagingArch>(...)`）
- DTB / ACPI / UEFI 解析
- 控制台框架与设备枚举
- VFS 管线集成与文件系统
- 平台选择与引导阶段编排

**约束：**
- 可依赖 `libs`（使用算法库）
- 不得包含 ISA 特定指令或 `asm!`
- 不得依赖具体 arch 实现 — 所有架构相关操作通过 trait 间接调用

### 2.4 `arch` — 架构实现层

**职责：** 实现 general 中定义的所有平台 trait（如 `impl PagingArch for LoongArch64Paging`）。包含汇编引导、CSR 操作、异常入口、页表激活、中断控制等硬件操作。同时调用 general 的泛型算法完成页表遍历、设备枚举等工作。

**依赖约束：**
- 必须实现 `general` 中定义的所有平台 trait
- 可依赖 `general`（调用泛型算法并注入架构特定回调）和 `libs`（使用算法库）
- 仅提供机制，不制定策略决策
- 按架构组织：`arch/src/<arch-name>/`

### 2.5 `libs` — 可复用算法库

**职责：** 独立于硬件架构的通用算法与数据结构（分配器、VFS 核心抽象、日志系统、POSIX 错误码等）。以独立 crate 存在于 `libs/` 下。

**约束：**
- 不包含 `asm!` 或 ISA 特定指令
- 平台能力依赖全部通过函数指针或 trait object 注入
- 不依赖 `hal`、`general`、`arch`、`kernel`
- `libs` 内部 crate 之间可按需依赖，但不得形成循环

---

## 3. 依赖规则

### 3.1 依赖方向图

```
kernel ──→ hal         kernel 通过 hal 的统一接口使用架构能力
kernel ──→ general     kernel 依赖 general 的泛型算法和平台 trait 定义
kernel ──→ libs        kernel 依赖 libs 的算法库

hal ──→ arch           hal 包装 arch 的具体实现，向上层暴露统一接口
hal ──→ general        hal 引用 general 定义的平台 trait 作为接口类型

arch ──→ general       arch 实现 general 定义的平台 trait，调用 general 的泛型算法
arch ──→ libs          arch 依赖 libs 的算法库

general ──→ libs       general 的通用算法依赖 libs 的数据结构
libs（仅依赖其他 libs crate）
```

箭头方向为编译期 `Cargo.toml` 依赖方向。

### 3.2 硬性规则

| # | 规则 |
|---|------|
| 1 | `libs` 不依赖 `hal`、`general`、`arch`、`kernel` |
| 2 | `general` 依赖 `libs`，不依赖 `hal`、`arch`、`kernel` |
| 3 | `arch` 依赖 `general`、`libs`，不依赖 `hal`、`kernel` |
| 4 | `hal` 依赖 `arch`、`general`，不依赖 `kernel` |
| 5 | `kernel` 依赖 `hal`、`general`、`libs`，不直接依赖 `arch` |
| 6 | 循环依赖绝对禁止 |

### 3.3 禁止的依赖路径

| 路径 | 原因 |
|------|------|
| `kernel → arch` | kernel 只能通过 hal 间接使用 arch 实现 |
| `general → arch` | general 必须是架构无关的，不得耦合具体架构实现 |
| `general → hal` | general 定义 trait，不消费 hal 的包装 |
| `arch → hal` | arch 实现 general 的 trait，hal 包装 arch，关系不反向 |
| `libs → hal / general / arch / kernel` | libs 通过函数指针注入获取平台能力 |

### 3.4 类型流转

- **general → arch：** general 定义平台 trait（如 `PagingArch`），arch 提供 `impl PagingArch for LoongArch64Paging`
- **general → kernel：** general 提供泛型算法和 trait 定义，kernel 直接使用
- **arch → hal：** arch 暴露架构实现，hal 包装并统一为对外接口
- **hal → kernel：** hal 提供统一接口，kernel 调用 hal 而非 arch
- **libs → 外部：** libs 暴露注入入口（如 `fn bind_allocator(backend: fn_ptr)`），调用方在 general、arch 或 hal 中注册具体后端

---

## 4. 能力注入机制

### 4.1 设计动机

内核需要在不依赖静态链接的前提下，在运行期确定子系统的具体后端实现。原因：

- **引导期需要最小化依赖。** 在正式分配器、页表、控制台就绪之前，系统仍然需要日志输出和内存分配。
- **支持多架构。** 同一个 libs crate 在不同 ISA 上需要不同的地址转换公式、时间戳源、临界区语义。
- **引导期与运行时后端不同。** 引导期使用线性分配器、直接 MMU 映射、裸 UART 输出；运行时替换为伙伴系统、页表映射、完整控制台。

### 4.2 核心模式

```
  后端实现               全局原子指针              调用方
  (arch/general)        AtomicPtr<dyn Trait>        (libs)

      │ 注册（Release store）   │                      │
      ├────────────────────────→│                      │
      │                         │ 查询（Acquire load） │
      │                         │←─────────────────────┤
      │                         ├─────────────────────→│
      │                         │返回 Option<&dyn Trait>
```

**注册端（单核执行）：**

```rust
static BACKEND: AtomicPtr<dyn SomeTrait> = AtomicPtr::new(null_mut());

pub fn bind(backend: &'static dyn SomeTrait) {
    BACKEND.store(backend as *const _ as *mut _, Ordering::Release);
}
```

**使用端（任意上下文）：**

```rust
pub fn do_something() -> Option<Output> {
    // Acquire 保证在见到非空指针时，Release 之前的所有写入均已可见
    let ptr = BACKEND.load(Ordering::Acquire);
    // Safety: ptr 非空时指向 'static 对象，注册后永不移动/释放
    Some(unsafe { (*ptr.as_ref()?).method() })
}
```

### 4.3 注入能力清单

所有跨越「引导期→运行时」边界的能力均通过注入管理：

| 能力 | 引导期后端 | 运行时后端 |
|------|-----------|-----------|
| 物理内存分配 | 线性分配器（boot allocator） | 伙伴系统 |
| 虚拟内存管理 | MMU 直接映射窗口 | 页表精细映射 + 虚拟地址空间管理 |
| 内核堆与全局分配器 | 禁用 / 未激活 | kheap + slab + managed heap |
| 日志输出 | 直接写硬件串口寄存器 | console-backed sink |
| 时间戳 | 架构计数器（频率未校准） | 架构计数器（频率已校准） |
| 控制台 | 未注册 | 设备驱动后端 |
| 地址转换 (phys↔virt) | 直接映射窗口公式 | 页表遍历 |
| 临界区保护 | 关中断/恢复中断 | per-CPU 中断控制 |

### 4.4 注入安全准则

- 后端指针注册后视为不可变。允许替换的能力必须通过 `compare_exchange` 确保唯一生产者。
- 每次读取必须处理 null 情况（尚未注册），返回 `Option` 或 `Result`。
- 注册使用 `Release` 存储，查询使用 `Acquire` 加载，保证 happens-before 关系。
- 注入点必须声明后端 trait 的来源（来自 general 的平台 trait），禁止注入无关接口。

---

## 5. 不安全代码治理

### 5.1 分层 unsafe 许可

| 层 | unsafe 许可范围 |
|----|---------------|
| `arch` | 合法使用 `asm!` 和 CSR 操作。所有 unsafe 块必须有安全注释。 |
| `libs` | 已验证有效性的裸指针解引用、受 `AtomicBool` 保护的 `static mut` 访问、全局分配器实现。 |
| `general` | 能力注册中的 trait object→裸指针转换、已验证的 FFI 指针解引用。 |
| `hal` | 转发 arch 的 unsafe 接口，自身不引入新的 unsafe。 |
| `kernel` | 目标为零 unsafe。仅允许 `extern "C"` 函数声明。 |

### 5.2 安全注释规范

每个 `unsafe` 块必须包含：

```rust
// Safety:
// - <操作为什么是 unsafe>
// - <调用方必须满足的前置条件>
// - <当前上下文为什么满足这些条件>
unsafe { ... }
```

### 5.3 禁止模式

| # | 禁止模式 |
|---|---------|
| 1 | 在 `static mut` 上创建引用 — 必须使用 `addr_of!`/`addr_of_mut!` + `ptr::read`/`ptr::write` |
| 2 | 未验证即解引用外部传入的裸指针 — 必须先做 null/对齐/范围检查 |
| 3 | 在 `libs` 或 `general` 中使用内联汇编 — 所有 `asm!` 仅在 `arch` 中使用 |
| 4 | 在中断上下文中以非原子操作访问全局可变状态 |
| 5 | `transmute` 到包含未定义位模式的类型（bool、枚举、引用） |

---

## 6. 并发模型

### 6.1 保护机制

所有共享状态通过以下机制之一保护：

- **原子类型**（`AtomicBool`、`AtomicPtr`、`AtomicUsize`）— 能力注册、标志位、计数器
- **per-CPU 数据结构** — 调度器状态、分配器缓存、中断统计
- **锁** — 设备驱动、文件系统元数据、进程表等复杂共享状态

### 6.2 SMP 语义分层

| 层 | SMP 职责 |
|----|---------|
| `general` | 定义 CPU 拓扑查询、IPI 发送、原子屏障的 trait 契约 |
| `arch` | 实现 fence/barrier/IPI/local interrupt control |
| `hal` | 统一暴露 arch 的 SMP 原语 |
| `kernel` | 决定锁粒度、抢占模型、RCU/epoch 策略、跨 CPU 调度 |
| `libs` | 内部使用原子操作；不引入锁策略 — 仅提供数据结构，同步方式由上层决定 |

### 6.3 禁止模式

- 禁止在 `libs` 中使用 ISA 特定 fence 指令 — 必须通过注入的函数指针
- 禁止在能力层提供带策略语义的 helper（如 `with_global_kernel_lock`）
- 禁止在 `arch` 之外使用架构特定内存屏障

---
