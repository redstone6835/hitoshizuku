#import "../config.typ": project-name
#import "../styles/diagram.typ": flow-arrow, flow-node, layer-card
#import "../styles/figure.typ": continued-table, figure-caption, pseudo-sample
#import "../styles/tokens.typ": handoff-fill, soft-fill, stable-fill, warm-fill

= 第一章 分层结构与启动控制流

我们在设计内核启动路径时面对的首要问题，是同一套内核需要同时运行在 LoongArch64 架构和 RISC-V64 架构上。两种平台的差异并不只体现在指令编码和寄存器名称上。固件交接方式不同，早期地址空间不同，内存描述来源不同，设备发现路径也不同。RISC-V64 架构的 QEMU 虚拟机直启路径由启动寄存器传入 DTB（Device Tree Blob，设备树二进制）物理地址。LoongArch64 架构的 ABI（Application Binary Interface，应用二进制接口）则传入 EFI（Extensible Firmware Interface，可扩展固件接口）标记、命令行地址和 EFI 系统表地址。当前 LoongArch64 架构加载器以 EFI 系统表为固件发现入口，优先从 EFI 配置表选择 ACPI（Advanced Configuration and Power Interface，高级配置与电源接口），缺省回退到 DTB。若平台无关层直接理解这些细节，启动代码会很快形成大量分支。若架构层只交出少量原始指针，后续子系统又无法判断这些指针的生命周期和有效性。

我们最终采用的边界是启动上下文（`StartContext`）。架构相关层负责把固件和处理器现场整理成稳定的启动上下文。平台无关层只消费这个上下文，并按照统一顺序激活内存、设备、文件系统、控制台以及调度器。这个边界让启动过程形成清晰的信息流。底层代码吸收硬件差异。交接对象保留必要能力。高层代码围绕稳定数据和回调组织系统策略。

本章讨论这一结构的设计理由和关键控制流。第一部分说明分层结构如何约束依赖方向。第二部分说明从固件入口到 `main` 函数的启动链路。第三部分分析架构加载器如何构造启动上下文。第四部分说明平台无关启动阶段如何根据 DTB 或 ACPI 激活运行期子系统。最后一部分总结本章启动设计中的工程创新。

== 1.1 问题背景与设计目标

跨架构启动代码的难点不在于写出两个入口。难点在于确定哪些差异应当留在架构层，哪些信息应当向上交给通用内核。启动早期没有完整堆分配器，没有统一设备模型，也没有稳定的日志输出。此时任何复杂依赖都可能放大故障面。启动后期又必须准备运行期系统。物理页分配器需要可用内存段。设备子系统需要 MMIO（Memory-Mapped Input/Output，内存映射输入输出）地址转换能力。控制台需要固件中的串口描述。调度器需要 VFS（Virtual File System，虚拟文件系统）根上下文和标准文件描述符。每一步都依赖前一步已经产出的稳定结果。

我们把启动路径的设计目标整理为四类。

#continued-table(
  "1-1",
  [启动路径的设计目标],
  (1.1fr, 2.2fr, 2.2fr),
  (
    table.cell(fill: soft-fill)[#text(weight: "bold")[目标]],
    table.cell(fill: soft-fill)[#text(weight: "bold")[含义]],
    table.cell(fill: soft-fill)[#text(weight: "bold")[工程约束]],
  ),
  (
    table.cell(fill: warm-fill)[依赖单向],
    table.cell(fill: warm-fill)[架构相关层向上交付数据和能力，平台无关层不反向读取架构私有状态。],
    table.cell(fill: warm-fill)[`kernel` 只能通过 `hal` 与 `general` 消费稳定接口。],
    table.cell(fill: soft-fill)[交接稳定],
    table.cell(fill: soft-fill)[固件表、命令行和内存图在移交前完成生命周期稳定化。],
    table.cell(fill: soft-fill)[平台无关层不直接依赖 EFI 服务或启动寄存器。],
    table.cell(fill: handoff-fill)[能力显式],
    table.cell(fill: handoff-fill)[地址转换、堆映射和页表安装以回调形式交付。],
    table.cell(fill: handoff-fill)[高层代码不嵌入早期直映窗口和 MMIO 映射常量。],
    table.cell(fill: stable-fill)[顺序可验证],
    table.cell(fill: stable-fill)[每个启动阶段都由前置条件决定，失败路径可以定位到具体边界。],
    table.cell(fill: stable-fill)[危险操作前先建立电源控制和日志能力。],
  ),
  kind: "表",
  continuation-kind: "续表",
  align: (left, left, left),
)

这四个目标共同决定了启动层的形态。我们不让平台无关层直接访问启动协议原始结构，也不让架构层继续参与运行期策略。架构层的任务是构造一个足够完整的交接对象。平台无关层的任务是验证这个对象，并把其中的数据转换为运行期状态。这个分界点越清晰，后续移植新平台时需要触碰的代码范围就越小。

== 1.2 项目分层结构

项目整体由五个层级组成。`libs` 模块提供可以复用的基础库和子系统部件。`general` 模块定义平台无关的数据结构、特征接口、固件视图以及启动上下文。`arch` 模块保存与具体指令集相关的入口、页表、异常处理和早期映射。`hal` 模块把不同架构的实现收敛成统一接口。`kernel` 模块负责策略编排和子系统集成。

#figure(caption: figure-caption("图", "1-1", [项目分层结构]))[
  #layer-card("基础库层", [可复用基础设施。包含分配器、文件系统、网络、调度以及设备相关库，不依赖上层启动策略。], fill: soft-fill)
  #align(center)[#text(fill: rgb("#2d5f73"))[↑]]
  #layer-card("平台无关基础层", [定义启动上下文、固件视图、设备模型、内存抽象和跨架构特征接口。], fill: soft-fill)
  #align(center)[#text(fill: rgb("#2d5f73"))[↑]]
  #layer-card("架构实现层", [处理汇编入口、控制寄存器、页表机制、异常入口、早期地址映射和固件快照。], fill: warm-fill)
  #align(center)[#text(fill: rgb("#2d5f73"))[↑]]
  #layer-card("统一架构接口层", [封装 `arch` 模块的具体实现，为 `kernel` 模块提供稳定函数表和运行期钩子。], fill: handoff-fill)
  #align(center)[#text(fill: rgb("#2d5f73"))[↑]]
  #layer-card("策略与集成层", [消费 `hal` 模块与 `general` 模块，完成内存、设备、VFS、控制台、调度器以及用户态入口的编排。], fill: stable-fill)
]

这套分层的关键不在于目录名称，而在于依赖方向。`general` 模块可以定义启动上下文，但它不读取某个架构的控制寄存器。`arch` 模块可以填充启动上下文，但它不决定根文件系统该如何选择。`hal` 模块可以注册调度器所需的架构钩子，但它不持有调度策略。`kernel` 模块可以启动设备和文件系统，但它不直接写入页表项或解释启动 ABI 寄存器。

这种单向依赖给启动路径带来两个直接收益。第一，平台差异被限制在 `arch` 模块和少量 `hal` 适配中。第二，平台无关层可以用相同的代码处理 DTB 路径和 ACPI 路径产出的结果。我们在调试启动问题时，也能先判断错误发生在交接之前还是交接之后。这个判断往往比单纯查看日志更重要，因为它直接决定应当检查汇编入口、固件快照，还是检查通用子系统初始化顺序。

== 1.3 启动链路概览

内核启动是一个逐步降低不确定性的过程。固件交出控制权时，处理器只满足很小的执行条件。到 `main` 函数执行时，内核已经有运行期分配器、设备模型、VFS 上下文、控制台和调度器入口。中间的每个阶段都需要把上一阶段的原始状态转化成下一阶段可以消费的对象。

#figure(caption: figure-caption("图", "1-2", [启动链路概览]))[
  #flow-node([固件或虚拟机进入架构入口], fill: warm-fill)
  #flow-arrow()
  #flow-node([早期环境建立：地址映射、异常入口、临时栈和启动参数保存], fill: warm-fill)
  #flow-arrow()
  #flow-node([架构加载器：固件快照、启动分配器、平台能力收集], fill: handoff-fill)
  #flow-arrow()
  #flow-node([启动上下文交接：数据和回调的唯一边界], fill: handoff-fill)
  #flow-arrow()
  #flow-node([平台无关启动：DTB 或 ACPI 分派，激活运行期子系统], fill: soft-fill)
  #flow-arrow()
  #flow-node([`main` 函数：调度器、系统调用和用户态初始进程], fill: stable-fill)
]

#continued-table(
  "1-2",
  [启动阶段职责划分],
  (1.2fr, 2.3fr, 2.1fr),
  (
    table.cell(fill: soft-fill)[#text(weight: "bold")[阶段]],
    table.cell(fill: soft-fill)[#text(weight: "bold")[主要职责]],
    table.cell(fill: soft-fill)[#text(weight: "bold")[交付结果]],
  ),
  (
    table.cell(fill: warm-fill)[早期入口],
    table.cell(fill: warm-fill)[建立最小地址空间，安装异常入口，切换临时栈，清零 BSS 后保存启动参数。],
    table.cell(fill: warm-fill)[可执行 Rust 代码的早期环境。],
    table.cell(fill: handoff-fill)[架构加载器],
    table.cell(fill: handoff-fill)[初始化早期日志和启动分配器，复制固件表，选择 DTB 或 ACPI，准备地址转换和分配器回调。],
    table.cell(fill: handoff-fill)[启动上下文。],
    table.cell(fill: soft-fill)[启动初始化],
    table.cell(fill: soft-fill)[校验上下文，根据固件来源进入 DTB 或 ACPI 路径，激活内存、设备、VFS 和控制台。],
    table.cell(fill: soft-fill)[运行期内核环境。],
    table.cell(fill: stable-fill)[主入口],
    table.cell(fill: stable-fill)[注册运行期钩子，初始化调度器，安装系统调用表，启动用户态初始进程。],
    table.cell(fill: stable-fill)[可调度的用户态系统。],
  ),
  kind: "表",
  continuation-kind: "续表",
  align: (left, left, left),
)

两个边界在这条链路中尤其重要。第一个边界是早期入口到架构加载器。这里要求代码已经有临时栈，BSS（Block Started by Symbol，未初始化静态区）状态也已知。第二个边界是架构加载器到 `__kernel_start_init` 入口。这里要求固件视图已经稳定，地址转换函数已经可用，内核镜像范围和可选内存图已经填入上下文。后续平台无关层只依赖这些结果。

== 1.4 架构相关启动阶段

架构相关启动阶段的核心职责，是把固件交来的初始现场转换为高级语言可以处理的状态。这个阶段直接面对控制寄存器、页表、异常入口、启动 ABI 和固件服务。我们把它拆成早期入口、预启动初始化以及架构加载器三段。

=== 1.4.1 早期入口与预启动初始化

早期入口不能假设运行期基础设施已经存在。进入 `_start` 入口时，栈可能尚未切好，BSS 中可能存在旧值，页表状态也可能由固件或虚拟机决定。我们因此把入口动作限制在少数硬件相关操作上。先建立最小地址映射，再安装异常入口，然后跳到带临时栈的虚拟地址环境。随后 `pre_boot_init` 函数清零 BSS，并把启动参数写入静态存储。

#pseudo-sample("1-1", [早期入口与预启动初始化], kind: "代码")[
  ```rust
  fn arch_entry() -> ! {
      setup_minimal_mapping();
      install_early_exception_entry();
      clear_unknown_control_state();
      jump_to_virtualized_entry()
  }

  fn virtualized_entry(boot_args: BootArgs) -> ! {
      clear_return_boundary();
      switch_to_temporary_stack();
      prepare_arch_runtime_for_rust();
      pre_boot_init(boot_args);
      jump_to_arch_loader(boot_args)
  }

  fn pre_boot_init(boot_args: BootArgs) {
      clear_bss();
      save_boot_arguments(boot_args);
      init_boot_cpu_local_if_needed();
  }
  ```
]

这里最容易出错的是顺序。BSS 必须先清零，再写入启动参数。LoongArch64 架构会把 EFI 标记、命令行地址和系统表地址保存在静态原子变量中。RISC-V64 架构会保存启动硬件线程编号和 DTB 地址，并初始化启动硬件线程的每硬件线程数据。如果先保存参数再清零 BSS，刚写入的参数会被覆盖。这个错误在后续阶段表现为固件缺失，很难从故障点直接看出原因。

LoongArch64 架构入口还有一个实际约束。当前编译配置下，LLVM 编译器可能在发布模式生成 LSX 向量指令。我们在进入 Rust 语言代码前设置 `EUEN.FPE` 和 `EUEN.SXE` 位，确保早期 Rust 语言路径不会因为浮点或 LSX 状态不可用而陷入异常。这个细节说明早期环境并非只服务于汇编入口，它还要满足编译器生成代码的最低运行条件。

=== 1.4.2 架构加载器

架构加载器位于架构相关启动阶段的末端。它已经可以执行较完整的 Rust 语言代码，但仍处在平台私有语境中。它的任务是建立早期诊断能力，复制固件数据，准备启动内存信息，并构造启动上下文。

#pseudo-sample("1-2", [架构加载器核心流程], kind: "代码")[
  ```rust
  fn kernel_arch_loader(raw: RawBootState) -> ! {
      install_exception_handlers();
      init_time_or_timer_source();
      bind_early_log_sink();
      init_boot_allocator();

      let firmware = snapshot_and_select_firmware(raw);
      let boot_map = snapshot_boot_memory_map(raw);
      let kernel_image = locate_kernel_image_range();

      let context = StartContext {
          boot: build_boot_info(raw),
          firmware,
          memory: StartMemory { kernel_image, boot_map },
          address: build_address_ops(),
          allocator: build_allocator_ops_if_supported(),
      };

      validate_context_before_handoff(&context);
      jump_to_kernel_start_init(&context)
  }
  ```
]

这一流程的顺序来自数据依赖。异常处理要早于复杂解析，否则早期错误会缺少可控出口。时间源或周期定时器要早于依赖时间戳的日志系统。启动分配器要在固件复制前可用，因为 DTB 和 ACPI 表都需要稳定的保存空间。固件选择要在启动上下文构造前完成，因为上下文中只能出现一种固件来源。

RISC-V64 架构的直启路径相对直接。启动寄存器提供 DTB 物理地址，加载器复制 DTB，记录 `StartBootProtocol::Direct` 枚举变体，并将启动内存图设为 `None`。后续 DTB 解析器从 `/memory` 节点取得 RAM（Random Access Memory，随机访问内存）信息。LoongArch64 架构的路径更复杂。加载器保存命令行，并以 EFI 系统表为固件发现入口。若配置表中存在 RSDP（Root System Description Pointer，根系统描述指针）且 EFI 内存映射可用，它会选择 ACPI 并复制 ACPI 表；否则在存在 FDT 时复制 DTB 视图。启动协议根据入口传入的 EFI 标记记录为 `Efi` 或 `Direct` 枚举变体，但当前实现仍要求能取得可用的 EFI 系统表来发现 ACPI 或 FDT。当前 ACPI 路径要求存在可用的启动内存映射，因为 ACPI 本身描述平台结构，却不提供可直接交给分配器的普通 RAM 列表。

这里还有一个生命周期约束。固件表指针不能简单透传给平台无关层。EFI 启动服务退出后，固件服务内存可能失效。DTB 所在物理页也可能在内存接管后被重新分配。加载器因此必须先复制需要的固件内容，再把稳定视图放入上下文。对于 DTB，这通常是一个连续二进制块的复制。对于 ACPI，加载器需要保存 RSDP，并维护 ACPI 表物理地址到复制后虚拟地址的映射。

== 1.5 启动上下文

启动上下文是架构层到平台无关层的唯一交接对象。它不是运行期数据库，也不是全局配置中心。它只在启动交接期间有效，负责描述已经稳定化的数据和必须由架构提供的能力。

#pseudo-sample("1-3", [启动上下文核心结构], kind: "代码")[
  ```rust
  struct StartContext {
      boot: StartBootInfo,
      firmware: StartFirmware,
      memory: StartMemory,
      address: StartAddressOps,
      allocator: Option<StartAllocatorOps>,
  }

  struct StartBootInfo {
      architecture: StartArchitecture,
      protocol: StartBootProtocol,
      boot_cpu_id: usize,
      command_line: Option<&'static [u8]>,
  }

  enum StartFirmware {
      Dtb(Dtb<'static>),
      Acpi(StartAcpiTables),
  }

  struct StartMemory {
      kernel_image: StartPhysRange,
      boot_map: StartMemoryMap,
  }

  enum StartMemoryMap {
      None,
      Regions(&'static [StartMemoryRegion]),
  }

  struct StartAddressOps {
      phys_to_virt: fn(usize) -> usize,
      virt_to_phys: fn(usize) -> usize,
      device_mmio_to_virt: fn(usize) -> usize,
  }

  struct StartAllocatorOps {
      kernel_heap_region: fn() -> VirtRange,
      map_kernel_heap_range: fn(usize, usize, usize, PagePolicy) -> bool,
      unmap_kernel_heap_range: fn(usize, usize) -> bool,
      init_kernel_page_table: fn(),
  }
  ```
]

这个结构的设计原则是传递已整理的事实和能力。`boot` 字段说明当前架构、启动协议、启动处理器标识和可选命令行。`firmware` 字段明确本次启动信任 DTB 还是 ACPI。`memory` 字段保存内核镜像占用范围和可选启动内存图。`address` 字段提供普通物理地址、内核虚拟地址和设备 MMIO 地址之间的转换。`allocator` 字段提供内核堆区域、堆映射、解映射以及页表安装回调。

启动固件视图使用枚举而不是多个可选字段。这个选择可以把不变量前移。上下文中只能有一个固件来源。`__kernel_start_init` 入口只需要读取 `firmware_source()` 方法，随后分派到 DTB 或 ACPI 路径。平台无关层不会再次比较 EFI 表中是否还存在另一个候选固件，也不会在解析失败后临时切换来源。启动日志和错误报告因此具有明确语义。

启动内存图的设计也体现了边界控制。DTB 直启系统常常在 DTB 的 `/memory` 节点中描述 RAM，此时独立启动内存图可以为空。EFI 或其它启动协议可能额外提供内存映射，此时加载器把协议私有描述符归一化为启动内存区域。平台无关层只看区域类型是否能在交接后使用，不理解 EFI 原始属性布局。ACPI 路径在上下文校验中要求 `boot_map` 字段至少包含可用区域，避免在没有普通 RAM 输入的情况下继续初始化分配器。

地址转换回调是上下文中最重要的能力接口之一。LoongArch64 可以用 DMW 窗口把 MMIO 物理地址转换为非缓存虚拟地址。RISC-V64 可以把设备地址映射到专门的 MMIO 虚拟窗口。平台无关层只调用 `device_mmio_to_virt` 回调，不关心具体窗口常量。普通 RAM 的 `phys_to_virt` 回调与 `virt_to_phys` 回调也遵循同样原则。这样一来，分配器、固件解析器和设备初始化代码可以共享同一套地址能力。

`StartContext::validate` 方法是交接点上的防线。它检查架构标识、启动协议、内核镜像范围和内存图条目。对于 ACPI，它还检查 RSDP、复制表映射和可用启动内存段。我们把这些校验放在进入通用启动流程之前，原因很直接。如果上下文不满足基本不变量，后续错误会扩散到分配器、VFS 或设备注册阶段。尽早拒绝错误上下文，可以把问题定位在架构加载器一侧。

== 1.6 平台无关启动阶段

平台无关启动阶段从 `__kernel_start_init` 入口开始。这个入口接收启动上下文指针，先把空指针和基本不变量排除掉，再根据固件来源进入 `dtb::kernel_start_init` 函数或 `acpi::kernel_start_init` 函数。两个路径解析的固件格式不同，但目标一致。它们都要准备内存分配器，安装电源控制，建立 VFS 基础设施，激活设备子系统，注册控制台，并把必要的启动期 VFS 部件交给调度器。

#pseudo-sample("1-4", [平台无关启动初始化], kind: "代码")[
  ```rust
  unsafe extern "C" fn __kernel_start_init(ctx: *const StartContext) -> ! {
      let context = checked_ref(ctx);
      context.validate().expect("invalid StartContext");

      match context.firmware_source() {
          StartFirmwareSource::Dtb => dtb_kernel_start_init(context),
          StartFirmwareSource::Acpi => acpi_kernel_start_init(context),
      }

      main()
  }
  ```
]

DTB 路径先解析整棵设备树，建立节点索引，处理 `aliases`、`phandle`、`status`、`reg` 和 `ranges` 等设备树属性，然后产出 CPU、内存、保留区、串口、平台设备、PCIe 主桥和电源控制等标准描述。若启动上下文同时提供了独立内存图，DTB 解析出的内存段会与可用启动内存段做交叉过滤。这样可以避免固件描述之间不一致时把不可用页交给物理分配器。

ACPI 路径先通过复制后的 RSDP 和表映射建立 ACPI 表视图，再从 MADT（Multiple APIC Description Table，多 APIC 描述表）取得 CPU 数量，从 SPCR（Serial Port Console Redirection Table，串口控制台重定向表）和 ACPI 命名空间发现串口设备，从命名空间发现 virtio-mmio 设备，并解析电源控制寄存器。ACPI 路径的普通 RAM 输入来自启动内存区域列表。这与 DTB 路径不同，但在进入分配器之前，两者都会整理出同一种内存段列表。

内存分配器的激活顺序在两个路径中保持一致。首先绑定 `phys_to_virt` 回调与 `virt_to_phys` 回调。随后保留内核镜像范围。DTB 路径还会保留可能存在的外部 initramfs 初始内存文件系统范围。接着初始化物理页分配器。若架构提供启动分配器接口，再绑定内核堆映射回调，安装内核页表，初始化虚拟内存管理器，最后启用内核堆、Slab 分配器和全局分配器。

这个顺序不能随意交换。物理页分配器必须早于页表页申请。页表初始化必须早于内核堆映射的稳定使用。Slab 分配器依赖后备页和堆能力。全局分配器要放在最后，因为一旦切换完成，后续设备对象、VFS 索引节点和调度器对象都会使用运行期分配路径。我们在这一步之前保持启动分配器的职责有限，可以减少不可释放早期分配对系统长期内存状态的影响。

电源控制要早于高风险初始化。分配器和页表初始化失败时，内核可能需要关机或重启。如果此时还没有安装平台电源控制，错误路径只能停机等待外部终止。DTB 和 ACPI 路径都会在激活分配器之前安装 `general::firmware::power` 电源控制模块，使内核 `panic` 异常处理和正常关机都能调用统一接口。

设备与 VFS 的启动顺序体现了第三章讨论的设备模型边界。我们先注册 tmpfs 临时文件系统、devtmpfs 设备文件系统、procfs 进程文件系统和 sysfs 系统文件系统驱动，以及标准设备号策略和设备文件投影器，再创建 devtmpfs 设备文件系统超级块，并把设备能力投影机制连接到设备子系统。DTB 路径随后登记平台设备并扫描 PCIe 主桥。ACPI 路径当前登记从 ACPI 发现的串口和 virtio-mmio 设备。驱动探测成功后发布设备能力，devtmpfs 设备文件系统可以接收投影事件。DTB 路径在根文件系统确定后把这棵已经填充的 devtmpfs 设备文件系统挂载到 `/dev`，ACPI 路径则在 tmpfs 临时文件系统根建立后先挂载 `/dev`，再继续设备登记。

根文件系统的选择在 DTB 路径中具有优先级。外部 initramfs 初始内存文件系统或内建 initramfs 初始内存文件系统优先。若不存在 initramfs 初始内存文件系统，再尝试从已经注册的块设备中挂载根盘。ACPI 路径当前采用 tmpfs 临时文件系统作为启动根。两条路径都会挂载 devtmpfs 设备文件系统到 `/dev`，并挂载 `/dev/shm` 与 `/sys`。需要注意的是，procfs 进程文件系统在核心文件系统阶段完成注册，但当前启动路径没有在这里自动挂载 `/proc`。两条路径最终都会创建 VFS 上下文、挂载命名空间、根目录和凭据，并通过 `sched::stash_boot_vfs_parts` 函数交给调度层。调度器随后给 PID 1 安装 VFS 上下文和文件描述符表。

控制台切换放在启动初始化后段。早期日志输出在架构加载器阶段就可以工作，运行期日志输出则依赖已经注册的字符设备。DTB 路径优先读取命令行中的 `console` 参数，然后回退到 `stdout-path` 属性。ACPI 路径优先读取命令行，再回退到 SPCR 指定或命名空间中发现的串口。找到控制台后，系统会注册控制台，绑定 `/dev/console`，并把日志输出端切换到正式字符设备。这个顺序可以让早期日志尽可能长地保留，同时在设备模型可用后切换到统一输出路径。

== 1.7 主入口与用户态交接

`main` 函数是启动阶段到运行阶段的交接点。执行到这里时，启动上下文已经被消费完毕。分配器、设备、VFS 和控制台已经处于运行期状态。`main` 函数不再接收启动上下文，也不重新解析固件。它只登记运行期钩子，启动调度器，并把 PID 1 送入用户态。

#pseudo-sample("1-5", [主入口控制流], kind: "代码")[
  ```rust
  fn main() -> ! {
      register_vdso_tick_hook();
      register_net_poll_hook();

      let init = sched_boot_init();
      register_tty_poller();

      run_configured_startup_hooks();
      set_runtime_log_level();
      start_init_process(&init)
  }
  ```
]

`sched::boot_init` 函数的注册顺序同样由依赖决定。它先通过 `hal` 模块注入上下文切换、时间、陷阱帧、内存切换和系统调用相关钩子。随后注册 `fork` 系统调用和 `clone` 系统调用需要的扩展复制钩子，再注册退出钩子和退出前钩子。退出前钩子要早于地址空间释放执行，因为 robust futex 清理机制和 clear-child-tid 机制需要在用户虚拟地址空间仍可访问时处理。接着注入用户进程镜像操作表、虚拟内存切换钩子和任务 CPU 状态发布钩子，最后创建 init 任务。

创建 init 任务后，调度层会取出启动阶段暂存的 VFS 部件。它构造 VFS 上下文和文件描述符表，安装标准输入、标准输出和标准错误，再把这些对象挂到 init 任务扩展槽中。之后为 CPU 0 创建空闲内核线程，并注册完整系统调用表。这个顺序保证用户态第一次进入内核时，文件系统、文件描述符和系统调用分发都已经就绪。

`start_init_process` 函数负责把 PID 1 替换为真实用户态镜像。它依次尝试 `/init`、`/sbin/init` 和 `/bin/init`。第一个可加载的用户镜像会获得用户地址空间、入口地址和初始栈。内核随后构造用户态陷阱帧，并通过架构相关返回路径进入用户态。此后系统进入常规调度阶段，启动上下文不再参与运行期控制流。

== 1.8 工程设计总结

本章围绕分层结构和启动控制流，说明了我们如何把架构差异、固件差异和运行期子系统初始化组织在同一条单向链路中。启动路径的核心落在一组边界的配合上，而不只是某一个入口函数。早期入口保证代码能够安全进入 Rust 语言环境。架构加载器稳定化固件信息，并构造启动上下文。平台无关层验证上下文，解析 DTB 或 ACPI，激活内存和设备。主入口完成调度器、系统调用和用户态 init 进程的交接。

工程设计具备以下创新。

第一是以启动上下文为中心建立一次性交接边界。回顾启动路径的早期设计，一个主要分歧在于平台无关层应当如何获得硬件信息。较直接的方案是让 `kernel` 模块在需要时调用 `arch` 模块查询函数。这个方案实现成本较低，但会把启动寄存器、固件表指针和早期映射状态长期暴露给高层。另一个方案是把所有固件信息提前解析成运行期全局变量。这个方向看似稳定，却容易让早期生命周期和运行期生命周期混在一起。我们最终选择启动上下文，让架构加载器在一个明确时刻完成数据收集、稳定化和能力封装。平台无关层只消费这个上下文，不反向读取架构私有状态。这个设计把启动交接变成了可验证的单次事务，也让支持新架构时的工作集中在上下文构造侧。

第二是把固件选择做成显式枚举和前置校验。DTB 与 ACPI 都能描述平台，但它们提供的信息范围不同，生命周期也不同。若上下文使用多个可选字段，平台无关层就必须在运行期维持组合不变量，例如 ACPI 表存在但内存图缺失，或者 DTB 和 ACPI 同时出现。我们把固件来源收敛为 `StartFirmware::Dtb` 枚举变体与 `StartFirmware::Acpi` 枚举变体。上下文中只能保存一种结果。`validate` 方法会在进入通用启动前检查该结果是否满足最低条件。ACPI 必须有复制后的 RSDP、表映射和可用内存段。DTB 路径则可以接受独立启动内存图为空，并由 `/memory` 节点提供 RAM 信息。这种设计减少了后续分派中的猜测，也让错误更早暴露在交接点。

第三是把地址转换和分配器能力作为显式回调交付。不同架构的早期地址空间差异很大。LoongArch64 可以依赖 DMW 形成直接窗口。RISC-V64 可以把 MMIO 放入专门虚拟窗口。若平台无关层直接写入这些常量，后续每增加一个架构都会扩散修改范围。我们在启动地址转换接口中明确区分普通 RAM 的 `phys_to_virt` 回调、内核地址反查的 `virt_to_phys` 回调，以及设备寄存器访问所需的 `device_mmio_to_virt` 回调。分配器需要的堆区域、堆映射和页表安装能力也通过启动分配器接口提供。这样，内存管理和设备初始化代码只依赖能力接口，具体映射策略仍由架构层掌握。

第四是把启动顺序设计成可解释的依赖链。启动初始化中很多顺序看似只是工程习惯，实际都对应明确约束。BSS 必须先清零再保存启动参数。电源控制必须早于分配器和页表等高风险操作。物理页分配器必须早于虚拟内存管理和 Slab 分配器。devtmpfs 设备文件系统需要早于设备扫描接收投影事件。控制台切换要晚于字符设备注册，以便早期日志保留尽可能久。我们把这些顺序固定在启动控制流中，并通过注释、伪代码和上下文校验说明原因。这样做的价值在调试阶段尤为明显。一次启动失败可以沿依赖链向前定位，而不是在多个全局状态之间反复猜测。

第五是让运行期交接保持最小状态继承。`main` 函数不再携带启动上下文，也不重新解释固件。它只登记运行期钩子，启动调度器，并让已有的 VFS、文件描述符和控制台状态进入 PID 1。调度层通过 `stash_boot_vfs_parts` 函数接收启动阶段准备好的 VFS 部件，再在 `boot_init` 函数中安装到 init 任务上。系统调用表在调度器就绪后注册，TTY 轮询器在空闲线程可用后启动。这个边界使启动阶段和运行阶段自然分离。启动代码完成一次性资源转换，运行期代码只面对已经稳定的对象。

从整体上看，第一章的分层与启动设计为后续章节提供了共同前提。第二章讨论的内存管理依赖启动阶段提供的可用物理内存和地址转换能力。第三章讨论的设备模型依赖启动阶段提供的固件解析结果、MMIO 转换和 devtmpfs 设备文件系统投影载体。后续调度、系统调用和用户态加载也都建立在 `main` 之后的运行期状态之上。我们把这些依赖收束在清晰的启动链路中，是为了让每个子系统都能在自己的边界内演化，同时保持整个内核的启动行为可推理、可验证和可移植。
