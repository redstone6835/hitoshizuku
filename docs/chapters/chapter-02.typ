#import "../config.typ": project-name
#import "../styles/diagram.typ": flow-arrow, flow-node, layer-card
#import "../styles/figure.typ": continued-table, figure-caption, pseudo-sample
#import "../styles/tokens.typ": handoff-fill, soft-fill, stable-fill, warm-fill

= 第二章 内存管理子系统

在第一章中，我们把启动链路划分为架构相关阶段和平台无关阶段，并说明了启动上下文如何交付固件视图、内存图、地址转换函数和分配器回调。本章讨论这些信息如何被进一步转化为运行期内存管理能力。内存管理是内核最早激活的核心子系统之一。设备对象、VFS 索引节点、任务结构、页表页和用户地址空间都依赖它。若这一层的边界不清晰，后续子系统的错误会被误判为设备错误、文件系统错误或调度错误，实际根因却可能是分配路径已经破坏了内存所有权。

我们面对的内存问题并非单一问题。启动阶段需要一个依赖极少的临时分配器。运行期需要管理不连续的物理页段。内核堆需要把物理页映射到可用虚拟地址。小对象需要低延迟复用。大对象需要页级对齐和完整回滚。用户态进程还需要独立的地址空间、缺页处理、共享映射和写时复制。把这些需求压进一个通用分配器会让接口变得含混，也会让锁顺序难以验证。

== 2.1 设计目标与约束

内存管理子系统需要同时满足四类约束。第一类约束来自启动顺序。早期代码不能依赖运行期堆，运行期分配器又需要早期分配器提供元数据空间。第二类约束来自硬件内存布局。可用 RAM 可能分散在多个物理段，中间夹有内核镜像、固件保留区、设备 MMIO 和 initramfs 初始内存文件系统。第三类约束来自分配模式。8 字节对象和 8 MiB 对象不应走完全相同的路径。第四类约束来自用户态安全。用户页表、用户指针和缺页异常必须被明确隔离，不能让内核普通内存访问路径承担不可信地址的风险。

我们将设计目标整理为表 2-1。

#continued-table(
  "2-1",
  [内存管理子系统的设计目标],
  (1.1fr, 2.2fr, 2.2fr),
  (
    table.cell(fill: soft-fill)[#text(weight: "bold")[目标]],
    table.cell(fill: soft-fill)[#text(weight: "bold")[含义]],
    table.cell(fill: soft-fill)[#text(weight: "bold")[工程约束]],
  ),
  (
    table.cell(fill: warm-fill)[启动自举],
    table.cell(fill: warm-fill)[从启动分配器逐步过渡到完整运行期分配器。],
    table.cell(fill: warm-fill)[正式接管后必须封存启动分配器，并回收可归还的整页尾部。],
    table.cell(fill: soft-fill)[物理页可靠],
    table.cell(fill: soft-fill)[按固件内存段和保留区建立保留段信息的伙伴页分配器。],
    table.cell(fill: soft-fill)[不可把内核镜像、元数据页和设备区误交给普通分配路径。],
    table.cell(fill: handoff-fill)[分配路径分工],
    table.cell(fill: handoff-fill)[小对象走 Slab 分配器，大对象走内核堆。],
    table.cell(fill: handoff-fill)[各路径共享统计、审计和注册表，但热路径保持短。],
    table.cell(fill: stable-fill)[用户空间隔离],
    table.cell(fill: stable-fill)[用户虚拟地址空间管理 VMA、常驻页、COW 和共享后备。],
    table.cell(fill: stable-fill)[架构层只注入页表、布局、用户访问和缺页解码操作。],
  ),
  kind: "表",
  continuation-kind: "续表",
  align: (left, left, left),
)

这些目标决定了内存管理不能只围绕一种算法展开。启动分配器解决的是自举问题。伙伴页分配器解决物理页所有权问题。内核地址空间解决虚拟地址与物理页绑定问题。Slab 分配器和内核堆解决对象分配路径问题。用户虚拟地址空间面向用户态地址空间，处理 VMA、缺页和页表。每一层都只暴露自己能够稳定承诺的接口。

== 2.2 总体分层结构

`allocator` 是分层内核分配器的总入口。它保存启动分配器（`BootAllocator`）、伙伴页分配器（`BuddyAllocator`）、内核地址空间（`KernelAddressSpace`）、内核堆（`KernelHeap`）、Slab 分配器（`SlabAllocator`）、元数据分配器（`MetadataAllocator`）和分配注册表（`AllocationRegistry`）。这些对象共同构成一条从原始启动区间到运行期分配 API 的链路。

#figure(caption: figure-caption("图", "2-1", [内存管理子系统分层结构]))[
  #layer-card("用户地址空间层", "用户虚拟地址空间管理 VMA、常驻页、COW、共享映射和缺页处理", fill: stable-fill)
  #flow-arrow(label: "通过用户页表接口建立映射")
  #layer-card("对象分配层", "Slab 分配器处理小对象，内核堆处理大对象", fill: stable-fill)
  #flow-arrow(label: "请求带物理后备的虚拟范围")
  #layer-card("内核地址空间层", "内核地址空间管理直接映射和内核两类分配域", fill: handoff-fill)
  #flow-arrow(label: "申请物理页并调用架构映射回调")
  #layer-card("物理页层", "保留段信息的伙伴页分配器管理可用 RAM、保留区、内存区域和阶空闲链表", fill: soft-fill)
  #flow-arrow(label: "启动期元数据来源")
  #layer-card("启动分配层", "启动分配器只负责早期线性分配，正式接管后封存并移交尾部", fill: warm-fill)
]

这条分层链路有两个关键性质。其一是所有权向上转化。启动分配器只拥有一段启动期连续区间。伙伴页分配器接管可用物理页。地址空间层将物理页转化为带虚拟地址的后备映射范围（`BackedRange`）。Slab 分配器和内核堆再把这些范围解释为对象或缓存。其二是策略向上集中。伙伴页分配器不理解对象大小。地址空间层不理解 Slab 尺寸类别。Slab 分配器不理解用户态 VMA。用户地址空间策略集中在用户虚拟地址空间，不会散落到具体架构页表实现中。

分配器还维护统一的能力查询、统计、审计和回收入口。`capabilities()` 方法暴露 API 版本、页大小、小对象上限和 CPU 上限。`stats()` 方法汇总启动分配器、虚拟内存分配域、Slab 分配器和内核堆状态。`audit()` 方法可以扫描物理页、分配注册表、Slab 缓存和内核堆缓存。`reclaim()` 方法与 `reclaim_caches()` 方法用于主动释放缓存页。这些入口让内存管理具备运行期可观测性，而不需要外部模块直接读取内部结构。

== 2.3 初始化顺序与锁顺序

内存管理的初始化顺序由第一章中的启动链路触发。架构加载器先调用 `init_boot` 函数，建立最早期的线性分配能力。平台无关启动阶段解析固件后，调用 `bind_address_translation` 函数注入 `phys_to_virt` 回调和 `virt_to_phys` 回调。随后 `init_phys` 函数根据可用内存段和保留区初始化伙伴页分配器。架构层提供内核堆回调后，启动代码调用 `init_kernel_page_table` 函数，再进入 `init_vmem` 函数。最后依次初始化内核堆和 Slab 分配器，并通过 `activate_global` 函数切换全局分配器。

#pseudo-sample("2-1", [分配器初始化顺序], kind: "代码")[
  ```rust
  fn init_allocator(ctx: &StartContext, memory: &[MemorySegment], reserved: &[(usize, usize)]) {
      KERNEL_ALLOCATOR.bind_address_translation(ctx.address.phys_to_virt, ctx.address.virt_to_phys);

      KERNEL_ALLOCATOR.init_phys(memory, reserved)?;

      if let Some(ops) = ctx.allocator {
          KERNEL_ALLOCATOR.bind_kernel_heap_ops(
              ops.kernel_heap_region,
              ops.map_kernel_heap_range,
              ops.unmap_kernel_heap_range,
          );
          (ops.init_kernel_page_table)();
          KERNEL_ALLOCATOR.init_vmem(reserved)?;
          KERNEL_ALLOCATOR.init_kheap();
          KERNEL_ALLOCATOR.init_slab(cpu_count);
          KERNEL_ALLOCATOR.activate_global()?;
      }
  }
  ```
]

这个顺序有明确原因。`init_phys` 函数需要启动分配器提供初始化元数据，也需要 `phys_to_virt` 回调把物理元数据页转换为可访问地址。`init_vmem` 函数需要伙伴页分配器已经就绪，因为内核地址空间会建立直接映射区和内核分配域，并可能申请元数据页。元数据分配器只有在 `init_vmem` 成功并释放伙伴页分配器锁后，才能切到动态路径。源码中特意保留了这个顺序，避免虚拟内存分配域初始化期间持有物理页锁，又因元数据扩容重入物理页锁。

`activate_global` 函数是启动期和运行期的分界。它会检查启动分配器、物理页层、虚拟内存分配域、内核堆和 Slab 分配器是否已经全部就绪，初始化分配注册表，然后封存启动分配器。启动分配器封存后会取出当前游标之后的完整页尾部，通过 `virt_to_phys` 回调转成物理地址，再交还伙伴页分配器。包含已用字节的部分页不会释放，因为其中可能仍保存早期元数据。

锁顺序是分配器能稳定组合的另一个基础。当前源码中明确规定从 `init_lock` 初始化锁开始，随后依次是虚拟内存分配域、元数据、物理页、登记表分片、Slab 全局状态、Slab 每 CPU 缓存、内核堆、受管 GC 和精确根注册表。这个顺序有两个实际含义。第一，任何路径都不能反序获取锁。第二，调用架构层映射回调和可能触发分配的函数前，应先释放分配器内部锁。

#continued-table(
  "2-2",
  [分配器锁顺序与约束],
  (1.2fr, 2.4fr, 2.1fr),
  (
    table.cell(fill: soft-fill)[#text(weight: "bold")[区域]],
    table.cell(fill: soft-fill)[#text(weight: "bold")[锁顺序]],
    table.cell(fill: soft-fill)[#text(weight: "bold")[约束原因]],
  ),
  (
    table.cell(fill: warm-fill)[初始化],
    table.cell(fill: warm-fill)[`init_lock` 最先获取，正常运行期尽量不持有。],
    table.cell(fill: warm-fill)[避免初始化阶段和运行期分配交错。],
    table.cell(fill: soft-fill)[地址空间与元数据],
    table.cell(fill: soft-fill)[虚拟内存分配域早于元数据，元数据早于物理页。],
    table.cell(fill: soft-fill)[元数据动态扩容可能需要物理页，不能在持有物理页锁时反向进入虚拟内存分配域。],
    table.cell(fill: handoff-fill)[对象分配],
    table.cell(fill: handoff-fill)[分配注册表早于 Slab 全局状态，Slab 全局状态早于每 CPU 缓存，随后才进入内核堆。],
    table.cell(fill: handoff-fill)[小对象缓存和注册表记账需要一致的所有权顺序。],
    table.cell(fill: stable-fill)[受管对象],
    table.cell(fill: stable-fill)[受管 GC 早于精确根注册表。],
    table.cell(fill: stable-fill)[GC 扫描需要稳定根集合，根注册表不能反向调用分配热路径。],
  ),
  kind: "表",
  continuation-kind: "续表",
  align: (left, left, left),
)

锁顺序使分层结构具备可验证性。我们在后续优化单个分配路径时，只要没有破坏这个顺序，就不会把局部优化扩散成全局死锁风险。

== 2.4 启动分配与物理页管理

启动分配器是最早期的线性递增分配器。它内部只保存起始地址、结束地址、当前游标、`initialized` 标记和 `sealed` 标记。分配时按 `layout` 参数的对齐要求向前推进游标。实现使用 CAS 循环更新 `pos` 字段，因此即便在早期多核状态尚未完整建立时，也不会因为普通非原子写入产生游标回退。

#pseudo-sample("2-2", [启动期线性分配], kind: "代码")[
  ```rust
  fn boot_alloc(layout: Layout) -> *mut u8 {
      if !initialized() || sealed() {
          return null_mut();
      }

      loop {
          let pos = load_pos();
          let aligned = align_up(pos, layout.align());
          let next = aligned + layout.pad_to_align().size().max(1);
          if next > heap_end() {
              return null_mut();
          }
          if compare_exchange_pos(pos, next).is_ok() {
              return aligned as *mut u8;
          }
      }
  }
  ```
]

启动分配器的关键设计落在生命周期边界上，分配速度只是次要目标。它不支持释放。正式分配器接管之后，它会进入封存状态，后续 `alloc()` 方法必须失败。这个选择阻止了运行期代码继续从启动期池中分配对象。封存阶段还会把未用完整页尾部交给伙伴页分配器。这样早期预留容量不会永久浪费，已经存放启动元数据的部分页也不会被错误复用。

伙伴页分配器是运行期物理页管理的核心。当前实现保留内存段信息。它支持多个物理内存段，不假设整机 RAM 是连续区间。每个段保留来源于固件解析的边界，初始化时会排除内核镜像、保留区和分配器元数据页。伙伴页分配器还区分默认空闲链表和低端 DMA 空闲链表，并提供精确物理放置和大页对齐相关能力。

伙伴页分配器的经典分裂合并仍然保留。分配时根据请求页数计算阶数，并从对应或更高阶数的空闲链表获取空闲块。高阶块会被分裂为两个低阶伙伴。释放时检查伙伴是否空闲，满足条件就向上合并。当前实现还引入 0 阶延迟合并水位。频繁分配和释放 4 KiB 页时，立即合并会导致下一次分配又重新分裂。保留少量 0 阶热页可以减少分裂和合并抖动。当空闲比例降到阈值以下时，延迟合并会被禁用，优先恢复高阶连续空间。

物理页层还提供审计和回收统计。伙伴分配器统计（`BuddyStats`）记录总页数、已分配页、空闲页、保留页、元数据页、内存段数量、阶数统计、分裂次数、合并次数和分配失败次数。伙伴分配器审计（`BuddyAudit`）会扫描内存段、哈希节点和空闲链表，检查链表循环、节点失效和页记账不一致。这些数据对定位内存泄漏和错误释放非常关键。

== 2.5 内核地址空间、Slab 分配器与内核堆

内核地址空间位于伙伴页分配器和对象分配器之间。它解决的问题是拿到物理页后映射到哪里。它内部有两个分配域。`DirectMap` 分配域记录直接映射区。`Kernel` 分配域服务普通内核堆和大对象。一次带物理后备的分配会经历三步。先在分配域中保留虚拟地址，再向伙伴页分配器申请物理页，最后调用架构层提供的 `map_kernel_heap_range` 回调建立映射。释放时按相反方向撤销映射、归还物理页和释放虚拟区间。

这个层次把三个不同的重要操作隔离开。上层 Slab 分配器和内核堆不需要理解页表。下层伙伴页分配器不需要理解虚拟地址布局。架构层只提供映射和解映射回调，不进入分配器内部锁。后备映射范围是这层交给上层的核心对象，包含分配域、虚拟地址、物理地址、大小和阶数。

Slab 分配器处理高频小对象。当前尺寸类别覆盖 8 到 2048 字节。每个尺寸类别维护自己的 Slab 链表和 `SlabNode` 空闲链表。一个 Slab 由一批页作为后备映射范围，再切分为固定大小槽位。槽位状态通过 `alloc_bitmap` 字段和 `cache_bitmap` 字段记录。对象可能处于空闲、已分配或缓存中。热路径优先访问每 CPU 缓存。缓存命中时，只需要在本地缓存锁内弹出一个槽位。缓存失配时再进入 Slab 全局状态，通过位图和 `next_free_hint` 字段查找可用槽。

#pseudo-sample("2-3", [小对象分配路径], kind: "代码")[
  ```rust
  fn alloc_small(layout: Layout) -> Option<usize> {
      let class = size_class_for(layout)?;
      if let Some(entry) = per_cpu_cache[class].pop() {
          mark_cached_as_allocated(entry);
          return Some(entry.ptr);
      }

      let mut state = slab_state.lock();
      if let Some(slot) = state.find_free_slot(class) {
          return Some(slot.ptr);
      }

      let range = address_space.alloc_kernel_backed_range(class.slab_order)?;
      state.add_slab(class, range);
      state.find_free_slot(class).map(|slot| slot.ptr)
  }
  ```
]

释放路径同样优先回到每 CPU 缓存。缓存未满时，对象不会立即归还 Slab 全局状态。缓存满时会排出一批对象，再交给全局状态处理。这个策略减少了锁竞争，也降低了跨核共享压力。空 Slab 数量超过保留水位时，Slab 分配器会回收后备映射范围并复用 `SlabNode` 元数据。

内核堆处理 Slab 分配器不适合承载的大对象。它不直接操作页表，也不自己管理物理页算法，而是组合内核地址空间与伙伴页分配器。对于基础页和两页块，内核堆维护阶范围缓存。缓存命中时，可以直接复用已经具备虚拟地址和物理后备的范围。缓存满时，新释放范围会替换最旧范围，被淘汰者立即回到底层。这个环形队列避免满桶时移动大量槽位，也让近期释放后立即分配的模式走短路径。

Slab 分配器和内核堆共同服务 `GlobalAlloc` 全局分配器接口。小请求进入 Slab 分配器，大请求进入内核堆，所有成功分配都会进入分配注册表。登记表记录指针、大小、对齐、分配域、类别和后备信息。释放时先查询登记表，确认所有权，再按记录回到对应分配路径。这个设计让非法释放和重复释放更容易被发现，也为统计和审计提供统一入口。

== 2.6 进程地址空间与缺页处理

用户虚拟地址空间是用户态地址空间的顶层对象。它位于平台无关层，负责把 VMA 集合、常驻页映射、用户页表句柄、`brk` 游标、`mmap` 游标和 `mlock` 状态组织在一起。架构层只提供不透明的页表根句柄（`PgdHandle`）和一组函数指针。用户虚拟地址空间不知道 LoongArch64 或 RISC-V64 如何编码页表项，也不直接操作硬件寄存器。

VMA 集合由虚拟内存区域集合（`VmaSet`）管理，底层使用有序结构支持按地址查找、范围覆盖检查、插入、拆分和合并。`pages` 映射记录已经常驻的页。每个常驻页由常驻页对象（`ResidentPage`）表示，可能是匿名页、共享匿名页、私有文件页、共享文件页或直接映射页。共享文件页和共享匿名页通过全局弱引用表复用常驻页，避免同一共享后备在多个地址空间中被重复读入。

`map_anon` 函数和 `map_file` 函数只注册 VMA，不立即分配所有物理页。首次访问时，架构陷入处理器调用平台无关层的 `dispatch_page_fault` 缺页分派入口。这个入口先用缺页解码接口从陷阱帧中取出缺页类型、缺页地址和来源特权级。若缺页来自内核态用户访问路径，会先尝试让当前用户虚拟地址空间执行延迟补页，再用 `__ex_table` 异常表修复项把不可修复的用户指针错误转换为可返回的错误。若缺页来自用户态，则交给当前任务的 `VmSpace::handle_fault` 方法。

#pseudo-sample("2-4", [缺页处理分派], kind: "代码")[
  ```rust
  fn dispatch_page_fault(tf: TrapFramePtr) -> FaultOutcome {
      let kind = fault_decode.fault_kind(tf);
      let addr = fault_decode.fault_addr(tf);

      if !fault_decode.fault_from_user(tf) {
          if let Some(vm) = current_task_vm_space()
              && vm.handle_fault(addr, kind) == FaultOutcome::Fixed
          {
              return FaultOutcome::Fixed;
          }
          return if fault_decode.try_fixup_kernel_access(tf) {
              FaultOutcome::Fixed
          } else {
              FaultOutcome::Kernel(UncaughtKernelAccess)
          };
      }

      current_task_vm_space()
          .map(|vm| vm.handle_fault(addr, kind))
          .unwrap_or(FaultOutcome::Kernel(NoVmSpace))
  }
  ```
]

`VmSpace::handle_fault` 方法先定位缺页所在页和 VMA。若找不到 VMA，会尝试按向下增长规则扩展栈。若权限不满足，返回 `Segv` 枚举变体。若页已经常驻，写缺页可能触发 COW、共享脏页跟踪标记或权限修复。若页尚未常驻，则根据后备信息分配或获取常驻页。匿名页分配零页。共享匿名页通过共享标识和偏移查找共享页。文件映射按文件偏移读取页。直接映射页直接使用指定物理地址。完成后通过 `UserPgdOps::map` 方法建立用户页表映射。

`fork` 系统调用路径体现了用户虚拟地址空间的策略边界。`fork` 系统调用复制 VMA 元数据，已常驻页根据私有 COW 或共享语义重建页表。父进程中原本可写的私有页会被保护为 COW。子进程映射同一个常驻页，也以 COW 权限进入。第一次写入时，缺页处理器复制物理页并替换当前进程映射。共享文件页、共享匿名页、System V 共享内存和直接映射页保持共享语义。这个策略全部位于平台无关层，架构页表代码只执行映射、解映射、保护和 TLB 失效。

用户指针访问通过用户访问接口集中封装。系统调用路径和装载器调用 `copy_from_user` 函数、`copy_to_user` 函数或 `copy_cstr_from_user` 函数。这些安全包装把内核切片和用户地址传给架构实现。架构侧使用异常表处理访问失败。上层只看到 `Result<_, UserAccessError>` 类型表达式，从而把用户态非法指针归约为标准错误，而不会变成不可恢复的内核异常。

== 2.7 架构注入契约

内存管理的跨架构复用依赖四组操作接口。用户地址布局接口描述用户页大小、`brk` 基址、`mmap` 区间、默认栈、PIE（Position Independent Executable，位置无关可执行文件）基址、解释器基址和 vDSO 基址。用户页表接口提供用户页表的新建、销毁、映射、解映射、保护、`fork` 克隆、激活和 TLB 失效。用户访问接口提供用户内存复制和用户字符串长度读取。缺页解码接口从陷阱帧中提取缺页类型、缺页地址、来源特权级，并尝试进行内核态用户访问修复。

#continued-table(
  "2-3",
  [用户地址空间的架构注入接口],
  (1.4fr, 2.3fr, 2.1fr),
  (
    table.cell(fill: soft-fill)[#text(weight: "bold")[接口]],
    table.cell(fill: soft-fill)[#text(weight: "bold")[提供内容]],
    table.cell(fill: soft-fill)[#text(weight: "bold")[设计边界]],
  ),
  (
    table.cell(fill: warm-fill)[`UserVmLayoutOps`],
    table.cell(fill: warm-fill)[用户页大小、堆、`mmap`、栈、PIE、解释器和 vDSO 布局。],
    table.cell(fill: warm-fill)[平台无关层消费布局，不固化具体虚拟地址常量。],
    table.cell(fill: soft-fill)[`UserPgdOps`],
    table.cell(fill: soft-fill)[PGD 创建、释放、映射、解映射、保护、激活和 TLB 失效。],
    table.cell(fill: soft-fill)[页表格式保留在架构层。],
    table.cell(fill: handoff-fill)[`UserAccessOps`],
    table.cell(fill: handoff-fill)[`copy_from_user`、`copy_to_user` 和 `strnlen_user`。],
    table.cell(fill: handoff-fill)[用户指针异常由架构层修复，系统调用只处理错误码。],
    table.cell(fill: stable-fill)[`FaultDecodeOps`],
    table.cell(fill: stable-fill)[缺页类型、缺页地址、是否来自用户态以及内核访问修复。],
    table.cell(fill: stable-fill)[陷阱帧结构不进入平台无关层。],
  ),
  kind: "表",
  continuation-kind: "续表",
  align: (left, left, left),
)

这些接口采用 `AtomicPtr` 原子指针加 Release/Acquire 内存序的注册模式。架构层在调度器和用户态内存初始化前完成注册。平台无关层调用时只读取函数表，不保存架构私有结构。这个模式与调度器的架构钩子类似。它让 LoongArch64 和 RISC-V64 可以共用同一套用户虚拟地址空间策略，同时保留各自的页表格式和异常恢复实现。

== 2.8 工程设计总结

本章讨论的内存管理子系统，从启动期的线性分配一直延伸到用户态缺页处理。它的主线是把不同阶段的内存问题放在不同层次中处理。启动期关注能否可靠自举，物理页层关注所有权和碎片，地址空间层关注虚拟地址与物理后备的绑定，对象分配层关注热路径延迟和回收，用户地址空间层关注隔离、共享和异常恢复。

内存管理子系统具备以下创新：

第一是把启动自举和运行期分配做成可验证的单向交接。回顾整个分配器的启动过程，最容易混淆的点在于启动分配器是否只是一个临时实现。它确实简单，但它不是可以被随意绕过的临时工具。它负责为伙伴页分配器、元数据分配器和早期固件解析提供最初的分配能力。正式分配器接管后，它必须被封存。剩余完整页尾部也必须按所有权移交给伙伴页分配器。我们曾经可以选择让启动分配器永久保留一段私有池，后续在内存压力下再按需使用。这个方案会让早期内存与运行期内存之间出现长期双重所有权。当前设计把交接固定在 `activate_global` 函数，使启动期内存的生命周期有明确终点。这个边界让内存统计更可信，也避免运行期对象继续落入不可释放的启动区间。

第二是物理页管理保留了固件段信息和运行期审计能力。很多教学化的伙伴系统实现会把所有 RAM 展平为一个连续数组。真实平台上的可用内存通常并不连续，还夹杂内核镜像、固件保留区和设备窗口。我们在伙伴页分配器中保留内存段视图，并围绕内存区域、阶空闲链表、哈希节点和保留范围建立所有权管理。0 阶延迟合并用于降低高频页分配的分裂和合并抖动，低空闲比例时又会退回积极合并。更重要的是，伙伴页分配器提供可扫描的审计结果。它能检查链表循环、节点损坏和记账不一致。这个设计让物理页管理不只是一个分配算法，也成为可以长期观测和验证的内核基础设施。

第三是把虚拟地址、物理页和页表操作拆成三个相互约束的边界。内核地址空间不拥有物理页算法，也不理解对象大小。它只负责在分配域中保留虚拟地址，向伙伴页分配器请求物理后备，并调用架构注入的映射回调。这个结构看似多了一层，实际解决了两个长期问题。其一，Slab 分配器和内核堆不需要携带页表细节。其二，架构层不需要进入分配器锁内部。尤其是在内核堆使用独立高半区窗口时，直接映射区只承担物理跨度视图和统计角色，真实可分配性由内核分配域与伙伴页分配器共同决定。这种边界处理减少了启动期脆弱的保留区切分逻辑，也让后续替换页表实现时不影响对象分配策略。

第四是小对象和大对象采用不同热路径，同时通过分配注册表统一所有权。Slab 分配器面向 2048 字节以内的小对象，使用尺寸类别、位图、每 CPU 缓存和批量补货减少锁竞争。内核堆面向页级对象和大对象，使用范围缓存复用已经具备虚拟地址与物理后备的范围。二者的算法目标不同，但分配结果都会进入分配注册表。释放时必须先通过登记表确认指针来源，再回到对应后端。这个组合让热路径保持专门化，同时让错误释放、重复释放和统计审计具有统一入口。我们没有把所有对象都交给 Slab 分配器，也没有让大对象路径绕过记账。这个取舍保证了性能和所有权检查可以同时存在。

第五是用户地址空间策略集中在用户虚拟地址空间，架构层只实现机械动作。用户虚拟地址空间管理 VMA 集合、常驻页、共享后备、`fork` 系统调用的 COW、`mremap` 系统调用、`mprotect` 系统调用、缺页提交和文件页回写。架构层通过用户页表接口、用户访问接口和缺页解码接口提供页表操作、用户指针访问和缺页解码。这样做的结果是，LoongArch64 和 RISC-V64 可以共用同一套地址空间语义。页表格式、陷阱帧和异常表恢复仍保留在各自架构内部。这个边界对系统调用稳定性很重要。用户指针错误会通过用户访问错误返回给系统调用，而不是在平台无关层变成无法恢复的内核异常。

从整体看，内存管理子系统的价值不只在于若干算法的组合。更关键的是每个算法都被放在了合适的边界内。启动分配器只负责早期自举。伙伴页分配器只负责物理页。地址空间层只负责虚拟范围和映射协调。Slab 分配器和内核堆各自服务不同大小的对象。用户虚拟地址空间把用户地址空间策略从架构细节中分离出来。边界清晰之后，性能优化和正确性检查才能分别推进，而不会相互牵扯。
