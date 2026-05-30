#import "../config.typ": term-index-title
#import "../styles/index.typ": term-index-anchor, term-index-columns, term-index-entry, term-index-group, term-index-title-box, term-page, term-pages

#let pg = term-page
#let pgs = term-pages

#term-index-anchor(term-index-title)
#term-index-title-box(term-index-title)

#term-index-columns(
  [
    #term-index-group("A")[
      #term-index-entry(
        "ABBA 死锁",
        [由两条路径以相反顺序获取两把锁导致的循环等待。运行队列负载均衡的双锁场景通过强制全局锁顺序消除该死锁。],
        pages: pgs(pg("body", 6, "P6")),
      )
      #term-index-entry(
        "ACPI",
        [Advanced Configuration and Power Interface,高级配置与电源接口规范。当前启动链路中用来提供 RSDP、SPCR、电源控制和平台资源描述。],
        pages: pgs(pg("front", 1, "PI"), pg("body", 1, "P1"), pg("body", 3, "P3"), pg("body", 4, "P4")),
      )
      #term-index-entry(
        "ASID",
        [Address Space Identifier,地址空间标识符。在每个 TLB 条目上附加的标签,使切换地址空间时无需 flush 整个 TLB。],
        pages: pgs(pg("body", 2, "P2"), pg("body", 6, "P6")),
        divider: false,
      )
    ]

    #term-index-group("B")[
      #term-index-entry(
        "Block Device",
        [见 块设备。],
      )
      #term-index-entry(
        "Boot Services",
        [EFI 在退出前提供的一组运行时前固件服务。内核复制系统表与内存图之后退出该阶段。],
        pages: pgs(pg("body", 1, "P1")),
      )
      #term-index-entry(
        "BSS 段",
        [Block Started by Symbol,未初始化静态区。预启动初始化先清零 BSS,再保存启动参数,避免覆盖已写入的早期状态。],
        pages: pgs(pg("body", 1, "P1")),
      )
      #term-index-entry(
        "BSS Section",
        [见 BSS 段。],
      )
      
      #term-index-entry(
        "Buddy System",
        [见 伙伴系统。],
        divider: false,
      )
    ]

    #term-index-group("C")[
      #term-index-entry(
        "Capability",
        [POSIX.1e 定义的细粒度权限模型。将 root 特权拆分为若干独立能力,降低 setuid 程序的影响范围。],
        pages: pgs(pg("body", 5, "P5")),
      )
      #term-index-entry(
        "CFS",
        [Completely Fair Scheduler,完全公平调度器。Linux 早期的公平调度算法,EEVDF 是其继任者并引入合格性与 lag 机制。],
        pages: pgs(pg("body", 6, "P6")),
      )
      #term-index-entry(
        "Character Device",
        [见 字符设备。],
      )
      #term-index-entry(
        "clone",
        [创建新任务的统一系统调用。fork、vfork 和 pthread_create 都是 clone 在不同共享标志下的特例。],
        pages: pgs(pg("body", 5, "P5")),
      )
      #term-index-entry(
        "Copy-on-Write",
        [见 CoW。],
        pages: pgs(pg("body", 2, "P2"), pg("body", 5, "P5")),
      )
      #term-index-entry(
        "CoW",
        [写时复制。多个进程共享同一物理页时设置只读标记,首次写入触发页错误并复制物理页;广泛用于 fork 实现。],
        pages: pgs(pg("body", 2, "P2"), pg("body", 5, "P5")),
      )
      #term-index-entry(
        "Credentials",
        [凭据。任务的身份和权限快照,包含 UID/GID、有效 ID、保存 ID 和能力集,采用不可变快照加整体替换策略。],
        pages: pgs(pg("body", 5, "P5")),
      )
      #term-index-entry(
        "CSR",
        [Control and Status Register,控制与状态寄存器。],
        pages: pgs(pg("front", 3, "PIII"), pg("body", 1, "P1")),
        divider: false,
      )
    ]

    #term-index-group("D")[
      #term-index-entry(
        "Deadline",
        [截止时间。EEVDF 中任务的虚拟截止时间等于 vruntime 加权时间片;调度器在合格任务中选 deadline 最小者。],
        pages: pgs(pg("body", 6, "P6")),
      )
      #term-index-entry(
        "Demand Paging",
        [需求分页。虚拟地址在首次访问前不立即映射物理页,通过缺页异常按需建立映射,延迟物理内存消耗。],
        pages: pgs(pg("body", 2, "P2")),
      )
      #term-index-entry(
        "Dentry",
        [目录项。VFS 中将文件名映射到 inode 的缓存对象,支持路径解析的快速查找和挂载点穿越。],
        pages: pgs(pg("body", 4, "P4")),
      )
      #term-index-entry(
        "devtmpfs",
        [将设备模型对象映射为 /dev 下文件节点的特殊文件系统。inode 直接持有设备引用,open 时零查找。],
        pages: pgs(pg("body", 3, "P3"), pg("body", 4, "P4")),
      )
      #term-index-entry(
        "DMA",
        [Direct Memory Access,直接内存访问。某些设备需要物理地址连续的缓冲区用于 DMA 操作。],
        pages: pgs(pg("body", 2, "P2")),
      )
      #term-index-entry(
        "DMW",
        [Direct Mapping Window,直接映射窗口。LoongArch64 架构会通过 DMW 建立最小可访问内存与设备空间。],
        pages: pgs(pg("body", 1, "P1")),
      )
      #term-index-entry(
        "DTB",
        [Device Tree Blob,扁平设备树二进制。固件提供的硬件描述格式。],
        pages: pgs(pg("front", 1, "PI"), pg("body", 1, "P1"), pg("body", 3, "P3"), pg("body", 4, "P4")),
        divider: false,
      )
    ]

    #term-index-group("E")[
      #term-index-entry(
        "EDF",
        [Earliest Deadline First,最早截止时间优先调度算法。截止时间调度类(DL)使用该算法。],
        pages: pgs(pg("body", 6, "P6")),
      )
      #term-index-entry(
        "EEVDF",
        [Earliest Eligible Virtual Deadline First,最早合格虚拟截止时间优先。本系统公平调度类的核心算法,CFS 的继任者。],
        pages: pgs(pg("body", 6, "P6")),
      )
      #term-index-entry(
        "EFI",
        [Extensible Firmware Interface,可扩展固件接口。一种被广泛使用的固件接口规范,提供启动期硬件描述与运行时服务。],
        pages: pgs(pg("front", 1, "PI"), pg("body", 1, "P1")),
      )
      #term-index-entry(
        "EFI Stub",
        [EFI 入口薄层。它只处理最早期的 EFI handoff,然后尽快汇合到统一 `_start` 路径。],
        pages: pgs(pg("body", 1, "P1")),
      )
      #term-index-entry(
        "Eligible",
        [合格性。EEVDF 中任务的 vruntime 小于等于运行队列平均 vruntime 时被认为合格,只有合格任务参与调度。],
        pages: pgs(pg("body", 6, "P6")),
      )
      #term-index-entry(
        "External Fragmentation",
        [外部碎片。可用空间被分割成不连续小块后无法满足大块分配请求的问题。],
        pages: pgs(pg("body", 2, "P2")),
        divider: false,
      )
    ]

    #term-index-group("F")[
      #term-index-entry(
        "Firmware",
        [固件。启动前向内核提供平台事实的固件环境,正文中主要指 EFI、ACPI、DTB 及其相关数据源。],
        pages: pgs(pg("front", 3, "PIII"), pg("body", 1, "P1"), pg("body", 3, "P3")),
      )
      #term-index-entry(
        "fork",
        [创建当前任务副本的系统调用。本系统中是 clone 在不带共享标志时的特例,通常配合写时复制实现高效复制。],
        pages: pgs(pg("body", 5, "P5")),
      )
      #term-index-entry(
        "FreeList",
        [空闲链表。分配器中用于管理可用内存块的链表数据结构。Slab 与伙伴系统都依赖这一基础组织形式。],
        pages: pgs(pg("body", 2, "P2")),
        divider: false,
      )
    ]

    #term-index-group("G")[
      #term-index-entry(
        "GC",
        [Garbage Collection,垃圾回收。自动回收不再被引用的受管对象的技术,通常基于可达性分析与标记-清除算法。],
        pages: pgs(pg("body", 2, "P2")),
      )
      #term-index-entry(
        "Generational GC",
        [分代回收。基于"大多数对象生命周期很短"的经验观察,将堆分为年轻代与年老代以减少每次回收的工作量。],
        pages: pgs(pg("body", 2, "P2")),
      )
      #term-index-entry(
        "挂载命名空间",
        [Mount Namespace。每个进程可拥有独立的挂载点视图,使容器和沙箱可以拥有不同的文件系统拓扑。],
        pages: pgs(pg("body", 4, "P4")),
        divider: false
      )
    ]

    #term-index-group("H")[
      #term-index-entry(
        "HAL",
        [Hardware Abstraction Layer,硬件抽象层。它把不同架构的实现收敛为更高层可消费的统一接口。],
        pages: pgs(pg("body", 1, "P1")),
      )
      #term-index-entry(
        "Handoff",
        [阶段性交接。指某一层把稳定化后的上下文无返回地移交给下一层,启动上下文是典型代表。],
        pages: pgs(pg("front", 4, "PIV"), pg("body", 1, "P1")),
      )
      #term-index-entry(
        "伙伴系统",
        [Buddy System,物理页帧分配的经典算法。通过分裂与合并维护幂次大小的空闲块链表,兼顾分配速度与碎片控制。],
        pages: pgs(pg("body", 2, "P2")),
        divider: false,
      )
    ]

    #term-index-group("I")[
      #term-index-entry(
        "init 进程",
        [PID 命名空间内的 1 号任务。所有孤儿任务移交给它收养,init 退出会导致命名空间内所有任务被强制终止。],
        pages: pgs(pg("body", 1, "P1"), pg("body", 5, "P5")),
      )
      #term-index-entry(
        "initramfs",
        [初始内存文件系统。内核启动后期挂载的临时根文件系统,在没有 root= 命令行参数时优先使用。],
        pages: pgs(pg("body", 1, "P1")),
      )
      #term-index-entry(
        "Inode",
        [索引节点。VFS 中表示文件元数据的对象,与文件名解耦,多个 dentry 可以指向同一 inode(硬链接)。],
        pages: pgs(pg("body", 4, "P4")),
      )
      #term-index-entry(
        "Internal Fragmentation",
        [内部碎片。分配块内部未被利用的空间,由于固定分配粒度造成,伙伴系统的固有局限之一。],
        pages: pgs(pg("body", 2, "P2")),
      )
      #term-index-entry(
        "ioctl",
        [I/O 控制系统调用。POSIX 用整数命令码与原始指针传递设备特定命令,本系统通过类型安全的 DriverControl 接口替代。],
        pages: pgs(pg("body", 3, "P3"), pg("body", 4, "P4")),
        divider: false,
      )
    ]

    #term-index-group("J")[
      #term-index-entry(
        "即插即用设备",
        [见 PnP。],
        divider: false,
      )
    ]

    #term-index-group("K")[
      #term-index-entry(
        "kernel_start_init",
        [平台无关的启动编排入口。它消费一次性的 StartContext,并继续组织分配器、平台状态与内核长期运行环境。],
        pages: pgs(pg("body", 1, "P1")),
      )
      #term-index-entry(
        "Kernel Heap",
        [见 内核堆。],
      )
      #term-index-entry(
        "块设备",
        [Block Device,以固定大小扇区进行随机访问的设备抽象。提供异步提交接口和完成回调,与字符设备并列构成双轨设备模型。],
        pages: pgs(pg("body", 3, "P3"), pg("body", 4, "P4")),
        divider: false,
      )
    ]

    #term-index-group("L")[
      #term-index-entry(
        "lag",
        [滞后量。EEVDF 中任务离开运行队列时的离队 avg_vruntime 减去 vruntime;入队时用于精确恢复任务的领先或落后程度。],
        pages: pgs(pg("body", 6, "P6")),
      )
      #term-index-entry(
        "Load Balance",
        [负载均衡。在多核每 CPU 运行队列设计下,定期或在唤醒时迁移任务以平衡各 CPU 的负载。],
        pages: pgs(pg("body", 6, "P6")),
      )
      #term-index-entry(
        "Lock Ordering",
        [锁顺序。为防止死锁而规定的一组锁的获取顺序,所有代码路径必须按照该顺序获取锁。],
        pages: pgs(pg("body", 2, "P2"), pg("body", 6, "P6")),
        divider: false,
      )
    ]

    #term-index-group("M")[
      #term-index-entry(
        "Mark-Sweep",
        [标记-清除算法。垃圾回收的一种实现方式,先从根集合标记所有可达对象,再清除未标记对象。],
        pages: pgs(pg("body", 2, "P2")),
      )
      #term-index-entry(
        "Memory Map",
        [启动期内存图。由固件或启动协议提供,并在 loader 中复制稳定化后供后续解析与分配器初始化使用。],
        pages: pgs(pg("body", 1, "P1"), pg("body", 2, "P2")),
      )
      #term-index-entry(
        "MMIO",
        [Memory-Mapped I/O,内存映射 I/O。设备寄存器访问通过 MMIO 地址转换函数进入统一框架。],
        pages: pgs(pg("body", 1, "P1"), pg("body", 3, "P3")),
      )
      #term-index-entry(
        "mmap",
        [内存映射系统调用。用于将匿名内存或文件映射到进程地址空间,触发 VMA 创建与按需分页。],
        pages: pgs(pg("body", 2, "P2")),
      )
      #term-index-entry(
        "命名空间",
        [Namespace。资源视图的隔离机制,使容器内进程看到的资源与裸金属上不同。],
        pages: pgs(pg("body", 4, "P4"), pg("body", 5, "P5")),
      )
      #term-index-entry(
        "Mount Namespace",
        [见 挂载命名空间。],
        divider: false,
      )
    ]

    #term-index-group("N")[
      #term-index-entry(
        "Namespace",
        [见 命名空间。],
      )
      #term-index-entry(
        "内核堆",
        [Kernel Heap。内核用于动态内存分配的虚拟地址空间区域及其相关数据结构,大块分配的主要入口。],
        pages: pgs(pg("body", 2, "P2")),
      )
      #term-index-entry(
        "ns",
        [见 命名空间。],
        divider: false,
      )
    ]

    #term-index-group("P")[
      #term-index-entry(
        "Page Fault",
        [缺页异常。访问尚未映射或权限不足的虚拟地址时由处理器产生,内核根据 VMA 属性分类处理。],
        pages: pgs(pg("body", 2, "P2")),
      )
      #term-index-entry(
        "Page Frame",
        [物理页帧。物理内存的基本分配单位,通常大小为 4 KiB,由伙伴系统统一管理。],
        pages: pgs(pg("body", 2, "P2"), pg("body", 4, "P4")),
      )
      #term-index-entry(
        "Page Table",
        [页表。建立虚拟地址到物理地址映射的多级树状结构,内核与用户进程各自维护独立的页表层次。],
        pages: pgs(pg("front", 5, "PV"), pg("body", 1, "P1"), pg("body", 2, "P2")),
      )
      #term-index-entry(
        "PCI",
        [Peripheral Component Interconnect,外围组件互连。连接外部设备的总线标准,PnP 框架下的总线类型之一。],
        pages: pgs(pg("body", 3, "P3"), pg("body", 4, "P4")),
      )
      #term-index-entry(
        "PELT",
        [Per-Entity Load Tracking,按调度实体的负载跟踪机制。通过指数衰减加权过去周期的运行时占比衡量任务负载。],
        pages: pgs(pg("body", 6, "P6")),
      )
      #term-index-entry(
        "Per-CPU Cache",
        [每 CPU 缓存。将热分配操作保留在本地 CPU 上以减少锁竞争的数据结构,在 Slab 与运行队列设计中均有体现。],
        pages: pgs(pg("body", 2, "P2"), pg("body", 6, "P6")),
      )
      #term-index-entry(
        "PID 命名空间",
        [PID Namespace。容器隔离机制,使容器内进程看到的 PID 序列与裸金属上一致;一个任务在每层命名空间中拥有独立的 PID。],
        pages: pgs(pg("body", 5, "P5")),
      )
      #term-index-entry(
        "PnP",
        [Plug and Play,即插即用框架。统一管理 PCI、USB、Platform 设备的生命周期、状态机和驱动绑定。],
        pages: pgs(pg("body", 3, "P3")),
        divider: false,
      )
    ]

    #term-index-group("Q")[
      #term-index-entry(
        "启动分配器",
        [Boot Allocator。内核启动阶段最早期的 bump 分配器,只分配不回收,用极简实现换取最大可靠性。],
        pages: pgs(pg("body", 2, "P2")),
      )
      #term-index-entry(
        "启动上下文",
        [StartContext 的中文称呼。loader 到 kernel_start_init 的一次性交接对象,封装稳定化后的启动事实与必要回调。],
        pages: pgs(pg("front", 2, "PII"), pg("body", 1, "P1")),
        divider: false,
      )
    ]

    #term-index-group("R")[
      #term-index-entry(
        "Reachability Analysis",
        [可达性分析。垃圾回收中通过追踪引用关系确定对象是否仍在使用的方法,从根集合出发遍历所有可达对象。],
        pages: pgs(pg("body", 2, "P2")),
      )
      #term-index-entry(
        "Root Set",
        [根集合。垃圾回收中已知活跃的对象集合,通常包括全局变量、栈变量与内核长期持有的引用。],
        pages: pgs(pg("body", 2, "P2")),
      )
      #term-index-entry(
        "RSDP",
        [Root System Description Pointer,根系统描述指针。ACPI 路径中的根指针,内核通过它定位后续的系统描述表集合。],
        pages: pgs(pg("body", 1, "P1")),
      )
      #term-index-entry(
        "RT 调度类",
        [Real-Time Scheduling Class。实时调度策略,支持 SCHED_FIFO 与 SCHED_RR;优先级严格高于公平调度类。],
        pages: pgs(pg("body", 6, "P6")),
      )
      #term-index-entry(
        "热插拔",
        [见 PnP。],
        divider: false,
      )
    ]

    #term-index-group("S")[
      #term-index-entry(
        "SchedClass",
        [调度类。对调度策略的抽象,系统支持 Fair、RT、DL 三类共存,通过严格的优先级顺序协作。],
        pages: pgs(pg("body", 6, "P6")),
      )
      #term-index-entry(
        "SchedDomain",
        [调度域。描述 CPU 拓扑的层次化数据结构,从超线程到 NUMA 节点;不同层次有不同的负载均衡频率。],
        pages: pgs(pg("body", 6, "P6")),
      )
      #term-index-entry(
        "Signal",
        [POSIX 异步通知机制。两阶段投递:发送加入待处理队列,接收在返回用户态前处理;SIGKILL 和 SIGSTOP 不可屏蔽。],
        pages: pgs(pg("body", 5, "P5")),
      )
      #term-index-entry(
        "Slab",
        [Slab 缓存。由一个或多个页帧组成,内部分为固定大小的对象槽用于小对象高频分配。],
        pages: pgs(pg("body", 2, "P2")),
      )
      #term-index-entry(
        "Slab Coloring",
        [Slab 着色。在 Slab 开头添加偏移以错开不同 Slab 中对象的缓存行位置,减少缓存行抖动。],
        pages: pgs(pg("body", 2, "P2")),
      )
      #term-index-entry(
        "StartContext",
        [见 启动上下文。],
      )
      #term-index-entry(
        "Superblock",
        [超级块。VFS 中表示一个具体文件系统实例的对象,持有该文件系统的元数据、根 inode 和 inode 缓存。],
        pages: pgs(pg("body", 4, "P4")),
        divider: false,
      )
    ]

    #term-index-group("T")[
      #term-index-entry(
        "Task",
        [任务。本系统调度器管理的最小实体,既可代表进程也可代表线程,具体由 clone 创建时的共享标志决定。],
        pages: pgs(pg("body", 5, "P5"), pg("body", 6, "P6")),
      )
      #term-index-entry(
        "Thread Group",
        [线程组。共享地址空间和文件描述符表的任务集合,getpid 返回的是线程组 leader 的 PID。],
        pages: pgs(pg("body", 5, "P5")),
      )
      #term-index-entry(
        "TLB",
        [Translation Lookaside Buffer,地址转换后备缓冲区。缓存虚拟地址到物理地址的转换结果,ASID 优化减少其失效开销。],
        pages: pgs(pg("body", 2, "P2"), pg("body", 6, "P6")),
      )
      #term-index-entry(
        "Tracing GC",
        [基于追踪的垃圾回收。从根集合出发追踪所有可达对象,与基于引用计数的 GC 相对。],
        pages: pgs(pg("body", 2, "P2")),
        divider: false,
      )
    ]

    #term-index-group("U")[
      #term-index-entry(
        "UART",
        [Universal Asynchronous Receiver/Transmitter,通用异步收发器。串行通信硬件,字符设备模型下的典型流式设备。],
        pages: pgs(pg("body", 3, "P3")),
      )
      #term-index-entry(
        "User Copy",
        [用户态数据复制。内核与用户态之间复制数据时的安全检查机制,通过异常表实现"乐观执行、异常修复"。],
        pages: pgs(pg("body", 2, "P2")),
        divider: false,
      )
    ]

    #term-index-group("V")[
      #term-index-entry(
        "vfork",
        [创建子任务并阻塞父任务直到子任务 exec 或 exit 的系统调用。本系统中是 clone 在 CLONE_VFORK 标志下的特例。],
        pages: pgs(pg("body", 5, "P5")),
      )
      #term-index-entry(
        "VFS",
        [Virtual File System,虚拟文件系统。内核中抽象文件系统操作的统一接口层,通过 inode、dentry、superblock 等对象组织。],
        pages: pgs(pg("body", 4, "P4")),
      )
      #term-index-entry(
        "VirtIO",
        [虚拟化 I/O 框架。一种半虚拟化设备的标准接口,广泛用于虚拟机环境中的高性能设备访问。],
        pages: pgs(pg("body", 3, "P3"), pg("body", 4, "P4")),
      )
      #term-index-entry(
        "VMA",
        [Virtual Memory Area,虚拟内存区域。进程地址空间中一段连续的虚拟地址范围,具有独立的属性和后备存储类型。],
        pages: pgs(pg("body", 2, "P2")),
      )
      #term-index-entry(
        "vruntime",
        [虚拟运行时间。EEVDF 中任务在调度器视角下的运行时间,推进速度与权重成反比,用于实现权重公平。],
        pages: pgs(pg("body", 6, "P6")),
        divider: false,
      )
    ]

    #term-index-group("W")[
      #term-index-entry(
        "WaitQueue",
        [等待队列。允许任务阻塞直到某条件满足的同步原语,使用弱引用存储等待者以避免任务自我保活。],
        pages: pgs(pg("body", 5, "P5")),
      )
      #term-index-entry(
        "Wake Balance",
        [唤醒均衡。任务从睡眠中唤醒时,在缓存亲和性和当前负载之间权衡选择目标 CPU。],
        pages: pgs(pg("body", 6, "P6")),
        divider: false,
      )
    ]

    #term-index-group("X")[
      #term-index-entry(
        "写时复制",
        [见 Copy-on-Write。],
        divider: false,
      )
    ]

    #term-index-group("Z")[
      #term-index-entry(
        "Zombie",
        [僵尸状态。任务调用 exit 后释放大部分资源但保留退出码,等待父任务通过 wait 回收。],
        pages: pgs(pg("body", 5, "P5")),
      )
      #term-index-entry(
        "Zone",
        [内存区域。用于区分不同用途或约束的内存子集,如普通区域和 DMA 区域。],
        pages: pgs(pg("body", 2, "P2")),
      )
      #term-index-entry(
        "字符设备",
        [Character Device,以字节流方式访问的设备抽象。接口拆分阻塞与非阻塞,精确匹配流式设备的行为模式。],
        pages: pgs(pg("body", 3, "P3"), pg("body", 4, "P4")),
        divider: false,
      )
    ]
  ]
)