#import "../config.typ": project-name
#import "../styles/diagram.typ": flow-arrow, flow-node, layer-card
#import "../styles/figure.typ": continued-table, figure-caption, pseudo-sample
#import "../styles/tokens.typ": handoff-fill, soft-fill, stable-fill, warm-fill

= 第五章 进程与线程管理

在第四章中，VFS 通过 VFS 上下文和文件描述符表为用户程序提供了稳定的文件系统视图。文件描述符、当前目录、挂载命名空间和凭据都需要依附在某个执行实体上。内存管理中的用户虚拟地址空间、信号处理中的阻塞掩码、调度器中的运行状态也有同样需求。本章讨论的进程与线程管理子系统，正是这些资源的组织中心。它需要回答三个问题。谁在运行。它拥有哪些资源。它如何创建、退出、等待和接收异步事件。

传统叙述常把进程和线程分成两套对象。我们在实现中采用统一的任务对象（`Task`）抽象。任务对象是调度器管理的最小实体，也是 POSIX 线程语义的落点。它可以代表一个拥有独立地址空间和文件表的进程，也可以代表共享地址空间和信号处理表的线程。二者的区别不由结构体类型决定，而由 `clone` 时选择共享哪些资源决定。这个设计使调度、等待、信号和退出都围绕同一个生命周期状态机展开。

统一任务对象以后，还需要处理 POSIX ABI 对整数 PID 的要求。内核内部不把 PID 作为任务主身份。任务身份由任务强引用（`Arc<Task>`）表达，父子关系、等待队列、信号投递和调度队列都围绕引用对象运行。PID 命名层只在系统调用和 procfs 进程文件系统等 ABI 边界提供整数名字。这个分层让热路径避开全局 PID 查找，也为后续 PID 命名空间留出空间。

== 5.1 设计目标与约束

进程与线程管理的约束来自三条路径。第一条是调度路径。调度器需要快速读取状态、切换内核上下文并唤醒任务，不能被 VFS、虚拟内存或 PID 命名细节拖慢。第二条是 POSIX 兼容路径。`fork`、`clone`、`waitpid`、`kill`、`setuid` 和信号处理都要求稳定的 ABI 语义。第三条是资源生命周期路径。任务退出时，虚拟内存、文件表、健壮 futex 机制、`clear_child_tid` 地址、PID、线程组和父子列表必须按固定顺序收束。

#continued-table(
  "5-1",
  [进程与线程管理的设计目标],
  (1.05fr, 2.1fr, 2.2fr),
  (
    table.cell(fill: soft-fill)[#text(weight: "bold")[目标]],
    table.cell(fill: soft-fill)[#text(weight: "bold")[设计含义]],
    table.cell(fill: soft-fill)[#text(weight: "bold")[关键约束]],
  ),
  (
    table.cell(fill: warm-fill)[统一执行实体],
    table.cell(fill: warm-fill)[任务对象同时承载进程和线程语义。],
    table.cell(fill: warm-fill)[调度器只面对任务对象，资源共享由 `clone` 标志和侧表钩子决定。],
    table.cell(fill: soft-fill)[PID 边界后移],
    table.cell(fill: soft-fill)[整数 PID 是 ABI 名字，内部身份由任务强引用表达。],
    table.cell(fill: soft-fill)[PID 登记表保存弱引用，不能反向保活任务。],
    table.cell(fill: handoff-fill)[生命周期分阶段],
    table.cell(fill: handoff-fill)[退出、僵尸状态、`wait` 回收和最终释放分开处理。],
    table.cell(fill: handoff-fill)[退出前钩子必须早于虚拟内存和文件描述符表清理，`wait` 可见状态必须保留到父回收。],
    table.cell(fill: stable-fill)[跨子系统解耦],
    table.cell(fill: stable-fill)[VFS、文件描述符表、用户虚拟地址空间和架构状态通过任务扩展接入。],
    table.cell(fill: stable-fill)[调度核心不直接依赖 VFS 和内存管理的内部结构。],
  ),
  kind: "表",
  continuation-kind: "续表",
  align: (left, left, left),
)

这些目标决定了任务对象不能成为所有子系统字段的简单堆叠。若把 VFS、虚拟内存、套接字、`ptrace` 和架构扩展全部写成固定字段，调度 crate 会依赖大量上层模块。若完全使用动态表，系统调用热路径又会因为频繁查找扩展项而变慢。当前实现采用折中方案。固定热槽位保存高频扩展，低频扩展继续使用类型擦除表。这样既保留了可扩展性，也避免 `read`、`write`、`mmap` 等热路径每次都锁住扩展向量。

== 5.2 任务对象核心身份模型

任务对象的主身份是引用计数对象。父任务的子列表持有子任务强引用，运行队列和当前 CPU 也会在调度期间持有强引用。等待队列只保存弱引用。任务退出后进入僵尸状态，仍留在父任务的子列表中，直到父任务执行 `wait` 回收。父任务先退出时，子任务被移交给 init 任务。这个关系使僵尸状态可观察，也避免等待队列保活已经退出的任务。

#pseudo-sample("5-1", [任务对象的核心字段], kind: "代码")[
  ```rust
  enum TaskState {
      New,
      Runnable,
      Running,
      Sleeping,
      Uninterruptible,
      Stopped,
      Continued,
      Zombie,
      Dead,
  }

  enum TaskKind {
      User,
      KernelThread,
      Idle,
  }

  struct Relations {
      parent: Weak<Task>,
      children: Vec<Arc<Task>>,
      thread_group: Arc<ThreadGroup>,
      process_group: Arc<ProcessGroup>,
      pid_in_ns: Vec<(Arc<PidNamespace>, PidT)>,
  }

  struct Task {
      sched: SchedEntity,
      kind: AtomicU8,
      state: AtomicU8,
      rel: Spinlock<Relations>,
      exit_waiters: WaitQueue,
      signal: SignalState,
      shared_signal: Spinlock<Arc<SharedSignal>>,
      creds: Spinlock<Arc<Credentials>>,
      hot_ext: HotTaskExt,
      ext: Spinlock<Vec<TaskExt>>,
  }
  ```
]

亲缘关系集中在一把锁下。父指针、子列表、线程组、进程组和 PID 登记副本同时受到保护。这个粒度看起来偏粗，但它避免了父任务锁、子任务锁、线程组锁和 PID 锁之间的反序风险。亲缘关系操作本身并非系统调用热路径。`fork`、`wait`、`setpgid` 和重新收养的频率远低于调度节拍和文件 I/O。我们更重视这一部分的可推理性。

任务状态由原子值保存。唤醒路径可以通过比较交换操作把睡眠状态或不可中断睡眠状态转为可运行状态，避免每次读取状态都进入运行队列锁。状态机限制了不合法转换。僵尸任务不会被唤醒回可运行状态。死亡任务只等待最后引用释放。停止状态和继续状态则服务于作业控制和 `wait` 可观察事件。

== 5.3 PID 命名层、线程组与进程组

PID 命名层把任务强引用映射为整数 PID。每个 PID 命名空间（`PidNamespace`）维护自己的登记表，槽位中保存弱引用。弱引用失效后，后续查找会自然失败，父进程回收僵尸任务时再归还 PID。任务内部保存自己在各命名空间中的 PID 副本，根命名空间的 PID 对应当前系统调用可见的 `getpid`、`kill` 和 `waitpid` 语义。

#pseudo-sample("5-2", [PID 命名层的边界], kind: "代码")[
  ```rust
  struct PidNamespace {
      parent: Option<Arc<PidNamespace>>,
      level: u32,
      registry: PidRegistry,
  }

  struct PidSlot {
      task: Weak<Task>,
      generation: u32,
  }

  fn resolve_pid(ns: &PidNamespace, pid: PidT) -> Option<Arc<Task>> {
      let weak = ns.registry.lookup(pid)?;
      weak.upgrade()
  }

  fn task_pid_in_ns(task: &Task, ns: &Arc<PidNamespace>) -> Option<PidT> {
      let rel = task.rel.lock();
      rel.pid_in_ns
          .iter()
          .find(|(entry_ns, _)| Arc::ptr_eq(entry_ns, ns))
          .map(|(_, pid)| *pid)
  }
  ```
]

线程组和进程组是独立对象。线程组表达共享信号处理语义，组内成员共享共享信号状态（`SharedSignal`），包括信号动作表和共享待处理队列。进程组服务于作业控制。终端向前台进程组发送信号时，内核可以遍历进程组成员并投递。会话再组织多个进程组，并保存控制终端等状态。成员索引使用弱引用，避免组对象反向保活任务。

这个结构让 PID、线程组、进程组和会话各自承担一类 ABI 语义。PID 解决命名。线程组解决同一进程内部线程之间的信号共享。进程组解决 shell 作业控制。会话解决登录和控制终端边界。它们关联在任务对象的亲缘锁下，但生命周期不混成同一个概念。

== 5.4 子任务与资源共享

`clone` 是创建新任务的统一入口，`fork` 和 `vfork` 都是它的特例。克隆参数（`CloneArgs`）中的标志与 Linux UAPI 对齐。系统调用层解析用户 ABI 后，把标志、用户栈、TLS、`tid` 地址、`pidfd` 和请求 PID 交给调度层。调度层再根据标志决定父子之间共享哪些资源。

#continued-table(
  "5-2",
  [常用 `clone` 标志与资源语义],
  (1.2fr, 2fr, 2fr),
  (
    table.cell(fill: soft-fill)[#text(weight: "bold")[标志]],
    table.cell(fill: soft-fill)[#text(weight: "bold")[语义]],
    table.cell(fill: soft-fill)[#text(weight: "bold")[交给谁落实]],
  ),
  (
    table.cell(fill: warm-fill)[`CLONE_VM`],
    table.cell(fill: warm-fill)[父子共享用户地址空间。],
    table.cell(fill: warm-fill)[虚拟内存扩展钩子决定共享或 `fork` 写时复制。],
    table.cell(fill: soft-fill)[`CLONE_FS`],
    table.cell(fill: soft-fill)[共享 VFS 上下文。],
    table.cell(fill: soft-fill)[VFS 扩展钩子共享或复制 VFS 上下文。],
    table.cell(fill: handoff-fill)[`CLONE_FILES`],
    table.cell(fill: handoff-fill)[共享文件描述符表。],
    table.cell(fill: handoff-fill)[文件描述符表扩展钩子共享或深拷贝描述符表。],
    table.cell(fill: stable-fill)[`CLONE_SIGHAND`],
    table.cell(fill: stable-fill)[共享信号动作表和共享待处理队列。],
    table.cell(fill: stable-fill)[任务创建路径安装共享信号状态。],
    table.cell(fill: soft-fill)[`CLONE_THREAD`],
    table.cell(fill: soft-fill)[加入父线程组。],
    table.cell(fill: soft-fill)[线程组对象和 PID/TGID 规则共同约束。],
    table.cell(fill: warm-fill)[`CLONE_VFORK`],
    table.cell(fill: warm-fill)[父任务阻塞到子任务 `exec` 或 `exit`。],
    table.cell(fill: warm-fill)[子任务的 `vfork_done` 等待队列负责唤醒父任务。],
  ),
  kind: "表",
  continuation-kind: "续表",
  align: (left, left, left),
)

创建流程可以拆成四步。第一步创建任务对象，建立父子关系、线程组和进程组关系，并在 PID 命名层登记整数名字。第二步复制或共享扩展状态。调度核心不理解 VFS 上下文、文件描述符表和用户虚拟地址空间的内部结构，而是调用内核在启动期注册的任务扩展克隆钩子（`TaskExtCloneHook`）。第三步构造用户上下文。架构层复制或调整陷阱帧，处理子栈、TLS 和子线程标识地址。第四步安装内核执行体，把子任务放入运行队列。

#pseudo-sample("5-3", [`clone` 的分阶段控制流], kind: "代码")[
  ```rust
  fn do_clone(parent: &Arc<Task>, args: CloneArgs) -> Result<PidT, Errno> {
      validate_clone_flags(args.flags)?;

      let groups = choose_groups(parent, args.flags);
      let child = Task::new(default_sched_params(), Arc::downgrade(parent), groups.tg, groups.pg);

      parent.add_child(Arc::clone(&child));
      register_pid_for_child(&child, args.requested_pid)?;

      for ext in parent.snapshot_exts() {
          let cloned = EXT_CLONE_HOOK.clone_for(parent, &child, ext.key, &ext.payload, args.flags);
          child.ext_install(ext.key, cloned);
      }

      process_clone_user_context(parent, &child, args)?;
      child.into_kernel_thread(user_clone_entry, 0);
      enqueue_task(Arc::clone(&child), now_ns_public());

      if args.flags.has(CLONE_VFORK) {
          parent_sleep_until_child_exec_or_exit(&child);
      }

      Ok(child.pid_root().unwrap_or(0))
  }
  ```
]

这里最重要的设计点是拷贝策略外置。VFS 知道 `CLONE_FS` 对当前目录和根目录意味着共享还是复制。文件描述符表知道 `CLONE_FILES` 下描述符项如何保留共享文件对象。内存管理知道 `CLONE_VM` 与 `fork` 写时复制的差异。调度器只负责按标志调用钩子。这个边界避免了调度器随着子系统增加而持续膨胀。

`vfork` 的时序需要单独说明。它让子任务临时共享父地址空间，并让父任务阻塞到子任务 `exec` 或 `exit`。这个阻塞不是普通 `wait`。父任务等待的是子任务释放地址空间共享风险的时刻，而不是等待子任务结束。我们用 `vfork_done` 等待队列表达这个事件。`exec` 和 `exit` 路径都会唤醒父任务，避免父子同时修改共享地址空间。

== 5.5 执行上下文与架构交接

任务对象本身保持架构无关。内核栈由通用层分配，架构上下文缓冲区的大小和对齐由架构上下文接口（`ArchContextOps`）注入。新建内核线程时，通用层分配内核栈（`KernelStack`）和架构上下文槽（`ArchContextSlot`），再调用架构层的 `init_kernel_context` 设置首次恢复入口。上下文切换时，调度器只把两个上下文指针交给 `switch_context` 汇编例程。

#pseudo-sample("5-4", [架构上下文注入], kind: "代码")[
  ```rust
  struct ArchContextOps {
      context_size: usize,
      context_align: usize,
      init_kernel_context: unsafe fn(ctx: NonNull<u8>, stack_top: usize, entry: KernelEntry, arg: usize),
      switch_context: unsafe fn(prev: NonNull<u8>, next: NonNull<u8>),
  }

  fn into_kernel_thread(task: &Arc<Task>, entry: KernelEntry, arg: usize) {
      let stack = KernelStack::new();
      let ctx = ArchContextSlot::new_from_registered_ops();
      let stack_top = stack.top();
      unsafe {
          ARCH_OPS.init_kernel_context(ctx.as_nonnull(), stack_top, entry, arg);
      }
      task.install_execution(stack, ctx);
  }
  ```
]

这种注入方式与第一章的启动上下文和第二章的架构内存接口保持一致。平台相关层负责寄存器布局和汇编切换。平台无关层负责生命周期、状态机和资源关系。这样可以让 RISC-V64 与 LoongArch64 共用任务对象结构和调度逻辑，同时保留各自异常返回、陷阱帧和上下文切换的实现差异。

== 5.6 退出、僵尸状态与 `wait` 回收

任务退出被拆成多个阶段。第一阶段运行退出前钩子。这个阶段仍然保留用户地址空间和文件表，因此可以处理 `CLONE_CHILD_CLEARTID`、健壮 futex 机制和其它必须访问用户地址的清理动作。第二阶段标记退出码和退出原因，把任务状态改为僵尸状态，唤醒等待者，并按退出信号规则通知父任务。第三阶段释放重量级扩展状态，例如用户虚拟地址空间、文件描述符表和 VFS 上下文。第四阶段由父任务执行 `wait` 回收，从子列表中取走僵尸任务，释放 PID，并把任务状态改为死亡状态。

#pseudo-sample("5-5", [退出和 `wait` 的阶段边界], kind: "代码")[
  ```rust
  fn exit_task(task: &Arc<Task>, code: ExitCode) -> ! {
      task.cleanup_before_exit();
      reparent_children_to_init(task);

      mark_task_exited(task, code);
      notify_parent_if_needed(task);
      task.vfork_done.wake_all();

      schedule_away_forever();
  }

  fn reap_child(parent: &Arc<Task>, selector: WaitSelector) -> Result<WaitResult, Errno> {
      loop {
          if let Some(child) = parent.reap_matching(|task| selector.matches(task)) {
              let status = child.exit_wait_status().unwrap();
              release_all_pids(&child);
              child.cleanup_exit_extensions();
              child.retire_execution();
              child.set_state(TaskState::Dead);
              return Ok(WaitResult { child, status });
          }

          if selector.nohang {
              return Err(EAGAIN);
          }

          parent.exit_waiters.wait_event(parent, || has_waitable_child(parent, selector));
      }
  }
  ```
]

僵尸状态的价值在于保留 `wait` 可见信息。父任务可能在子任务退出很久以后才调用 `wait`。若 `exit` 立即释放任务对象，退出码、终止信号、资源用量和 procfs 进程文件系统可见状态都会丢失。若僵尸任务继续保留完整虚拟内存、文件表和内核栈，LTP 中大量 `fork` 和 `exit` 测试会快速积累内存压力。当前实现把轻量状态留在任务对象本体中，把重量级状态交给幂等的退出钩子清理。这样既满足 `wait` 语义，也控制了僵尸任务的资源占用。

父任务先退出时，子任务会被移交给 init 任务。移交过程更新子任务的父弱引用，并把子任务移动到 init 任务的子列表中。这个操作保证所有僵尸任务最终都有可回收者。init 任务的 `wait` 循环因此是进程系统的最后兜底。

== 5.7 等待队列

等待队列是任务阻塞和唤醒的通用原语。它保存等待者的弱引用。准备睡眠时，任务先进入等待队列，再把状态切为睡眠状态或不可中断睡眠状态。真正让出 CPU 前，调用方必须重新检查条件。事件发生时，唤醒者从队列中取出弱引用，升级成功后把任务状态切回可运行状态，并在锁外调用调度器入队。

#pseudo-sample("5-6", [等待队列协议], kind: "代码")[
  ```rust
  struct WaitQueue {
      waiters: Spinlock<VecDeque<Weak<Task>>>,
  }

  fn wait_event(queue: &WaitQueue, task: &Arc<Task>, condition: impl Fn() -> bool) {
      while !condition() {
          queue.enqueue(task);
          task.set_state(TaskState::Sleeping);

          if condition() {
              queue.finish_wait(task);
              return;
          }

          schedule_once(now_ns_public());
          queue.finish_wait(task);
      }
  }

  fn wake_one(queue: &WaitQueue) {
      let picked = queue.pop_upgradeable_waiter();
      if let Some(task) = picked {
          task.cas_state(TaskState::Sleeping, TaskState::Runnable);
          enqueue_task(task, now_ns_public());
      }
  }
  ```
]

弱引用是这里的关键。等待队列不能成为任务生命周期的所有者。否则任务等待自身退出、`poll` 多个对象或超时取消时，队列中的强引用可能让任务迟迟无法释放。弱引用还让过期等待者的清理变得自然。升级失败就跳过。唤醒回调在锁外执行，避免等待队列锁和运行队列锁形成反向依赖。

先入队再切状态的顺序也有明确原因。若先把任务切为睡眠状态，再把它放入队列，事件可能在两步之间发生。唤醒者看到队列为空后返回，任务随后入队并睡眠，唤醒就丢失了。当前协议把登记放在前面，并在让出 CPU 前再次检查条件，从而覆盖事件与睡眠之间的竞态窗口。

== 5.8 信号机制

信号是异步通知机制。发送阶段只负责权限检查和入队。接收阶段发生在目标任务返回用户态前，由调度和陷入返回路径检查待处理信号，再决定默认动作、忽略或构造用户态信号帧。每个任务有私有待处理队列和阻塞掩码。线程组共享共享信号状态，其中保存信号动作表和共享待处理队列。

#pseudo-sample("5-7", [信号状态结构], kind: "代码")[
  ```rust
  struct SignalState {
      pending_bits: AtomicU64,
      pending_infos: Spinlock<Vec<SigInfo>>,
      blocked: AtomicU64,
      saved_blocked: AtomicU64,
      sigtimedwait_mask: AtomicU64,
  }

  struct SharedSignal {
      actions: Spinlock<[SigAction; NSIG]>,
      pending_bits: AtomicU64,
      pending_infos: Spinlock<Vec<SigInfo>>,
  }

  fn send_signal(target: &Arc<Task>, info: SigInfo) -> Result<(), Errno> {
      check_signal_permission(current_task(), target, info.sig)?;
      target.signal.deliver(info);
      wake_if_interruptible_sleep(target);
      Ok(())
  }
  ```
]

`SIGKILL` 和 `SIGSTOP` 有硬性语义，不能被捕获、忽略或屏蔽。普通信号进入待处理队列后，会受到阻塞掩码影响。实时信号和带信号信息的信号需要保存额外载荷，因此实现同时维护位图和信号信息队列（`SigInfo`）。位图用于快速判断是否存在信号，队列用于保存发送者 PID、UID 和用户态信号信息。

线程组共享信号的处理需要结合每个线程的阻塞掩码。一个发送到线程组的信号可以由组内任意未屏蔽该信号的线程处理。共享待处理队列因此不能绑定到某个固定任务。任务返回用户态前，会同时检查私有待处理队列和共享待处理队列，并按自身屏蔽字选择可处理信号。这个设计保持了 POSIX 线程语义，也避免在发送阶段做复杂的线程选择。

== 5.9 凭据与能力

凭据描述任务的权限身份，包括 UID、GID、`fsuid`、`fsgid`、附加组和 Linux capability 能力集合。调度层的凭据对象（`Credentials`）与 VFS 凭据类型保持分离，跨层转换由内核胶合层完成。这样调度 crate 不需要依赖 VFS。VFS 在打开文件时使用同步过去的凭据快照完成权限检查，信号权限检查则使用调度层凭据。

#pseudo-sample("5-8", [凭据快照与整体替换], kind: "代码")[
  ```rust
  struct Credentials {
      uid: Uid,
      euid: Uid,
      suid: Uid,
      fsuid: Uid,
      gid: Gid,
      egid: Gid,
      sgid: Gid,
      fsgid: Gid,
      groups: Vec<Gid>,
      caps: CapSet,
      cap_permitted: CapSet,
      cap_inheritable: CapSet,
      cap_bset: CapSet,
  }

  fn replace_credentials(task: &Arc<Task>, update: impl FnOnce(&Credentials) -> Credentials) {
      let old = task.credentials();
      let new = Arc::new(update(&old));
      task.set_credentials(Arc::clone(&new));
      sync_vfs_credentials(task, new);
  }
  ```
]

凭据采用不可变快照和整体替换。`setuid`、`setgid` 和 `capset` 不在原对象上逐字段修改，而是构造新对象后替换凭据强引用（`Arc<Credentials>`）。这样读者要么看到旧凭据，要么看到新凭据，不会看到 UID 已变但 fsuid 尚未变的中间状态。能力集使用位图表达，权限检查可以通过一次位测试完成。root 凭据携带完整 Linux capability 能力集，普通用户默认没有 Linux capability 能力。

== 5.10 工程设计总结

进程与线程管理子系统把执行身份、资源集合和 POSIX ABI 语义放在同一个生命周期框架内处理。它既要支撑调度器的高频状态切换，也要满足 `fork`、`clone`、`exec`、`wait` 和信号等复杂接口的兼容要求。我们没有把所有资源都写进一个巨大结构，也没有把 PID 作为内部唯一身份，而是把任务对象、PID 命名层、任务扩展、线程组和等待队列拆成可以独立推理的对象。

进程与线程管理子系统具备以下创新。

第一是以任务对象统一进程和线程的执行抽象。这个设计让调度器、等待机制、信号机制和退出机制都只面对一种执行实体。进程和线程的差异被推迟到资源共享层，由 `clone` 标志和扩展钩子决定。早期若采用进程结构体包线程结构体的模型，调度器需要选择线程，`wait` 需要回到进程，信号又要在二者之间转发。当前结构避免了这种双层身份转换。一个任务是否与父共享地址空间、文件表和信号处理表，由创建时的标志决定。任务状态机本身不关心这些差异。这个统一性降低了系统调用路径的分支数量，也让内核线程和空闲任务能够通过任务类型（`TaskKind`）纳入同一调度框架，同时隔离出 POSIX 信号和 `wait` 语义。

第二是把 PID 从内部身份降级为 ABI 兼容层。PID 对用户程序不可或缺，但它不适合作为内核内部的主引用。整数会复用，查找需要全局表，生命周期也容易与任务对象脱节。我们让内部路径传递任务强引用，PID 登记表只保存弱引用。`getpid`、`kill`、`waitpid` 和 procfs 进程文件系统在边界上把整数 PID 翻译为任务引用。这个设计减少了 PID 表锁对内部路径的影响，也避免登记表保活已经退出的任务。更重要的是，它为 PID 命名空间保留了自然结构。任务可以在多层命名空间中拥有不同 PID，调度核心仍然只处理同一个任务对象。

第三是任务扩展钩子把调度核心和资源子系统解耦。VFS、文件描述符表、用户虚拟地址空间、RISC-V 向量状态和信号栈扩展都可以挂在任务上，但调度 crate 不需要知道它们的内部布局。`clone` 时，扩展钩子根据标志决定共享、深拷贝或写时复制。`exit` 时，退出前钩子先处理必须依赖用户地址空间的清理，扩展退出钩子再释放重量级资源。为了避免动态扩展表拖慢系统调用热路径，我们又为 VFS、文件描述符表和用户虚拟地址空间等高频项设置热路径扩展槽位。这个组合同时满足了解耦和性能两个目标。

这些创新共同支撑了进程管理子系统的长期演化能力。新增资源类型可以通过任务扩展接入。新增命名空间语义可以沿 PID 和组对象扩展。新增架构上下文可以通过架构钩子注入。用户态 ABI 仍然看到熟悉的 PID、`wait` 状态、信号和凭据，而内核内部保持引用对象、单向依赖和分阶段生命周期。这个结构把复杂性放在明确的交接点上，使进程与线程管理能够承接 VFS、内存管理和调度系统的共同需求。
