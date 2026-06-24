#import "../config.typ": project-name
#import "../styles/diagram.typ": flow-arrow, flow-node, layer-card
#import "../styles/figure.typ": continued-table, figure-caption, pseudo-sample
#import "../styles/tokens.typ": handoff-fill, soft-fill, stable-fill, warm-fill

= 第六章 调度系统

在第五章中，任务对象被定义为统一的执行实体，并通过状态机、亲缘关系和扩展侧表承载进程与线程语义。本章继续向下推进，讨论这些任务对象如何获得 CPU 时间。进程管理回答谁存在，调度系统回答谁运行。这个问题看似只涉及一个选择动作，但它实际连接了公平性、响应性、上下文切换成本、CPU 亲和性和实时策略。

调度系统面对的核心矛盾，是目标之间存在天然冲突。公平性要求每个任务按权重获得 CPU。响应性要求刚被唤醒的交互任务尽快运行。吞吐量要求减少无意义的上下文切换。实时策略又要求某些任务绕过普通公平竞争，优先获得确定的执行机会。若调度器只追求公平，短等待任务可能在唤醒后等待过久。若调度器过度偏向唤醒任务，长期运行任务会失去稳定份额。我们采用分层调度类和 EEVDF 公平调度，把不同目标放在不同层次处理。

当前实现的调度系统以运行队列（`Runqueue`）为核心。每个 CPU 拥有一个运行队列。运行队列内部按截止时间类、实时类、公平类和空闲类分层。公平类使用 EEVDF 调度算法。实时类提供 FIFO 和轮转语义。截止时间类保留 EDF、预算和补充时间点的框架。空闲类作为兜底，只在没有其它任务可运行时使用。这样的结构把策略选择、队列排序、抢占判断和架构上下文切换分开，使调度系统可以随平台能力逐步扩展。

== 6.1 设计目标与约束

调度系统的设计目标可以概括为四项。第一，常规任务需要权重公平。`nice` 值应当稳定影响 CPU 份额，任务不能因为一次睡眠获得无限补偿。第二，短等待任务需要及时响应。futex 机制、管道、套接字和 `wait` 这类场景中，唤醒后的交接不能总是等到下一个调度节拍。第三，策略边界必须清晰。实时任务、截止时间任务和普通任务不应混在一棵含义不明的队列中。第四，调度器不能过度依赖平台细节。寄存器保存、CPU 标识、时间读取和空闲指令由架构层注入。

#continued-table(
  "6-1",
  [调度系统的设计目标],
  (1.05fr, 2.1fr, 2.2fr),
  (
    table.cell(fill: soft-fill)[#text(weight: "bold")[目标]],
    table.cell(fill: soft-fill)[#text(weight: "bold")[设计含义]],
    table.cell(fill: soft-fill)[#text(weight: "bold")[实现边界]],
  ),
  (
    table.cell(fill: warm-fill)[公平性],
    table.cell(fill: warm-fill)[公平类通过权重、虚拟时间和 EEVDF 截止点分配 CPU。],
    table.cell(fill: warm-fill)[`nice` 到权重的映射沿用 Linux 权重表，虚拟运行时间推进与权重成反比。],
    table.cell(fill: soft-fill)[响应性],
    table.cell(fill: soft-fill)[唤醒路径可请求重调度，futex 机制热路径可设置一次性优先候选。],
    table.cell(fill: soft-fill)[优先候选不改变长期优先级，选择时仍复查调度类和亲和性。],
    table.cell(fill: handoff-fill)[策略分层],
    table.cell(fill: handoff-fill)[截止时间类、实时类、公平类和空闲类按固定顺序协作。],
    table.cell(fill: handoff-fill)[不同策略拥有独立队列和判断规则，避免公平类逻辑污染实时类。],
    table.cell(fill: stable-fill)[平台解耦],
    table.cell(fill: stable-fill)[时间、CPU 标识、空闲放松动作和上下文切换通过架构钩子注入。],
    table.cell(fill: stable-fill)[调度核心只处理任务对象、运行队列和抽象时间戳。],
  ),
  kind: "表",
  continuation-kind: "续表",
  align: (left, left, left),
)

这里还有一个现实约束。当前代码已经把每 CPU 数组、CPU 位图、调度拓扑和跨 CPU 迁移接口准备好，但启动路径仍以 CPU0 为主要运行环境。文档在描述多 CPU 机制时，只把它作为数据结构和接口层面的能力，不把它写成完整应用处理器启动后的运行状态。这个边界有助于避免把预留设计误读为已经完成的 SMP 调度。

== 6.2 总体结构

调度系统分为四层。最上层是调度策略模型，把用户态传入的调度属性转换为调度策略（`SchedPolicy`）和调度类（`SchedClass`）。第二层是每个任务的调度实体，保存权重、虚拟时间、截止点、实时优先级和截止时间预算等信息。第三层是每 CPU 运行队列，按调度类保存有序队列。第四层是全局调度入口，处理当前任务、定时器节拍、重调度标志、空闲任务和跨 CPU 均衡请求。

#figure(caption: figure-caption("图", "6-1", [调度系统分层结构]))[
  #layer-card("系统调用翻译层", [`sched_setattr`、`sched_setscheduler`、`nice`、`sched_yield` 转换为调度属性], fill: soft-fill)
  #flow-arrow(label: "规范化调度属性")
  #layer-card("调度实体层", [调度实体保存策略、权重、虚拟运行时间、截止点、实时优先级和截止时间预算], fill: soft-fill)
  #flow-arrow(label: "进入本 CPU 运行队列")
  #layer-card("运行队列层", [截止时间队列、实时队列、公平队列、空闲队列和 EEVDF 聚合量], fill: handoff-fill)
  #flow-arrow(label: "选择下一个任务")
  #layer-card("全局调度入口", [`schedule_once`、`on_timer_tick`、`NEED_RESCHED`、空闲任务和 `balance_once`], fill: warm-fill)
  #flow-arrow(label: "委托架构切换")
  #layer-card("架构钩子", [`switch_context`、`now_ns`、`current_cpu_id`、`idle_relax`、`send_resched`], fill: stable-fill)
]

这种分层的关键收益是策略和机制分开。调度属性（`SchedAttr`）负责规范化用户请求。调度实体（`SchedEntity`）负责保存单任务状态。运行队列负责排序和选择。`scheduler.rs` 文件负责系统级入口和每 CPU 槽位。架构层只处理硬件相关动作。每层都可以在不重写其它层的前提下演化。例如未来把公平队列内部从有序映射（`BTreeMap`）换成更专用的数据结构，不需要改变任务对象身份模型和系统调用 ABI。

== 6.3 调度策略与调度类

调度策略使用稳定枚举表达。调度策略描述用户可见策略，包括公平策略、实时 FIFO 策略、实时轮转策略、截止时间策略和空闲策略。调度类描述运行队列选择顺序。截止时间类优先于实时类，实时类优先于公平类，公平类优先于空闲类。运行队列选择任务时按这个顺序查找可运行任务。

#pseudo-sample("6-1", [调度策略与属性], kind: "代码")[
  ```rust
  enum SchedClass {
      Deadline,
      Realtime,
      Fair,
      Idle,
  }

  enum SchedPolicy {
      Fair,
      RtFifo,
      RtRoundRobin,
      Deadline,
      Idle,
  }

  struct SchedAttr {
      policy: SchedPolicy,
      nice: i8,
      slice_ns: u64,
      priority: u8,
      runtime_ns: u64,
      deadline_ns: u64,
      period_ns: u64,
  }

  fn normalize(attr: SchedAttr) -> Result<SchedAttr, Errno> {
      match attr.policy {
          SchedPolicy::Fair => normalize_fair(attr),
          SchedPolicy::RtFifo => validate_rt_priority(attr),
          SchedPolicy::RtRoundRobin => validate_rr_slice(attr),
          SchedPolicy::Deadline => validate_deadline_tuple(attr),
          SchedPolicy::Idle => Ok(idle_attr()),
      }
  }
  ```
]

我们没有在调度热路径上使用运行时虚表来表示调度类。策略被规约为枚举，运行队列根据枚举进入不同子队列。这减少了间接调用，也让选择顺序在代码中保持显式。代价是新增调度类需要修改枚举和运行队列结构。考虑到调度类数量很少，并且每类策略都需要特殊的系统调用校验和统计语义，这个代价可以接受。

实时类采用静态优先级。FIFO 任务在同优先级内按入队顺序运行，轮转任务在同优先级内消耗时间片后重新入队。截止时间类保存运行时间、截止时间和周期，并用绝对截止时间排序，同时维护预算和补充时间点。当前实现尚未提供完整的全局准入控制，所以截止时间类更接近调度框架和 ABI 边界，不能被描述为完整硬实时保证。

== 6.4 公平类与 EEVDF

公平类负责普通任务。它使用 EEVDF 调度算法。EEVDF 的核心是三个值。`vruntime` 表示任务在虚拟时间轴上的运行进度。权重越大，物理运行时间转换成虚拟运行时间的增量越小。`avg_vruntime` 表示运行队列上所有公平任务的加权平均进度。只有 `vruntime <= avg_vruntime` 的任务是合格任务。`deadline` 表示任务当前时间片在虚拟时间轴上的截止点，合格任务中截止点最小者优先运行。

#pseudo-sample("6-2", [EEVDF 的基本计算], kind: "代码")[
  ```rust
  const NICE_0_WEIGHT: u64 = 1024;

  fn scale_delta(delta_exec_ns: u64, weight: u64) -> u64 {
      ((delta_exec_ns as u128 * NICE_0_WEIGHT as u128) / weight.max(1) as u128) as u64
  }

  fn update_current(task: &Arc<Task>, delta_exec_ns: u64) {
      let delta_vruntime = scale_delta(delta_exec_ns, task.sched.weight());
      task.sched.vruntime += delta_vruntime;
      task.sched.deadline = task.sched.vruntime + scale_delta(task.sched.slice_ns(), task.sched.weight());
  }

  fn eligible(task: &Arc<Task>, avg_vruntime: u64) -> bool {
      task.sched.vruntime() <= avg_vruntime
  }
  ```
]

EEVDF 对 CFS 的一个重要修正，是避免睡眠任务在唤醒后获得过量奖励。任务离开运行队列时，调度器保存 `lag = avg_vruntime - vruntime`。任务重新入队时，调度器按新的平均虚拟运行时间恢复这个滞后量。这样任务睡眠前的领先或落后状态可以跨睡眠周期保留。长期睡眠不会直接变成无限优先级，短等待任务又能保留应该获得的补偿。

#pseudo-sample("6-3", [滞后量保存与恢复], kind: "代码")[
  ```rust
  fn dequeue_fair(task: &Arc<Task>, rq: &mut RqInner) {
      let avg = avg_vruntime(rq);
      let lag = avg as i64 - task.sched.vruntime() as i64;
      task.sched.store_lag(lag);
      remove_from_fair_tree(task, rq);
      subtract_rq_account(task, rq);
  }

  fn enqueue_fair(task: Arc<Task>, rq: &mut RqInner, now_ns: u64) {
      let avg = avg_vruntime(rq);
      let lag = task.sched.lag();
      if lag != 0 {
          task.sched.store_vruntime((avg as i64 - lag).max(0) as u64);
          task.sched.store_lag(0);
      } else {
          task.sched.store_vruntime(task.sched.vruntime().max(rq.min_vruntime));
      }

      let deadline = task.sched.recalc_deadline();
      task.sched.store_deadline(deadline);
      insert_into_fair_tree(task, rq);
  }
  ```
]

运行队列维护 `total_weight` 和 `weighted_vruntime_sum`，用增量方式计算平均虚拟运行时间。任务入队和离队时更新聚合量。当前任务运行时，调度节拍推进它的虚拟运行时间，并同步更新聚合量。这样平均虚拟运行时间的维护不需要每次遍历所有任务。选择路径仍会扫描有序候选，因为它还要复查任务状态、CPU 亲和性、优先候选和合格条件。文档因此只说平均虚拟时间的维护是增量的，不把整个选择路径说成纯 O(1)。

== 6.5 运行队列与多调度类队列

每个运行队列内部有五棵有序树。公平树按 EEVDF 截止点和任务地址排序。实时树按倒序优先级、入队序号和任务地址排序。截止时间树按绝对截止时间排序。截止时间限流树保存预算耗尽后等待补充的任务。空闲树保存兜底任务。运行队列用一把锁保护所有子队列和当前任务指针。

#pseudo-sample("6-4", [运行队列结构], kind: "代码")[
  ```rust
  struct Runqueue {
      inner: Spinlock<RqInner>,
  }

  struct RqInner {
      fair_tree: BTreeMap<FairKey, Arc<Task>>,
      rt_tree: BTreeMap<RtKey, Arc<Task>>,
      deadline_tree: BTreeMap<DeadlineKey, Arc<Task>>,
      deadline_throttled: BTreeMap<DeadlineThrottleKey, Arc<Task>>,
      idle_tree: BTreeMap<RtKey, Arc<Task>>,
      total_weight: u128,
      weighted_vruntime_sum: u128,
      min_vruntime: u64,
      current: Option<Arc<Task>>,
      enqueue_seq: u64,
      preferred_fair_addr: Option<usize>,
  }
  ```
]

单锁保护整个运行队列，是一个有意识的取舍。调度操作需要同时观察当前任务、多个子队列和 EEVDF 聚合量。若给每个子队列单独加锁，跨调度类抢占和任务选择就需要固定锁序，错误空间明显增大。调度锁确实是热路径锁，但每 CPU 运行队列把竞争限制在本 CPU 内。当前核心数和任务规模下，单锁的可调试性收益更重要。

有序映射的选择也偏向工程稳定。堆在取最小值上很便宜，但删除任意任务和重排任务不方便。链表插入简单，但查找最早截止点或最高优先级需要扫描。有序映射让插入、删除和取最小都保持对数复杂度，并且便于调试时观察键的顺序。后续若某类任务数量增长，可以在子队列内部替换数据结构，不影响调度类边界。

== 6.6 重调度时机与上下文切换

定时器中断调用 `on_timer_tick`。它唤醒超时睡眠者，处理实时计时器，调用当前 CPU 的运行队列节拍函数，并在需要抢占时设置 `NEED_RESCHED`。定时器中断本身不直接执行完整上下文切换。真正的切换发生在系统调用返回、中断返回、主动让出或空闲循环等安全点。

#pseudo-sample("6-5", [调度节拍与延迟重调度], kind: "代码")[
  ```rust
  fn on_timer_tick(now_ns: u64) {
      wake_expired_sleepers(now_ns);
      fire_expired_realtime_itimers(now_ns);

      let cpu_id = current_cpu_id();
      if RUNQUEUES[cpu_id].tick(now_ns) {
          NEED_RESCHED[cpu_id].store(true, Release);
      }
  }

  fn run_if_needed() {
      let cpu_id = current_cpu_id();
      if NEED_RESCHED[cpu_id].swap(false, AcqRel) {
          if NEED_BALANCE[cpu_id].swap(false, AcqRel) {
              balance_once(cpu_id);
          }
          schedule_once(now_ns_public());
      }
  }
  ```
]

延迟重调度的原因来自内核关键区。定时器中断可能打断任意位置。若中断处理函数直接切换任务，当前任务可能正在持有自旋锁，新的任务随后等待同一把锁，就会出现不可恢复的阻塞。`NEED_RESCHED` 把异步事件变成同步决策。切换只在已知安全的位置发生。这一原则与第五章中等待队列的锁外唤醒保持一致。

`schedule_once` 的主要流程是取得当前 CPU 的运行队列，选择下一个可运行任务，必要时回退到空闲任务，然后通过架构钩子切换上下文。退出任务在最终切离 CPU 后会进入延迟释放列表，后续调度边界再在活任务栈上释放它的虚拟内存、文件表和执行上下文。这样可以避免在已经离开的内核栈上运行析构。

#pseudo-sample("6-6", [schedule_once 的关键路径], kind: "代码")[
  ```rust
  fn schedule_once(now_ns: u64) {
      let cpu_id = current_cpu_id();
      cleanup_retired_tasks(cpu_id);

      let prev = CURRENT_TASKS[cpu_id].clone().unwrap();
      deliver_pending_signals_if_needed(&prev);

      let next = RUNQUEUES[cpu_id]
          .pick_next_on(now_ns, cpu_mask(cpu_id))
          .or_else(|| idle_task(cpu_id))
          .unwrap_or_else(|| Arc::clone(&prev));

      if Arc::ptr_eq(&prev, &next) {
          return;
      }

      CURRENT_TASKS[cpu_id] = Some(Arc::clone(&next));
      arch_switch_context(prev.arch_context(), next.arch_context());
  }
  ```
]

上下文切换的直接成本包括保存寄存器、恢复寄存器、设置当前任务指针和切换内核陷入栈。间接成本来自 TLB、缓存和分支预测状态。地址空间切换由第二章中的用户页表接口和架构层处理。线程之间共享用户虚拟地址空间时，可以避免一部分页表和 TLB 成本。调度系统只要求任务在切换前拥有有效架构上下文，不直接解析页表细节。

== 6.7 唤醒、优先候选与系统调用后交接

唤醒路径通过 `enqueue_task` 把任务放入目标 CPU 的运行队列，并请求该 CPU 重调度。普通唤醒只改变任务状态和队列归属。futex 这类短等待热路径可以调用 `enqueue_task_preferred`，在公平队列中记录一次性优先候选。选择任务时仍然会复查任务状态、调度类和 CPU 亲和性。优先候选只是减少短等待任务等到下一个调度节拍的概率，不改变长期公平性。

`clone` 后还有一个专门的系统调用后交接。新任务创建成功时，父任务的系统调用返回值和程序计数器仍在收尾阶段。若在 `clone` 内部立即切换，父任务的陷阱帧可能尚未写好。我们把交接请求记录在 `POST_SYSCALL_HANDOFF` 中，等系统调用分发器完成收尾后再执行一次有界调度。这个设计解决了父子任务首轮运行顺序与陷阱帧一致性的冲突。

#pseudo-sample("6-7", [唤醒与一次性交接], kind: "代码")[
  ```rust
  fn enqueue_task_preferred(task: Arc<Task>, now_ns: u64) {
      let cpu = select_task_cpu(&task);
      task.set_current_cpu(cpu);
      RUNQUEUES[cpu].enqueue_preferred(task, now_ns);
      request_resched(cpu);
  }

  fn request_post_syscall_handoff() {
      let cpu = current_cpu_id();
      POST_SYSCALL_HANDOFF[cpu].store(1, Release);
      request_resched(cpu);
  }

  fn run_post_syscall_handoff(now_ns: u64) {
      let cpu = current_cpu_id();
      if POST_SYSCALL_HANDOFF[cpu].swap(0, AcqRel) != 0 {
          schedule_once(now_ns);
      }
  }
  ```
]

这里的共同原则是把异步性限制在安全边界。唤醒可以发生在等待队列、设备中断、套接字状态变化或定时器到期时。它们只负责把任务变为可运行状态，并请求调度。是否立刻切换，由当前 CPU 在安全点决定。

== 6.8 CPU 亲和性、拓扑与负载均衡

调度器使用固定容量 CPU 位图表示亲和性和在线状态。CPU 位图（`CpuMask`）始终被截断到当前构建支持的 CPU 范围内，空亲和性会退回启动 CPU。调度拓扑（`SchedTopology`）保存调度域。默认拓扑只有覆盖所有支持 CPU 的根域，平台可以在启动期安装更细的域层级。任务放置时，调度器在亲和性与在线 CPU 的交集中选择负载较低的 CPU，并尽量保留当前 CPU。

#pseudo-sample("6-8", [CPU 选择与负载快照], kind: "代码")[
  ```rust
  fn select_task_cpu(task: &Arc<Task>) -> usize {
      let affinity = CpuMask::from_bits_or_boot(task.cpu_affinity());
      let online = online_cpu_set();
      let current = CpuId::new(task.current_cpu());
      let prefer_current = task.state() != TaskState::New;
      let snapshot = collect_rq_load_snapshot(affinity.intersection(online));

      sched_topology()
          .select_cpu(affinity, online, current, prefer_current, |cpu| snapshot.load_of(cpu))
          .unwrap_or_else(CpuId::boot)
          .get()
  }
  ```
]

负载均衡通过 `balance_once` 从较忙 CPU 拉取一个可迁移任务到本 CPU。它不会直接摘走远端当前任务，只从公平、实时和截止时间就绪队列中取可迁移任务。截止时间限流队列和空闲队列不参与迁移。选择源 CPU 时，调度器使用一次负载快照，避免在拓扑遍历过程中反复持有多个运行队列锁。

#pseudo-sample("6-9", [一次负载均衡], kind: "代码")[
  ```rust
  fn balance_once(cpu_id: usize) -> bool {
      let local = CpuId::new(cpu_id).unwrap_or_else(CpuId::boot);
      let online = online_cpu_set();
      let local_load = RUNQUEUES[cpu_id].migratable_load();

      let source = select_balance_source(sched_topology(), local, online, local_load, |cpu| {
          RUNQUEUES[cpu.get()].migratable_load_for(local.mask().bits())
      })?;

      let task = RUNQUEUES[source.get()].take_migratable(local.mask().bits(), now_ns_public())?;
      task.set_current_cpu(cpu_id);
      RUNQUEUES[cpu_id].enqueue(task, now_ns_public());
      true
  }
  ```
]

这个均衡策略偏保守。它只在明显不均衡时迁移，避免任务在两个负载接近的 CPU 之间来回移动。迁移会带来缓存冷启动和 TLB 局部性损失。对于当前单核为主的运行环境，均衡逻辑主要是为多 CPU 启动准备好数据结构和锁顺序。未来接入完整应用处理器后，可以在调度域中表达共享缓存、NUMA 或其它拓扑信息。

== 6.9 空闲任务与初始化顺序

调度器初始化依赖架构上下文钩子。启动期先注册架构上下文接口和时间接口，再创建 init 任务，建立根 PID 命名空间、线程组、进程组和会话。init 任务接管当前 CPU 的执行上下文。随后内核注册任务扩展克隆钩子、退出钩子和退出前钩子，并为 CPU0 派生空闲任务。

空闲任务是每个 CPU 的兜底执行体。它不参与普通公平权重竞争，调度类被设置为空闲类。运行队列没有其它可运行任务时，`schedule_once` 才会回退到空闲任务。空闲循环会尝试一次负载均衡，然后调用 `schedule_once`。仍然没有任务可运行时，架构空闲钩子可以执行 `wfi` 一类放松指令。未注入时退化为自旋提示。

#pseudo-sample("6-10", [调度器初始化与空闲任务], kind: "代码")[
  ```rust
  fn sched_boot_init() {
      register_arch_context_ops();
      register_arch_time_ops();
      let init = sched::init();

      register_ext_clone_hook(&KERNEL_EXT_CLONE_HOOK);
      register_ext_exit_hook(&KERNEL_EXT_EXIT_HOOK);
      register_pre_exit_hook(&KERNEL_PRE_EXIT_HOOK);

      spawn_idle_for_cpu(0);
      start_user_init_from(init);
  }

  fn idle_loop() -> ! {
      loop {
          balance_once(current_cpu_id());
          schedule_once(now_ns_public());
          arch_idle_relax_or_spin();
      }
  }
  ```
]

空闲任务被设计成独立调度类，而非一个极低权重的普通任务。任何有限权重都意味着它会在公平竞争中获得份额。空闲类的真实语义是没有其它任务时才运行。独立调度类能直接表达这个语义，也让运行队列在队列为空时拥有安全落点。

== 6.10 工程设计总结

调度系统把高频执行路径和策略扩展路径分开。高频路径只处理每 CPU 运行队列、原子状态、虚拟时间和架构上下文。策略扩展通过调度策略、调度类、调度属性和子队列完成。这个结构让常规任务、实时任务、截止时间任务和空闲任务都能进入同一调度框架，同时保留各自语义。

调度系统具备以下创新。

第一是以 EEVDF 作为普通任务的公平调度基础。它把权重公平和延迟敏感放在同一个虚拟时间模型中处理。虚拟运行时间保证任务按权重推进。合格条件防止已经领先平均进度的任务继续占用 CPU。截止点选择让更紧迫的任务更早运行。滞后量保存和恢复解决了睡眠任务跨周期的公平性问题。与简单的最小虚拟运行时间选择相比，EEVDF 更适合处理大量短等待任务和长期运行任务混合的场景。我们在实现中还保留了增量平均虚拟运行时间维护，避免每次调度重新遍历整个公平队列。

第二是调度类分层让策略冲突保持在明确边界内。截止时间类、实时类、公平类和空闲类按固定顺序协作。实时任务不参与 `nice` 权重竞争，普通任务也不能因为滞后量补偿越过实时任务。截止时间类保留 EDF 和预算框架，但文档不把它描述为完整硬实时实现，因为准入控制尚未落地。这样的写法看似保守，却符合工程实际。一个调度器最危险的问题之一，是对外承诺强于内部机制能够保证的语义。我们宁愿把边界写清楚，也不让后续维护者误以为当前截止时间类已经提供完整实时保证。

第三是架构相关操作全部通过钩子注入。调度核心不读取具体 CSR，不解释保存寄存器的布局，也不直接执行某条空闲指令。它只知道如何取得当前 CPU 标识、读取时间戳、请求远端重调度和切换上下文。这个结构使 RISC-V64 与 LoongArch64 共用调度策略，同时允许两者在汇编切换、陷入栈设置和空闲放松动作上各自优化。第一章启动层、第二章内存层和第五章任务执行体都遵循类似模式。调度系统在这里延续了全局的单向依赖原则。

这些创新共同形成了调度子系统的工程价值。它让普通任务获得可解释的权重公平，让短等待路径具备及时交接能力，让实时和截止时间策略拥有独立入口，也让未来 SMP 扩展有稳定的数据结构基础。调度器越成熟，用户越不应直接感知它。我们的目标并非让调度策略在正常负载下频繁暴露存在感，而是让任务创建、阻塞、唤醒和退出都能自然落入同一套可分析的时间分配机制。
