#import "../config.typ": term-index-title
#import "../styles/index.typ": term-index-anchor, term-index-columns, term-index-entry, term-index-group, term-index-title-box
#import "../styles/tokens.typ": panel-fill

#term-index-anchor(term-index-title)
#term-index-title-box(term-index-title)

#term-index-columns(
  [
    #term-index-group("A")[
      #term-index-entry("安全点", [垃圾回收或运行时检查可以安全观察线程状态的位置。], divider: false)
      #term-index-entry("按需分页", [虚拟地址在首次访问时才建立物理映射的内存管理策略。], divider: false)
    ]

    #term-index-group("B")[
      #term-index-entry("保护页", [故意保留为不可访问的页面，用于捕获越界访问或栈增长边界。], divider: false)
      #term-index-entry("备用信号栈", [信号处理函数可以使用的独立用户栈，常用于处理普通用户栈不可用的情况。], divider: false)
      #term-index-entry("比较并交换", [一种原子读改写操作，用于无锁数据结构和并发状态转换。], divider: false)
      #term-index-entry("扁平设备树二进制", [固件传递给内核的设备树二进制表示，用于描述平台硬件。], divider: false)
      #term-index-entry("标记-清除", [垃圾回收中的基础算法，先标记可达对象，再清扫不可达对象。], divider: false)
      #term-index-entry("标准错误", [用户程序默认的错误输出流。], divider: false)
      #term-index-entry("标准输出", [用户程序默认的普通输出流。], divider: false)
      #term-index-entry("标准输入", [用户程序默认的输入流。], divider: false)
    ]

    #term-index-group("C")[
      #term-index-entry("程序断点扩展", [用户地址空间中堆顶扩展相关机制，通常用于传统堆区域增长。], divider: false)
      #term-index-entry("程序解释器", [装载可执行文件时需要同时装入的用户态解释器，常见于动态链接程序。], divider: false)
      #term-index-entry("程序头", [可执行文件中描述可装载段、解释器和权限等信息的结构。], divider: false)
    ]

    #term-index-group("D")[
      #term-index-entry("打开文件描述", [多个文件描述符可以共同引用的打开文件状态。], divider: false)
      #term-index-entry("大小类", [小对象分配器中按照对象尺寸划分的分配类别。], divider: false)
      #term-index-entry("待处理信号", [已经投递到任务或线程组但尚未被处理的信号。], divider: false)
      #term-index-entry("单向依赖", [上层只消费下层导出的稳定接口，下层不反向调用上层策略的依赖关系。], divider: false)
      #term-index-entry("等待队列", [阻塞任务挂接的位置，事件发生后由唤醒路径重新调度。], divider: false)
      #term-index-entry("地址空间", [进程或内核可见的一组虚拟地址范围，以及这些范围到物理后备的映射关系。], divider: false)
      #term-index-entry("地址空间标识符", [标记地址空间身份的处理器标签，用于减少地址空间切换时的缓存刷新成本。], divider: false)
      #term-index-entry("地址空间布局随机化", [通过随机化可执行文件、栈、堆和映射区域的位置，降低地址预测攻击的效果。], divider: false)
      #term-index-entry("地址转换后备缓冲", [处理器缓存虚拟地址到物理地址翻译结果的结构。], divider: false)
      #term-index-entry("调度类", [调度器中负责某类策略和任务集合的策略单元。], divider: false)
      #term-index-entry("调度实体", [参与调度排序和记账的对象。], divider: false)
      #term-index-entry("调度系统", [负责任务选择、阻塞唤醒、抢占和运行队列维护的子系统。], divider: false)
      #term-index-entry("调度域", [描述处理器拓扑和任务迁移边界的结构。], divider: false)
      #term-index-entry("定时器钩子", [架构或平台向调度和时间子系统提供的定时事件入口。], divider: false)
      #term-index-entry("动态链接器", [装载动态链接程序时负责解析共享对象和重定位的用户态组件。], divider: false)
      #term-index-entry("读写锁", [允许多个读者并发进入、写者独占进入的同步原语。], divider: false)
      #term-index-entry("短临界区", [持锁时间较短且不应执行阻塞操作的关键区域。], divider: false)
      #term-index-entry("对称多处理", [多个处理器以相同角色共享同一个操作系统实例的多处理结构。], divider: false)
    ]

    #term-index-group("F")[
      #term-index-entry("非一致内存访问", [不同处理器访问不同内存节点时延迟和带宽不完全一致的体系结构。], divider: false)
      #term-index-entry("分层结构", [把内核职责拆成若干层级，使每层只依赖下层稳定边界。], divider: false)
      #term-index-entry("分代回收", [把对象按生命周期划分代际，以降低垃圾回收扫描成本的策略。], divider: false)
      #term-index-entry("分段感知伙伴分配器", [保留固件内存段信息的伙伴页分配器。], divider: false)
      #term-index-entry("分配器", [负责向内核或用户地址空间提供内存对象、页或地址范围的组件。], divider: false)
      #term-index-entry("分配注册表", [记录分配结果来源和尺寸的登记结构，用于释放检查和统计审计。], divider: false)
      #term-index-entry("浮点单元", [处理浮点寄存器和浮点运算状态的处理器部件。], divider: false)
      #term-index-entry("符号链接", [目录项中保存另一路径文本的文件类型，路径解析时需要继续展开。], divider: false)
      #term-index-entry("负向缓存", [记录某个名称不存在的目录项缓存，用于避免重复查找。], divider: false)
      #term-index-entry("负载均衡", [调度器在处理器或运行队列之间迁移任务以平衡执行压力的机制。], divider: false)
      #term-index-entry("辅助向量", [内核在执行新程序时压入用户栈的键值数组，用于传递页面大小、入口信息等启动参数。], divider: false)
    ]

    #term-index-group("G")[
      #term-index-entry("高级配置与电源接口", [固件向操作系统提供平台配置、电源管理和设备描述的规范。], divider: false)
      #term-index-entry("根集合", [垃圾回收从中开始可达性分析的一组根引用。], divider: false)
      #term-index-entry("根文件系统", [启动完成后作为路径解析根的文件系统。], divider: false)
      #term-index-entry("根系统描述指针", [固件提供的系统描述入口指针，用于定位相关平台表。], divider: false)
      #term-index-entry("工作队列", [把可延迟处理的工作项放入队列并由内核工作上下文执行的机制。], divider: false)
      #term-index-entry("固件", [启动前向内核提供平台事实的执行环境和数据来源。], divider: false)
      #term-index-entry("固件表", [固件提供的结构化平台信息表。], divider: false)
      #term-index-entry("挂载标志", [描述挂载点属性和访问限制的标志集合。], divider: false)
      #term-index-entry("挂载点", [文件系统树中另一个文件系统实例接入的位置。], divider: false)
      #term-index-entry("挂载命名空间", [为进程提供独立文件系统挂载视图的命名空间。], divider: false)
    ]

    #term-index-group("H")[
      #term-index-entry("后备区间", [虚拟地址范围背后已经分配或可按需分配的内存区间。], divider: false)
      #term-index-entry("后台进程组", [不在控制终端前台的进程组，终端读写可能触发作业控制信号。], divider: false)
      #term-index-entry("缓存行", [处理器缓存与内存之间搬运数据的基本粒度。], divider: false)
      #term-index-entry("缓存一致性", [多个处理器或设备观察同一内存位置时保持一致结果的约束。], divider: false)
      #term-index-entry("回环接口", [本机网络通信使用的虚拟网络接口。], divider: false)
      #term-index-entry("伙伴系统", [按二次幂大小拆分与合并空闲块的物理页分配算法。], divider: false)
    ]

    #term-index-group("J")[
      #term-index-entry("即时探测", [设备发现后立即尝试匹配驱动并发布能力的过程。], divider: false)
      #term-index-entry("基址寄存器", [外设总线中描述设备资源窗口起始地址和类型的寄存器。], divider: false)
      #term-index-entry("记住对象", [分代垃圾回收中记录跨代引用的对象。], divider: false)
      #term-index-entry("交接对象", [某一阶段稳定化后传给下一阶段的一次性上下文对象。], divider: false)
      #term-index-entry("阶段性交接", [控制权和上下文从一个阶段无返回地移交给下一个阶段的过程。], divider: false)
      #term-index-entry("截止时间", [调度实体在截止时间类或公平调度模型中用于排序的时间点。], divider: false)
      #term-index-entry("截止时间调度类", [按照截止时间和预算约束选择任务的调度类。], divider: false)
      #term-index-entry("进程", [拥有地址空间、文件描述符表和资源视图的执行容器。], divider: false)
      #term-index-entry("进程标识符", [内核和用户态用于指代进程的整数名称。], divider: false)
      #term-index-entry("进程间通信", [不同进程之间交换数据、事件或共享内存的机制。], divider: false)
      #term-index-entry("进程文件系统", [把进程和内核状态投影为文件树的特殊文件系统。], divider: false)
      #term-index-entry("进程组", [用于作业控制和信号投递的一组进程。], divider: false)
      #term-index-entry("进程组标识符", [用户态和内核用于指代进程组的整数名称。], divider: false)
      #term-index-entry("精确根槽", [受管堆中记录精确根引用位置的槽位。], divider: false)
      #term-index-entry("就绪状态", [对象当前可以执行读、写或状态推进的用户可见状态。], divider: false)
    ]

    #term-index-group("K")[
      #term-index-entry("可达性分析", [从根集合出发判断对象是否仍可被访问的分析过程。], divider: false)
      #term-index-entry("可扩展固件接口", [固件向内核提供启动服务和运行时服务的接口规范。], divider: false)
      #term-index-entry("可睡眠锁", [允许持锁路径进入睡眠状态的同步原语。], divider: false)
      #term-index-entry("可移植操作系统接口", [用户态程序与类 Unix 操作系统之间的一组可移植接口规范。], divider: false)
      #term-index-entry("可执行与可链接格式", [用户程序和共享对象使用的二进制文件格式。], divider: false)
      #term-index-entry("空闲调度类", [没有普通任务可运行时选择空闲任务的调度类。], divider: false)
      #term-index-entry("空闲链表", [保存当前可复用对象、页或块的链表。], divider: false)
      #term-index-entry("控制端点", [面向设备或子系统控制命令的用户态或内核态入口。], divider: false)
      #term-index-entry("控制请求", [类型化控制路径中表达操作意图和参数的请求对象。], divider: false)
      #term-index-entry("控制台", [内核早期和运行期用于输出日志或交互字符的设备入口。], divider: false)
      #term-index-entry("控制与状态寄存器", [处理器中保存控制状态和异常信息的寄存器。], divider: false)
      #term-index-entry("快速路径", [常见场景下尽量减少分支、锁和额外检查的执行路径。], divider: false)
    ]

    #term-index-group("L")[
      #term-index-entry("垃圾回收", [自动回收不再可达对象的内存管理技术。], divider: false)
      #term-index-entry("懒加载", [对象或页面在首次访问时才真正装入或建立映射的策略。], divider: false)
      #term-index-entry("老年代", [分代回收中保存生命周期较长对象的区域。], divider: false)
      #term-index-entry("类型化对象", [保留具体设备、文件或协议语义的结构化对象。], divider: false)
      #term-index-entry("类型化控制", [用结构化请求和响应替代无类型命令码的控制方式。], divider: false)
      #term-index-entry("类型化请求", [与具体设备或子系统绑定的结构化控制请求。], divider: false)
      #term-index-entry("类型化载荷", [投影或控制路径中携带具体类型信息的载荷。], divider: false)
      #term-index-entry("临界区", [需要同步保护、不能被并发破坏的一段代码或状态访问范围。], divider: false)
      #term-index-entry("临时文件系统", [以内存对象为主要后备的临时文件系统。], divider: false)
      #term-index-entry("流式套接字", [面向字节流传输的套接字类型。], divider: false)
      #term-index-entry("路径解析", [根据路径字符串逐级查找目录项、跨越挂载点并处理符号链接的过程。], divider: false)
      #term-index-entry("轮询", [反复检查设备、套接字或事件状态以推动系统前进的机制。], divider: false)
    ]

    #term-index-group("M")[
      #term-index-entry("每处理器缓存", [按处理器分片的小对象缓存，用于减少全局锁竞争。], divider: false)
      #term-index-entry("命令行", [固件或启动器传给内核的参数字符串。], divider: false)
      #term-index-entry("命名空间", [为进程提供隔离视图的内核对象集合。], divider: false)
      #term-index-entry("默认动作", [信号未被用户处理时采用的内核预设处理方式。], divider: false)
      #term-index-entry("目录项", [文件系统路径解析中把名称关联到索引节点的对象。], divider: false)
      #term-index-entry("目录项缓存", [缓存名称查找结果以加速路径解析的结构。], divider: false)
    ]

    #term-index-group("N")[
      #term-index-entry("内部碎片", [分配块内部未被实际使用的空间。], divider: false)
      #term-index-entry("内存保护", [改变虚拟内存区域访问权限的机制。], divider: false)
      #term-index-entry("内存分配器", [管理内核堆、页帧、小对象或受管对象的分配组件。], divider: false)
      #term-index-entry("内存管理子系统", [负责物理页、虚拟地址空间、堆和缺页处理的子系统。], divider: false)
      #term-index-entry("内存区域", [物理内存或虚拟地址空间中的一段可描述范围。], divider: false)
      #term-index-entry("内存碎片", [可用内存因分布或分配粒度原因难以满足后续请求的现象。], divider: false)
      #term-index-entry("内存锁定", [阻止指定用户内存范围被换出或回收的机制。], divider: false)
      #term-index-entry("内存映射输入输出", [把设备寄存器映射到内存地址空间后进行访问的方式。], divider: false)
      #term-index-entry("内核地址空间", [内核可见和管理的虚拟地址空间。], divider: false)
      #term-index-entry("内核堆", [运行期用于分配内核对象和大对象的堆空间。], divider: false)
      #term-index-entry("内核态", [处理器以特权级执行内核代码的状态。], divider: false)
      #term-index-entry("内核栈", [内核执行任务上下文时使用的栈。], divider: false)
    ]

    #term-index-group("P")[
      #term-index-entry("凭据", [任务身份和权限的快照，包含用户、组和能力相关状态。], divider: false)
      #term-index-entry("凭据快照", [以不可变方式保存的一组身份和权限字段。], divider: false)
      #term-index-entry("平台设备", [由固件或平台代码描述、并非通过可枚举总线发现的设备。], divider: false)
      #term-index-entry("平台无关", [不依赖具体指令集、固件入口或平台寄存器的通用逻辑。], divider: false)
      #term-index-entry("平台资源", [固件或总线为设备描述的地址窗口、中断和其他依赖。], divider: false)
      #term-index-entry("普通信号栈", [用户程序常规执行路径使用的信号处理栈。], divider: false)
    ]

    #term-index-group("Q")[
      #term-index-entry("启动参数", [固件或启动器传给内核的初始参数集合。], divider: false)
      #term-index-entry("启动阶段", [进入主入口之前完成基础设施建立和上下文交接的阶段。], divider: false)
      #term-index-entry("启动控制流", [从固件入口到内核主入口之间的控制权转移过程。], divider: false)
      #term-index-entry("启动上下文", [架构相关启动阶段整理后传给平台无关层的稳定上下文。], divider: false)
      #term-index-entry("启动协议", [固件、启动器和内核之间约定启动参数与入口方式的协议。], divider: false)
      #term-index-entry("启动映射", [启动早期为访问内核镜像、栈、固件表和设备窗口建立的地址映射。], divider: false)
      #term-index-entry("前台进程组", [拥有控制终端前台访问权的进程组。], divider: false)
      #term-index-entry("强引用", [保持对象存活的引用关系。], divider: false)
      #term-index-entry("全局回收", [跨缓存、页和文件后备协调释放内存的回收过程。], divider: false)
      #term-index-entry("权限检查", [根据凭据、模式位、能力和上下文判断操作是否允许的过程。], divider: false)
      #term-index-entry("缺页", [访问尚未建立有效映射的虚拟地址时产生的异常。], divider: false)
      #term-index-entry("缺页处理", [根据异常地址和访问类型建立映射或向上返回错误的过程。], divider: false)
      #term-index-entry("缺页处理器", [执行缺页修复或错误返回的处理逻辑。], divider: false)
    ]

    #term-index-group("R")[
      #term-index-entry("热插拔", [设备在运行期加入或移除系统的能力。], divider: false)
      #term-index-entry("任务", [调度器管理的执行实体。], divider: false)
      #term-index-entry("任务状态", [任务在运行、可运行、睡眠、停止或退出等阶段中的状态。], divider: false)
      #term-index-entry("日志接收端", [接收并输出内核日志的抽象目标。], divider: false)
      #term-index-entry("软中断让步", [在可延后处理或调度点上主动让出执行机会的行为。], divider: false)
    ]

    #term-index-group("S")[
      #term-index-entry("设备抽象", [把异构硬件能力整理成内核可消费对象的设计边界。], divider: false)
      #term-index-entry("设备发现", [从固件、总线或平台信息中发现设备身份和资源的过程。], divider: false)
      #term-index-entry("设备功能单元", [驱动向内核开放出来的设备能力对象。], divider: false)
      #term-index-entry("设备功能单元注册表", [保存所有已发布设备能力并广播生命周期事件的注册表。], divider: false)
      #term-index-entry("设备号", [用户态兼容视图中用于标识设备文件的编号。], divider: false)
      #term-index-entry("设备模型", [管理设备发现、驱动绑定、能力发布和用户态投影的整体结构。], divider: false)
      #term-index-entry("设备树", [描述平台硬件层次、资源和属性的树形数据结构。], divider: false)
      #term-index-entry("设备文件", [把设备能力投影到文件系统名字空间后的文件节点。], divider: false)
      #term-index-entry("设备文件投影器", [把设备能力转换为用户可见设备节点的组件。], divider: false)
      #term-index-entry("设备移除", [设备退出系统时阻止新访问、排空旧请求并释放资源的过程。], divider: false)
      #term-index-entry("实时调度类", [按照实时优先级选择任务的调度类。], divider: false)
      #term-index-entry("实时时钟", [提供日历时间、告警或周期中断能力的硬件或虚拟设备。], divider: false)
      #term-index-entry("受管堆", [提供受管对象分配和垃圾回收能力的堆。], divider: false)
      #term-index-entry("受管对象", [由受管堆跟踪并参与自动回收的对象。], divider: false)
      #term-index-entry("数据包", [网络设备和协议栈传递的包级数据单位。], divider: false)
      #term-index-entry("数据报套接字", [保持消息边界的数据报通信端点。], divider: false)
      #term-index-entry("输入输出", [系统在设备、文件、套接字或用户缓冲区之间传递数据的操作。], divider: false)
      #term-index-entry("双端队列", [允许在两端插入和移除元素的队列结构。], divider: false)
      #term-index-entry("睡眠状态", [任务因等待事件而暂时不可运行的状态。], divider: false)
      #term-index-entry("私有待处理信号", [只投递给某个具体任务的待处理信号。], divider: false)
      #term-index-entry("私有文件映射", [文件内容以私有方式映射到地址空间，写入时通常触发复制。], divider: false)
      #term-index-entry("松弛语义", [不建立额外同步顺序的原子内存序。], divider: false)
      #term-index-entry("随机访问内存", [可按地址随机读写的主存。], divider: false)
      #term-index-entry("索引节点", [表示文件元数据的文件系统对象，与文件名分离。], divider: false)
    ]

    #term-index-group("T")[
      #term-index-entry("特殊设备文件系统", [自动投影设备节点的特殊文件系统。], divider: false)
      #term-index-entry("提升", [分代回收中把长期存活对象移入更老代的过程。], divider: false)
      #term-index-entry("条件变量", [线程等待某个条件变化并由其他线程唤醒的同步原语。], divider: false)
      #term-index-entry("同步等待", [发起请求后等待完成结果的执行模式。], divider: false)
      #term-index-entry("通用异步收发器", [串口控制器常见硬件类型。], divider: false)
      #term-index-entry("图形设备", [面向显示输出的设备类别。], divider: false)
    ]

    #term-index-group("W")[
      #term-index-entry("外部碎片", [空闲空间被分割成不连续小块后难以满足大块请求的现象。], divider: false)
      #term-index-entry("完全公平调度器", [以虚拟运行时间为核心的公平调度算法。], divider: false)
      #term-index-entry("位置无关可执行文件", [装载位置可以变化的可执行文件形式。], divider: false)
      #term-index-entry("文件后备", [虚拟内存区域背后由文件内容提供数据的后备关系。], divider: false)
      #term-index-entry("文件描述符", [用户态通过整数句柄引用打开文件对象的方式。], divider: false)
      #term-index-entry("文件描述符表", [进程或线程组保存文件描述符到文件对象映射的表。], divider: false)
      #term-index-entry("文件偏移", [文件对象中记录顺序读写当前位置的值。], divider: false)
      #term-index-entry("文件系统用户标识符", [文件系统权限检查时使用的用户身份。], divider: false)
      #term-index-entry("文件系统用户组标识符", [文件系统权限检查时使用的组身份。], divider: false)
      #term-index-entry("文件页", [由文件内容填充或回写的页。], divider: false)
      #term-index-entry("物理地址", [处理器和设备访问物理内存或设备窗口使用的机器地址。], divider: false)
      #term-index-entry("物理页", [物理内存管理中的页粒度单位。], divider: false)
      #term-index-entry("物理页帧", [页大小对齐的物理内存帧。], divider: false)
      #term-index-entry("无锁读取", [不通过传统互斥锁完成读取的并发访问方式。], divider: false)
    ]

    #term-index-group("X")[
      #term-index-entry("系统调用", [用户态进入内核请求服务的接口。], divider: false)
      #term-index-entry("系统调用表", [根据系统调用号分发到具体处理函数的表。], divider: false)
      #term-index-entry("系统调用入口", [处理器从用户态进入内核执行系统调用的入口路径。], divider: false)
      #term-index-entry("系统调用上下文", [系统调用路径传递参数、返回值和任务状态的上下文。], divider: false)
      #term-index-entry("系统文件系统", [向用户态投影内核对象属性的特殊文件系统。], divider: false)
      #term-index-entry("线程", [与同组线程共享部分资源的可调度执行实体。], divider: false)
      #term-index-entry("线程标识符", [内核和用户态用于指代线程的整数名称。], divider: false)
      #term-index-entry("线程局部存储", [每个线程独立保存的数据区域。], divider: false)
      #term-index-entry("线程组", [共享进程级资源的一组线程。], divider: false)
      #term-index-entry("写时复制", [多个对象共享同一物理后备，首次写入时再复制的机制。], divider: false)
      #term-index-entry("信号", [内核向任务或线程组传递异步事件的机制。], divider: false)
      #term-index-entry("信号动作", [指定信号处理函数、掩码和标志的配置。], divider: false)
      #term-index-entry("信号返回", [用户态信号处理结束后恢复原上下文的过程。], divider: false)
      #term-index-entry("信号屏蔽字", [控制哪些信号暂时不被递送的掩码。], divider: false)
      #term-index-entry("信号帧", [内核在用户栈上构造的信号处理返回上下文。], divider: false)
      #term-index-entry("虚拟地址", [程序或内核通过地址空间看到的地址。], divider: false)
      #term-index-entry("虚拟地址空间", [由多个虚拟地址范围组成的地址视图。], divider: false)
      #term-index-entry("虚拟动态共享对象", [内核映射到用户态的只读辅助代码或数据对象。], divider: false)
      #term-index-entry("虚拟队列", [虚拟设备规范中用于前后端传递描述符的队列。], divider: false)
      #term-index-entry("虚拟截止时间", [公平调度中用于排序任务的虚拟时间目标。], divider: false)
      #term-index-entry("虚拟内存", [通过页表把虚拟地址映射到物理后备的内存机制。], divider: false)
      #term-index-entry("虚拟内存区域", [用户地址空间中一段权限、后备和语义一致的区域。], divider: false)
      #term-index-entry("虚拟文件系统", [把不同文件系统、设备文件和套接字统一为文件对象接口的抽象层。], divider: false)
    ]

    #term-index-group("Y")[
      #term-index-entry("页表", [保存虚拟地址到物理地址映射关系的数据结构。], divider: false)
      #term-index-entry("页表项", [页表中描述某一级映射、权限和物理地址的条目。], divider: false)
      #term-index-entry("页缓存", [缓存文件页内容以加速文件访问和内存映射的结构。], divider: false)
      #term-index-entry("硬件抽象层", [把架构相关能力整理为上层可消费接口的抽象层。], divider: false)
      #term-index-entry("应用程序接口", [程序通过函数、系统调用或协议消费服务的接口。], divider: false)
      #term-index-entry("应用二进制接口", [规定二进制程序与内核、运行库和处理器约定之间交互方式的接口边界。], divider: false)
      #term-index-entry("用户地址", [用户地址空间中的虚拟地址。], divider: false)
      #term-index-entry("用户地址空间", [用户进程可见的虚拟地址空间。], divider: false)
      #term-index-entry("用户复制", [内核与用户地址空间之间复制数据的过程。], divider: false)
      #term-index-entry("用户上下文", [用户态寄存器、栈和信号恢复所需的上下文信息。], divider: false)
      #term-index-entry("用户态", [处理器以非特权级执行用户程序的状态。], divider: false)
      #term-index-entry("用户态投影", [把内核对象映射为用户可见文件、属性或节点的过程。], divider: false)
      #term-index-entry("用户指针", [用户态传入内核、需要安全访问检查的地址。], divider: false)
      #term-index-entry("原始套接字", [允许用户态直接构造或接收低层协议负载的套接字。], divider: false)
      #term-index-entry("运行队列", [调度器保存可运行任务并执行选择的队列结构。], divider: false)
      #term-index-entry("运行期", [内核基础能力建立之后的长期执行阶段。], divider: false)
      #term-index-entry("运行时前固件服务", [固件退出启动服务前提供的一组服务。], divider: false)
      #term-index-entry("运行状态", [任务当前正在处理器上执行的状态。], divider: false)
      #term-index-entry("运行资源限制", [约束进程可使用文件数、内存或其他资源的限制。], divider: false)
      #term-index-entry("运行资源用量", [进程或线程累计消耗的时间和资源统计。], divider: false)
    ]

    #term-index-group("Z")[
      #term-index-entry("脏卡", [垃圾回收中记录某个内存卡片可能包含跨代引用的标记。], divider: false)
      #term-index-entry("早期入口", [处理器刚进入内核镜像时执行的最早代码路径。], divider: false)
      #term-index-entry("粘滞位", [文件系统权限中的特殊模式位。], divider: false)
      #term-index-entry("栈指针", [指向当前栈顶或栈操作位置的寄存器值。], divider: false)
      #term-index-entry("直接内存访问", [设备在不经过处理器逐字节搬运的情况下访问内存的机制。], divider: false)
      #term-index-entry("直接映射", [将物理内存按固定偏移映射到内核虚拟地址空间的方式。], divider: false)
      #term-index-entry("直接映射窗口", [部分架构中用于建立固定地址映射的硬件窗口。], divider: false)
      #term-index-entry("执行时关闭", [程序执行替换时自动关闭指定文件描述符的标志。], divider: false)
      #term-index-entry("执行替换", [用新程序映像替换当前进程地址空间和用户态入口的过程。], divider: false)
      #term-index-entry("终端行规程", [在底层字符设备与用户读写之间处理回显、规范模式和控制字符的层。], divider: false)
      #term-index-entry("中断控制器", [接收、仲裁并向处理器投递中断的硬件或虚拟组件。], divider: false)
      #term-index-entry("中断请求", [设备或控制器向处理器请求中断处理的信号。], divider: false)
      #term-index-entry("终端设备", [面向字符交互、控制字符和作业控制的设备抽象。], divider: false)
      #term-index-entry("中央处理器", [执行指令和处理异常的处理器核心。], divider: false)
      #term-index-entry("注册表", [保存对象集合并提供查找、注册和注销能力的数据结构。], divider: false)
      #term-index-entry("自旋锁", [在等待期间忙等的同步原语。], divider: false)
      #term-index-entry("资源所有权", [内核对象或驱动对设备、内存或中断资源的占有关系。], divider: false)
      #term-index-entry("资源限制", [限制进程可使用资源数量或规模的机制。], divider: false)
      #term-index-entry("阻塞模式", [操作无法立即完成时允许任务睡眠等待的模式。], divider: false)
      #term-index-entry("最大传输单元", [网络接口一次可传输的最大数据单元。], divider: false)
      #term-index-entry("最早合格虚拟截止时间优先", [结合合格性和虚拟截止时间的公平调度算法。], divider: false)
      #term-index-entry("最早截止时间优先", [每次选择截止时间最早任务的调度算法。], divider: false)
      #term-index-entry("作业控制", [终端、进程组和信号协同管理前后台作业的机制。], divider: false)
    ]
  ]
)

== 缩写词表

#table(
  columns: (0.9fr, 2.2fr, 1.8fr),
  inset: 6pt,
  table.header(
    table.cell(fill: panel-fill)[#text(weight: "bold")[英文缩写]],
    table.cell(fill: panel-fill)[#text(weight: "bold")[英文全称]],
    table.cell(fill: panel-fill)[#text(weight: "bold")[中文名称]],
  ),
  [ABI], [Application Binary Interface], [应用二进制接口],
  [ACPI], [Advanced Configuration and Power Interface], [高级配置与电源接口],
  [API], [Application Programming Interface], [应用程序接口],
  [ASID], [Address Space Identifier], [地址空间标识符],
  [ASLR], [Address Space Layout Randomization], [地址空间布局随机化],
  [BAR], [Base Address Register], [基址寄存器],
  [BSS], [Block Started by Symbol], [未初始化静态区],
  [CAS], [Compare-and-Swap], [比较并交换],
  [CFS], [Completely Fair Scheduler], [完全公平调度器],
  [COW], [Copy-on-Write], [写时复制],
  [CPU], [Central Processing Unit], [中央处理器],
  [CSR], [Control and Status Register], [控制与状态寄存器],
  [DL], [Deadline], [截止时间调度类],
  [DMA], [Direct Memory Access], [直接内存访问],
  [DMW], [Direct Mapping Window], [直接映射窗口],
  [DTB], [Device Tree Blob], [扁平设备树二进制],
  [EDF], [Earliest Deadline First], [最早截止时间优先],
  [EEVDF], [Earliest Eligible Virtual Deadline First], [最早合格虚拟截止时间优先],
  [EFI], [Extensible Firmware Interface], [可扩展固件接口],
  [ELF], [Executable and Linkable Format], [可执行与可链接格式],
  [EOF], [End of File], [文件结束标记],
  [FDT], [Flattened Device Tree], [扁平设备树],
  [FIFO], [First In, First Out], [先进先出],
  [FPU], [Floating-Point Unit], [浮点单元],
  [GC], [Garbage Collection], [垃圾回收],
  [GID], [Group Identifier], [组标识符],
  [HAL], [Hardware Abstraction Layer], [硬件抽象层],
  [ICMP], [Internet Control Message Protocol], [互联网控制消息协议],
  [IPC], [Inter-Process Communication], [进程间通信],
  [I/O], [Input/Output], [输入输出],
  [IP], [Internet Protocol], [互联网协议],
  [IPv4], [Internet Protocol Version 4], [互联网协议第四版],
  [IPv6], [Internet Protocol Version 6], [互联网协议第六版],
  [IRQ], [Interrupt Request], [中断请求],
  [LSM], [Linux Security Module], [Linux 安全模块],
  [MAC], [Media Access Control], [介质访问控制],
  [MADT], [Multiple APIC Description Table], [多 APIC 描述表],
  [MMIO], [Memory-Mapped Input/Output], [内存映射输入输出],
  [MSI], [Message Signaled Interrupts], [消息信号中断],
  [MTU], [Maximum Transmission Unit], [最大传输单元],
  [NAPI], [New API], [网络轮询接口],
  [NUMA], [Non-Uniform Memory Access], [非一致内存访问],
  [NUL], [Null Character], [空字符],
  [PC], [Program Counter], [程序计数器],
  [PCI], [Peripheral Component Interconnect], [外设组件互连],
  [PCIe], [PCI Express], [高速外设组件互连],
  [PGID], [Process Group Identifier], [进程组标识符],
  [PID], [Process Identifier], [进程标识符],
  [PIE], [Position-Independent Executable], [位置无关可执行文件],
  [POSIX], [Portable Operating System Interface], [可移植操作系统接口],
  [PTY], [Pseudo Terminal], [伪终端],
  [RAM], [Random Access Memory], [随机访问内存],
  [RSDP], [Root System Description Pointer], [根系统描述指针],
  [RT], [Real-Time], [实时调度类],
  [RTC], [Real-Time Clock], [实时时钟],
  [SMP], [Symmetric Multiprocessing], [对称多处理],
  [SP], [Stack Pointer], [栈指针],
  [SPCR], [Serial Port Console Redirection Table], [串口控制台重定向表],
  [TCP], [Transmission Control Protocol], [传输控制协议],
  [TGID], [Thread Group Identifier], [线程组标识符],
  [TID], [Thread Identifier], [线程标识符],
  [TLB], [Translation Lookaside Buffer], [地址转换后备缓冲],
  [TLS], [Thread Local Storage], [线程局部存储],
  [TOCTOU], [Time-of-Check to Time-of-Use], [检查时到使用时竞态],
  [TTY], [Teletype], [终端设备],
  [UART], [Universal Asynchronous Receiver/Transmitter], [通用异步收发器],
  [UDP], [User Datagram Protocol], [用户数据报协议],
  [UAPI], [User API], [用户态接口],
  [UID], [User Identifier], [用户标识符],
  [VFS], [Virtual File System], [虚拟文件系统],
  [VM], [Virtual Memory], [虚拟内存],
  [VMA], [Virtual Memory Area], [虚拟内存区域],
  [W^X], [Write XOR Execute], [写异或执行],
)
