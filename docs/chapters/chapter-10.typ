#import "../config.typ": project-name
#import "../styles/diagram.typ": flow-arrow, layer-card
#import "../styles/figure.typ": continued-table, figure-caption, pseudo-sample
#import "../styles/tokens.typ": handoff-fill, soft-fill, stable-fill, warm-fill

= 第十章 信号与异步事件

在第九章中，用户态执行环境把 ELF 镜像、用户栈和陷入帧准备好，使任务能够进入用户态运行。本章讨论用户态运行过程中最重要的异步机制，信号。信号允许内核或其它任务打断目标任务的正常控制流，要求它终止、停止、继续，或者在用户态执行一个处理函数。它既属于进程管理，也属于系统调用返回路径，还与等待队列和用户栈布局密切相关。

信号机制的难点在于它不是普通函数调用。发送者不能假设接收者正在内核态，也不能等待接收者立即处理。接收者可能屏蔽某个信号，可能正在不可中断睡眠，也可能正在从系统调用返回用户态。线程组又引入共享信号处理动作和共享待处理队列。我们把信号拆成发送、排队、选择、投递和返回五个阶段，让异步事件在可控边界内落地。

== 信号状态模型

每个任务对象拥有私有信号状态（`SignalState`）。其中包含待处理位图、带负载数据的 `SigInfo` 队列、信号屏蔽字、临时保存的信号屏蔽字，以及 `sigtimedwait` 使用的等待屏蔽字。线程组共享信号状态（`SharedSignal`）保存信号动作表和共享待处理队列。这个分层对应 POSIX 线程语义。信号处理动作属于线程组，屏蔽字属于单个线程。

#pseudo-sample("10-1", [信号状态结构], kind: "代码")[
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

  struct SigAction {
      handler: SigHandler,
      mask: SigSet,
      flags: SigActionFlags,
      restorer: usize,
  }
  ```
]

待处理位图用于快速判断是否有信号。`SigInfo` 队列保存发送者 PID、UID、信号码和用户提供的原始负载。标准信号可以合并，实时信号和 `sigqueueinfo` 需要保留更多信息。当前实现使用队列保存具体 `SigInfo`，位图作为快速索引。取出信号时会跳过被信号屏蔽字屏蔽的条目。

== 发送与权限检查

信号发送入口包括 `kill`、`tkill`、`tgkill`、`rt_sigqueueinfo` 和 `pidfd_send_signal`。目标可以由 PID、线程 ID 或 pidfd 文件描述符表示。发送阶段首先解析目标任务或线程组，然后检查权限，再把 `SigInfo` 放入目标的私有待处理队列或共享待处理队列。若目标处于可中断睡眠，发送路径会尝试唤醒它。

#continued-table(
  "10-1",
  [信号发送入口],
  (1.2fr, 2fr, 2fr),
  (
    table.cell(fill: soft-fill)[#text(weight: "bold")[入口]],
    table.cell(fill: soft-fill)[#text(weight: "bold")[目标选择]],
    table.cell(fill: soft-fill)[#text(weight: "bold")[特点]],
  ),
  (
    table.cell(fill: warm-fill)[`kill`],
    table.cell(fill: warm-fill)[按进程或进程组发送。],
    table.cell(fill: warm-fill)[适合 shell 作业控制和普通进程终止。],
    table.cell(fill: soft-fill)[`tkill` / `tgkill`],
    table.cell(fill: soft-fill)[按线程 ID 或线程组加线程 ID 精确发送。],
    table.cell(fill: soft-fill)[用于 POSIX 线程和调试相关场景。],
    table.cell(fill: handoff-fill)[`rt_sigqueueinfo`],
    table.cell(fill: handoff-fill)[按 PID 发送带信号信息的信号。],
    table.cell(fill: handoff-fill)[需要从用户态复制 128 字节信号信息负载。],
    table.cell(fill: stable-fill)[`pidfd_send_signal`],
    table.cell(fill: stable-fill)[通过 pidfd 文件对象找到目标任务。],
    table.cell(fill: stable-fill)[避免 PID 复用带来的名字竞态。],
  ),
  kind: "表",
  continuation-kind: "续表",
  align: (left, left, left),
)

权限检查依赖发送者和接收者的凭据。第五章已经说明，调度层凭据与 VFS 凭据分离。信号权限使用调度层凭据中的 UID 和 capability 权限。`SIGKILL` 和 `SIGSTOP` 拥有特殊语义，不能被捕获、忽略或屏蔽。发送阶段可以直接记录强制动作，后续投递阶段会执行默认处理。

== 屏蔽、等待与信号动作

`rt_sigaction` 修改线程组共享的动作表。`rt_sigprocmask` 修改当前任务的信号屏蔽字，并自动剥离不可屏蔽的 `SIGKILL` 与 `SIGSTOP`。`rt_sigpending` 返回当前待处理信号与屏蔽信号的组合视图。`rt_sigsuspend` 临时替换信号屏蔽字，睡眠直到某个信号到来，然后恢复旧屏蔽字并返回 `EINTR`。`rt_sigtimedwait` 则允许用户态同步等待一组信号。

#pseudo-sample("10-2", [sigsuspend 的控制流], kind: "代码")[
  ```rust
  fn rt_sigsuspend(mask: SigSet) -> Result<usize, Errno> {
      let task = current_task();
      task.signal.save_blocked(mask);

      loop {
          if sigpending().raw() != 0 {
              break;
          }
          task.cas_state(TaskState::Running, TaskState::Sleeping);
          sched_yield();
      }

      task.signal.restore_blocked();
      Err(EINTR)
  }
  ```
]

这类调用体现了信号的双重性质。信号既可以异步打断任务，也可以被任务同步等待。同步等待不能绕过待处理队列，因为信号仍然需要保留发送者信息和屏蔽语义。`sigtimedwait` 先非阻塞轮询，未命中时登记等待屏蔽字并让出 CPU，到期后返回 `EAGAIN`。这种做法避免忙等，也使信号等待与第七章的等待队列模型一致。

== 返回用户态前的投递

真正执行信号动作发生在任务返回用户态前。系统调用分发器、异常返回路径和调度边界都会检查待处理信号。若默认动作是终止，调度层调用退出路径。若默认动作是停止，任务进入停止状态并通知父任务。若动作是用户处理函数，内核在用户栈或备用信号栈上构造信号帧，修改陷入帧，使用户态返回到处理函数。

#pseudo-sample("10-3", [信号投递阶段], kind: "代码")[
  ```rust
  fn deliver_pending_signals(task: &Arc<Task>, ctx: UserContextRef) -> Result<(), Errno> {
      while let Some(info) = dequeue_unblocked_signal(task) {
          let action = task.shared_signal().get_action(info.sig);
          match action.handler {
              SigHandler::Default => run_default_action(task, info.sig),
              SigHandler::Ignore => continue,
              SigHandler::Handler(entry) => {
                  setup_signal_frame(task, info, action, ctx)?;
                  return Ok(());
              }
          }
      }
      Ok(())
  }
  ```
]

用户态处理函数的关键是信号帧。信号帧保存原陷入帧、旧信号屏蔽字、信号信息和返回跳板。处理函数执行完成后调用 `rt_sigreturn`，内核从用户栈恢复原上下文。`sigaltstack` 允许任务指定备用信号栈。当前实现会检查当前栈指针是否已经在备用栈内，避免在处理信号时非法切换备用栈。

== 信号与系统调用重启

阻塞系统调用被信号打断时，用户态通常看到 `EINTR`，某些带 `SA_RESTART` 的信号可以触发重启语义。当前系统调用分发器在看到 `EINTR` 返回时，会尝试消费可投递信号并构造信号帧。`restart_syscall` 作为兼容入口保留。这个设计使大多数系统调用实现只需返回错误码，不需要直接管理信号帧。

这里的边界很重要。系统调用实现负责自己的业务状态。信号投递负责改变用户态控制流。分发器负责在返回前把两者接起来。若每个阻塞系统调用都自行处理信号，管道、套接字、futex 机制、`nanosleep` 和等待接口都会重复一套复杂逻辑。集中处理降低了错误概率。

== 默认动作与任务状态转换

信号的默认动作不只是终止进程。POSIX 信号可以终止、产生核心转储语义、停止、继续或忽略。`SIGKILL` 必须终止，`SIGSTOP` 必须停止，二者不能被用户处理函数捕获，也不能被信号屏蔽字屏蔽。`SIGCHLD` 默认忽略，但它仍然承担父子进程状态通知功能。`SIGCONT` 会让已停止任务继续运行，并可能清理相反方向的停止信号。默认动作因此需要和任务状态机、父子等待和进程组控制结合。

任务终止动作进入退出路径。信号投递阶段发现默认动作为终止时，不能简单设置一个标志后继续返回用户态。它必须调用进程退出流程，关闭资源，通知父任务，唤醒等待队列，并根据线程组语义决定是否退出整个进程。停止动作则把任务或线程组置为停止状态，并向父任务发布可等待状态。继续动作把停止状态恢复为可运行状态，并通知等待者。每个动作都需要穿过第七章的等待队列和第六章的调度状态。

#continued-table(
  "10-2",
  [典型默认动作与内核处理],
  (1.1fr, 1.8fr, 2.4fr),
  (
    table.cell(fill: soft-fill)[#text(weight: "bold")[动作]],
    table.cell(fill: soft-fill)[#text(weight: "bold")[典型信号]],
    table.cell(fill: soft-fill)[#text(weight: "bold")[处理要点]],
  ),
  (
    table.cell(fill: warm-fill)[终止],
    table.cell(fill: warm-fill)[`SIGKILL`、`SIGTERM`、`SIGSEGV`。],
    table.cell(fill: warm-fill)[进入退出路径，通知父任务，释放资源并停止返回用户态。],
    table.cell(fill: soft-fill)[停止],
    table.cell(fill: soft-fill)[`SIGSTOP`、`SIGTSTP`、`SIGTTIN`。],
    table.cell(fill: soft-fill)[设置停止状态，触发等待接口可观察事件，等待继续信号。],
    table.cell(fill: handoff-fill)[继续],
    table.cell(fill: handoff-fill)[`SIGCONT`。],
    table.cell(fill: handoff-fill)[清理停止状态，唤醒可运行任务，并通知父任务。],
    table.cell(fill: stable-fill)[忽略],
    table.cell(fill: stable-fill)[部分 `SIGCHLD` 和用户设置忽略的信号。],
    table.cell(fill: stable-fill)[从待处理队列中消费，不构造用户信号帧。],
    table.cell(fill: warm-fill)[捕获],
    table.cell(fill: warm-fill)[用户通过 `rt_sigaction` 注册处理函数的信号。],
    table.cell(fill: warm-fill)[构造信号帧，修改陷入帧，返回用户处理函数。],
  ),
  kind: "表",
  continuation-kind: "续表",
  align: (left, left, left),
)

默认动作与父子等待之间存在细节。父任务调用 `wait4` 时，可能等待子任务退出，也可能在指定标志下观察停止或继续状态。子任务收到停止信号后，需要把状态变化发布给父任务。收到继续信号后，也可能发布继续事件。若这些事件只修改任务状态而不唤醒父任务，等待接口会长时间阻塞。若重复发布，父任务可能看到重复状态。我们在任务状态中保留可消费的等待事件，使状态变化和等待语义能够对齐。

默认动作还要考虑线程组。向进程发送的终止信号通常影响整个线程组。向特定线程发送的信号可能只投递给该线程，但如果默认动作是进程终止，最终效果仍然是整个进程退出。当前实现以线程组共享信号状态为基础，把进程级动作放在共享层收束。这样后续扩展完整 POSIX 线程语义时，不需要重新设计状态划分。

== 线程组选路与待处理队列

信号发送的目标可能是一个线程，也可能是一个线程组。`tgkill` 可以精确指定线程组和线程 ID。`kill` 面向进程或进程组。终端作业控制会向前台进程组发送信号。pidfd 文件描述符则通过文件对象引用具体任务，避免 PID 复用问题。目标选择阶段必须把这些形式转化为任务对象或共享信号状态上的待处理条目。

线程组信号需要选择一个可投递线程。最理想的目标是未屏蔽该信号、处于可唤醒状态的线程。若当前没有线程可投递，信号应留在共享待处理队列中，等某个线程解除屏蔽或返回用户态时再处理。线程私有信号则放入目标任务的私有待处理队列。投递阶段先检查私有待处理队列，再检查共享待处理队列，或者按内核定义的优先级合并选择。这个过程不能丢失信号信息，也不能让屏蔽信号越过信号屏蔽字。

#pseudo-sample("10-4", [线程组待处理信号的投递选择], kind: "代码")[
  ```rust
  fn pick_pending_signal(task: &Task) -> Option<SigInfo> {
      let blocked = task.signal.blocked.load(Acquire);

      if let Some(info) = task.signal.take_unblocked(blocked) {
          return Some(info);
      }

      let shared = task.thread_group.shared_signal();
      shared.take_unblocked_for_task(blocked)
  }
  ```
]

待处理位图和信号信息队列需要保持一致。位图用于快速判断某个信号是否可能存在。队列保存具体负载。取出最后一个对应信号后，需要清理位图。若位图提前清理，而队列中仍有条目，信号可能迟迟不投递。若队列已经空，位图仍然保留，返回用户态前会反复进入无效检查。我们把队列修改放在锁内，位图作为加速索引同步更新。标准信号合并时，也要确保新的信号信息不破坏已有语义。

实时信号对队列要求更高。标准信号通常可以合并，实时信号需要按顺序排队，并保留用户负载。`rt_sigqueueinfo` 和 `pidfd_send_signal` 都可能携带用户提供的信号信息。当前队列模型为这类语义保留了空间。即使某些实时细节暂未完全覆盖，结构上已经避免把待处理状态简化为单个位图。这个选择的成本是队列管理稍复杂，收益是后续兼容扩展不需要重建信号状态。

== 信号动作语义与用户处理函数

`rt_sigaction` 不是简单设置函数指针。它还包含处理函数类型、附加屏蔽字、标志和恢复入口。处理函数可以是默认、忽略或用户地址。标志会影响投递行为，例如是否传递信号信息、处理函数执行期间是否自动屏蔽当前信号、系统调用是否尝试重启、是否使用备用信号栈。恢复入口通常由 C 运行库提供，用于处理函数返回后执行 `rt_sigreturn`。

修改信号动作时使用线程组共享锁。读取动作时也要在稳定边界上取得快照。投递阶段不能在持有动作表锁时写用户栈，因为用户栈写入可能触发用户访问错误，也可能需要复杂处理。正确方式是先复制出信号动作，再释放锁，然后构造信号帧。这样动作表锁只保护动作表，不参与用户内存访问和调度路径。

#pseudo-sample("10-5", [信号动作快照与投递分离], kind: "代码")[
  ```rust
  fn deliver_one(task: &Task, info: SigInfo, ctx: UserContextRef) -> Result<(), Errno> {
      let action = {
          let actions = task.shared_signal().actions.lock();
          actions[info.sig as usize].clone()
      };

      match action.handler {
          SigHandler::Default => run_default_action(task, info.sig),
          SigHandler::Ignore => Ok(()),
          SigHandler::Handler(entry) => setup_signal_frame(task, info, action, entry, ctx),
      }
  }
  ```
]

处理函数执行期间的屏蔽字处理需要精确。投递时，内核通常把当前信号和动作附加屏蔽字加入信号屏蔽字，除非设置了对应标志。这样可以避免处理函数被同一信号无限递归打断。旧屏蔽字保存在信号帧中。`rt_sigreturn` 从用户栈恢复旧屏蔽字。若恢复失败，例如用户栈上的信号帧被破坏，内核应向任务发送致命信号或终止进程，因为继续运行的上下文已经不可信。

用户处理函数的入口参数也受 ABI 影响。普通处理函数可能只接收信号编号。带 `SA_SIGINFO` 的处理函数接收信号编号、信号信息指针和用户上下文指针。内核需要在用户栈上按 ABI 布置这些对象，并设置对应寄存器。架构层通过进程映像接口或陷入帧辅助函数完成寄存器设置。信号核心层只表达投递这个信号到这个处理函数，不直接编码每个架构的调用约定。

== sigaltstack 与嵌套投递

备用信号栈用于处理普通用户栈不可用或接近溢出的场景。用户通过 `sigaltstack` 注册一段内存，设置启用或禁用状态。信号动作若带 `SA_ONSTACK`，且当前不在备用栈上，内核应在备用栈上构造信号帧。若当前已经在备用栈上，再次投递通常继续使用当前栈，避免递归切换导致状态混乱。

备用栈检查依赖当前用户栈指针。内核需要判断栈指针是否落在已注册的备用栈区间内。若已经在区间内，`SS_ONSTACK` 状态应反映出来。若信号动作请求备用栈但备用栈未启用或大小不足，内核需要按普通栈或错误策略处理。信号栈本质上仍然是用户内存，写入信号帧时可能失败。失败通常意味着无法安全进入处理函数，任务应按致命信号处理。

#pseudo-sample("10-6", [信号栈选择], kind: "代码")[
  ```rust
  fn choose_signal_stack(task: &Task, action: &SigAction, user_sp: usize) -> usize {
      let alt = task.signal.altstack.load();
      if action.flags.contains(SA_ONSTACK) && alt.enabled && !alt.contains(user_sp) {
          return alt.top_aligned();
      }
      user_sp
  }
  ```
]

嵌套投递需要控制递归深度。用户处理函数执行期间，若又有未屏蔽信号到来，返回用户态前可能再次构造信号帧。合法嵌套是 POSIX 允许的，但无限嵌套会耗尽用户栈。内核不能完全阻止用户通过屏蔽字允许递归信号，但需要确保每次信号帧构造都检查用户地址范围和栈边界。若写入信号帧失败，不能继续假装投递成功。

备用栈还影响调试。若程序栈溢出导致 `SIGSEGV`，普通栈可能已经不可用。启用备用栈后，处理函数有机会运行并输出诊断。内核正确支持 `sigaltstack`，有助于用户态运行库和异常处理框架处理严重错误。它也是动态语言运行时和 sanitizer 检查工具依赖的基础能力之一。

== 信号等待与第七章的同步模型

信号等待建立在第七章的等待队列模型之上，但它多了屏蔽字和待处理队列语义。`rt_sigsuspend` 会临时替换信号屏蔽字，然后睡眠到有未屏蔽信号到来。`rt_sigtimedwait` 等待指定屏蔽字中的信号，并把信号信息复制给用户态。两者都需要避免丢失唤醒。任务改变屏蔽字后必须重新检查待处理信号，登记等待后也要再次检查。

`sigtimedwait` 的返回语义与普通异步投递不同。它同步消费信号，并把信息返回给调用者，不构造用户处理函数。若等待超时，返回 `EAGAIN`。若用户信号信息指针不可写，返回 `EFAULT`。若等待期间收到不在等待屏蔽字中但未屏蔽的其它信号，可能需要按普通信号投递处理。这要求等待路径和返回前投递路径共享同一套待处理信号选择逻辑。

#pseudo-sample("10-7", [sigtimedwait 的等待和消费], kind: "代码")[
  ```rust
  fn rt_sigtimedwait(mask: SigSet, timeout: Option<Time>) -> Result<SigInfo, Errno> {
      let task = current_task();
      loop {
          if let Some(info) = task.signal.take_matching(mask) {
              return Ok(info);
          }
          if timeout_expired(timeout) {
              return Err(Errno::EAGAIN);
          }

          task.signal.set_wait_mask(mask);
          task.signal_wait.prepare_to_wait(&task, TaskState::InterruptibleSleep);
          if let Some(info) = task.signal.take_matching(mask) {
              task.signal_wait.finish_wait(&task);
              return Ok(info);
          }
          schedule_until(timeout);
          task.signal_wait.finish_wait(&task);
      }
  }
  ```
]

发送信号时，如果目标任务正在等待相关屏蔽字，发送路径需要唤醒它。若目标任务正在不可中断睡眠，待处理状态仍然记录，但任务可能要等到睡眠结束才处理。这个行为符合信号语义。信号并不保证立即执行处理函数，它保证事件被记录，并在合适边界被观察。等待队列只提供有信号可能可消费的通知，具体是否匹配屏蔽字仍由醒来后的检查决定。

这个模式也解释了为什么信号不能简单作为普通回调执行。发送者和接收者之间没有直接函数调用关系。发送者只能提交事件并唤醒。接收者在自己的上下文中检查、消费和投递。这样可以避免发送者在持有其它锁时进入目标任务的用户栈构造，也避免跨任务执行流难以推理。

== 系统调用重启的语义边界

系统调用重启是信号机制中最容易产生兼容性差异的部分。阻塞系统调用被信号打断后，用户态可能看到 `EINTR`，也可能在处理函数返回后自动重启。是否重启取决于系统调用类型、信号动作中的 `SA_RESTART`、已经完成的业务进度以及内核返回码。一个已经部分写入数据的 `write` 不应简单重启为全量写。一个尚未发生任何副作用的等待操作可以更容易重启。

我们把系统调用实现和重启框架分开。系统调用实现遇到可打断等待时返回内部错误码或 `EINTR`。分发器在返回前检查待处理信号，构造信号帧，并根据系统调用上下文和信号动作标志决定是否设置重启状态。重启状态可以通过调整程序计数器、保存原参数或使用 `restart_syscall` 入口来表达。具体策略需要和架构陷入帧协作。

重启语义必须谨慎处理副作用。`nanosleep` 被打断时需要返回剩余时间。`poll` 被打断时通常返回 `EINTR`。`read` 若已经读到数据，应返回数据长度。`futex` 等待如果被信号打断，需要区分用户值变化、超时和信号。我们不能把所有 `EINTR` 都机械重启。系统调用实现应当在业务层决定当前点是否可重启，分发器只执行统一框架。

第八章已经说明，返回前收尾是处理信号和重启的安全点。第十章补充的是，信号动作标志和当前系统调用的可重启属性要在这里汇合。这样做能避免每个阻塞系统调用都直接修改用户陷入帧，也能让信号章节统一描述用户处理函数和信号返回的控制流。

== 作业控制与终端信号

终端子系统会产生信号。用户在终端输入中断字符，通常向前台进程组发送 `SIGINT`。输入停止字符会发送 `SIGTSTP`。后台进程读写终端可能收到 `SIGTTIN` 或 `SIGTTOU`。这些信号由 TTY 层根据会话、进程组和控制终端状态生成，并不经过普通 `kill` 入口。第十一章会讨论终端输入泵，本章只说明信号层需要承接这些来源。

作业控制要求信号能够按进程组发送。shell 程序启动管道时，会把多个进程放入同一进程组。终端前台进程组收到控制字符后，整组任务都应收到信号。父 shell 程序通过等待接口观察子进程停止或继续状态，再决定是否恢复提示符。若信号层只支持单个 PID，终端作业控制就无法正常工作。我们在发送入口中保留进程组目标，正是为了支持这一类语义。

停止和继续信号还影响等待接口。父进程可以使用 `WUNTRACED` 观察停止状态，用 `WCONTINUED` 观察继续状态。信号默认动作执行时，必须把这些状态转换转化为等待接口可见事件。否则 shell 程序无法知道前台作业已经暂停，也无法正确实现前台和后台切换。这个交互说明信号不是孤立子系统，它连接终端、调度、进程组和等待语义。

作业控制还要求某些信号具有不可屏蔽或特殊合并行为。`SIGKILL` 与 `SIGSTOP` 不受用户动作控制。`SIGCONT` 到达时会使已停止任务继续，并可能清除待处理的停止类信号。停止类信号到达时，也可能与待处理的继续信号交互。我们在状态模型中把默认动作和待处理队列处理分开，使这些规则可以在投递阶段集中实现。

== 兼容性与运行时语义

信号 ABI 的细节非常密集。`rt_sigaction` 需要正确保存旧动作，`rt_sigprocmask` 需要剥离不可屏蔽信号，`sigsuspend` 应按语义返回 `EINTR`，`sigtimedwait` 需要取出信号信息，`sigaltstack` 需要报告正确状态，`kill`、`tkill` 和 `tgkill` 需要按目标发送。任何一个细节错误都会影响用户态运行库和 shell 程序的行为。

信号语义的困难在于时序。发送信号和接收信号之间可能经历调度。阻塞系统调用可能在信号到来前或到来后进入睡眠。父进程可能在子进程状态变化前调用等待接口，也可能在变化后调用。我们使用待处理队列、等待队列和返回前投递，把这些时序统一成可重复协议。若某个路径记录了待处理状态但没有唤醒，或者唤醒后没有重新检查条件，任务就可能停留在错误状态。

用户态运行库对信号有自己的包装。运行库通常提供恢复入口、信号集封装和 POSIX 线程相关语义。内核需要接受用户传入的动作结构布局，并按 ABI 复制。若结构大小、屏蔽字字节数或标志解释错误，用户态包装层会出现异常。我们在系统调用层使用明确的结构转换，避免把用户结构直接当作内核结构长期保存。

信号兼容性还关系到进程收束能力。若任务处于可打断睡眠却不能被信号唤醒，发送者会看到目标长期不退出。若 `SIGKILL` 被错误屏蔽，系统无法可靠终止异常任务。我们在信号发送阶段对强制信号特殊处理，并在等待路径中让可打断睡眠响应待处理信号。这个能力对系统长期运行非常关键。

== 信号帧与 rt_sigreturn

用户处理函数的执行需要一个可恢复现场。内核投递信号时，不能只把程序计数器改成处理函数地址。处理函数返回后，用户程序应回到信号发生前的位置，寄存器、栈指针和信号屏蔽字都要恢复。这个恢复由信号帧和 `rt_sigreturn` 配合完成。信号帧位于用户栈或备用栈上，保存信号信息、用户上下文、旧屏蔽字和返回跳板需要的信息。处理函数执行完成后跳到恢复入口，恢复入口发起 `rt_sigreturn` 系统调用，内核再从用户栈读取信号帧并恢复陷入帧。

信号帧是内核写入用户内存的数据结构，因此必须经过用户访问辅助函数。写入信号帧时可能失败。失败说明用户栈不可写，或者用户提供的备用栈无效。此时内核无法安全进入处理函数，通常应执行默认致命处理。`rt_sigreturn` 读取信号帧时也可能失败。若用户篡改信号帧造成非法上下文，内核不能盲目恢复到内核地址或无效用户地址。恢复前需要验证程序计数器、栈指针和屏蔽字等字段符合用户态约束。

#pseudo-sample("10-8", [信号帧的构造与恢复], kind: "代码")[
  ```rust
  fn setup_signal_frame(task: &Task, info: SigInfo, action: SigAction, ctx: UserContextRef)
      -> Result<(), Errno>
  {
      let sp = choose_signal_stack(task, &action, ctx.user_sp());
      let frame_addr = allocate_frame_on_user_stack(sp, size_of::<SignalFrame>())?;
      let frame = SignalFrame::from_context(info, action, ctx, task.signal.blocked());

      copy_to_user(frame_addr, &frame)?;
      task.signal.set_blocked(action.mask_with_current(info.sig));
      ctx.set_user_arg0(info.sig as usize);
      ctx.set_user_pc(action.handler_addr());
      ctx.set_user_sp(frame_addr);
      Ok(())
  }

  fn rt_sigreturn(ctx: UserContextRef) -> Result<usize, Errno> {
      let frame = copy_frame_from_user(ctx.user_sp())?;
      validate_user_context(&frame.saved_context)?;
      restore_context(ctx, frame.saved_context);
      current_task().signal.set_blocked(frame.old_mask);
      current_syscall_context().finalize_frame();
      Ok(0)
  }
  ```
]

`rt_sigreturn` 是少数会接管陷入帧的系统调用。它成功后不应按普通系统调用规则写返回值和推进程序计数器，而是恢复信号发生前的上下文。第八章中的 `finalize_frame` 正是为这类系统调用准备的。若分发器在 `rt_sigreturn` 后继续写返回值，就会破坏用户程序原本的寄存器状态。这个细节是信号机制和系统调用框架之间的重要接口。

信号帧也影响调试和安全。用户态可以篡改信号帧，内核必须验证恢复上下文。内核需要允许合法用户地址和合法信号屏蔽字，同时拒绝返回到内核地址、未对齐栈或不合法状态。验证策略不能过度严格，否则会破坏用户态运行库的合法行为。也不能过度宽松，否则用户可以借助 `sigreturn` 构造异常控制流。我们把架构相关验证放在用户上下文层，信号核心层只管理信号帧生命周期和屏蔽字语义。

== 权限、凭据与 pidfd 文件描述符

信号发送涉及权限。一个进程不能任意向其它用户的进程发送信号。通常规则依赖真实 UID、有效 UID、保存 UID 和 capability 权限。特权任务可以发送更多信号。发送给自己或同一用户进程通常允许。具体规则需要和凭据模型对齐。第五章已经讨论任务凭据，第十章在此基础上使用调度层凭据进行权限判断。

权限检查应在目标解析后、待处理队列入队前完成。目标解析可能失败，返回 `ESRCH`。权限不足返回 `EPERM`。信号号非法返回 `EINVAL`。若信号号为 0，`kill` 用于探测目标是否存在和是否有权限，不真正入队信号。这个语义被很多用户态程序用于进程探测。把这些错误码区分清楚，是 POSIX 兼容的一部分。

pidfd 文件描述符提供了比 PID 数字更稳定的目标引用。PID 可能复用。用户态拿到一个 PID 后，目标进程可能退出，另一个进程复用同一 PID。pidfd 文件对象持有对目标任务或进程的引用，可以避免名字竞态。`pidfd_send_signal` 通过文件描述符解析目标，然后执行同样权限检查和入队逻辑。这样 pidfd 只改变目标定位方式，不改变信号核心语义。

进程组发送还涉及权限遍历。向一个进程组发送信号时，可能部分目标存在，部分目标无权限。内核需要按语义决定是否对允许的目标发送，并返回合适结果。这个细节对 shell 作业控制很重要。我们在发送路径中把目标集合构造、权限检查和入队动作分开，使后续完善进程组语义时有清晰落点。

== 与进程退出和等待接口的关系

信号默认动作经常触发进程退出或状态变化。退出后，父进程需要通过等待接口观察。若子进程因信号终止，等待状态应包含终止信号。若子进程停止，等待状态应包含停止信号。若子进程继续，等待状态应表达继续状态。信号层因此需要把默认动作的原因传给进程管理层，而不是只设置任务状态。

退出路径也会产生信号。子进程退出时，父进程通常收到 `SIGCHLD`。若父进程忽略 `SIGCHLD` 或设置特定标志，子进程回收语义会受影响。这个方向说明信号和等待接口是双向耦合。信号可以导致退出，退出也可以产生信号。我们把实际资源回收放在进程管理层，信号层只提交事件和默认动作原因，使职责边界保持清晰。

线程组退出更复杂。一个线程收到致命信号后，整个线程组可能需要进入退出状态。其它线程如果正在睡眠，需要被唤醒并收束。若只终止当前线程，用户态可能看到违反进程级信号语义的状态。当前设计通过共享信号状态和线程组状态为这一点预留结构。即使实现逐步完善，文档中的模型也明确了目标方向。

等待侧的同步遵循第七章原则。父任务等待子状态变化时登记等待队列。子任务因信号退出或停止时修改状态并唤醒父任务。父任务醒来后重新扫描子列表，消费状态事件。信号层不会直接把返回值写入父任务用户栈，也不会直接完成等待系统调用。这样异步信号和同步等待之间保持清晰交接。

== 工程设计总结

信号机制把异步事件转化为用户态可见控制流。它不能在任意位置直接打断内核执行，只能在待处理队列、返回用户态和用户信号帧之间传递状态。本章把信号拆成状态、发送、屏蔽、等待、投递和返回几个阶段。

信号与异步事件机制具备以下创新。

第一是把每任务状态和线程组状态分离，并让二者在投递阶段汇合。屏蔽字、私有待处理队列和 `sigtimedwait` 属于单个任务，信号动作表和共享待处理队列属于线程组。Linux 也区分线程待处理队列和共享待处理队列，我们的实现保留了这一核心语义，同时把状态结构压缩到更小的 Rust 数据模型中。相比 xv6 这类教学内核通常缺少完整信号体系的做法，这个设计使线程组、进程组和用户处理函数能够共存。

第二是发送阶段只做入队和唤醒，真正投递集中在返回用户态前。这个边界避免信号在内核关键区中直接改变执行流，也让系统调用、异常和调度共享同一套投递逻辑。信号等待同样复用第七章的等待协议，发送者提交事件，等待者根据信号屏蔽字自行判断是否可消费。这样异步信号没有被实现成跨任务回调，而是被压缩为待处理状态和安全点处理。

第三是用户信号帧和 `rt_sigreturn` 通过用户上下文层落实。处理函数执行前，内核保存旧陷入帧和旧屏蔽字。处理函数返回后，`rt_sigreturn` 恢复旧上下文并接管系统调用返回路径。信号核心层不直接理解每个架构的陷入帧细节，而是通过进程映像接口或用户上下文辅助函数处理信号帧写入、备用栈选择和恢复验证。这个边界延续了第八章的陷入帧解耦。

第四是默认动作与进程状态机被纳入同一协议。终止、停止、继续和忽略都会影响退出、等待、调度和进程组。我们把默认动作集中在投递阶段执行，使父子等待、作业控制和线程组退出都能围绕同一状态入口展开。FreeBSD 和 Linux 的信号实现都把进程控制和信号紧密结合，我们的实现保留这个工程事实，但用较小的状态机表达其中最关键的收束关系。

信号机制的价值在于把异步性变成有序协议。发送者只提交事件，接收者在安全边界处理事件。任务可以屏蔽、等待、捕获或默认处理信号，但这些动作都围绕同一组状态结构和返回路径展开。它连接进程管理、系统调用返回、终端控制和运行库语义，是用户态兼容性中最能体现细节密度的子系统之一。信号层越稳定，用户态程序在异常、超时和交互场景下的行为就越可预测。对内核而言，信号层也是检验睡眠唤醒、任务状态、用户栈访问和系统调用返回是否协调一致的集中场景，并为后续终端章节提供控制事件基础，也为异常进程收束提供可靠工具。这种能力会直接影响整机长期运行稳定性，也影响用户态交互体验。
