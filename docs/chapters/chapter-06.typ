#import "../config.typ": project-name
#import "../styles/diagram.typ": flow-arrow, flow-node, layer-card
#import "../styles/figure.typ": continued-table, figure-caption, pseudo-sample
#import "../styles/tokens.typ": handoff-fill, soft-fill, stable-fill, warm-fill

= 第六章 调度系统

第五章讨论了进程与线程管理子系统如何通过统一的 Task 抽象、亲缘关系和生命周期状态机来管理操作系统的执行实体。然而,Task 本身只是一种静态的资源单位,它描述了"谁在这里",却没有回答"现在该谁运行"这个问题。在任意时刻,系统中就绪的 Task 通常远多于可用的 CPU 核心数,而每个 CPU 在任意时刻只能执行一个 Task，因此必须有一套机制来决定就绪的 Task 中哪一个获得当前的 CPU 时间。这就是调度系统的职责。调度系统的设计直接决定了用户的交互感受和系统的吞吐能力，它的响应速度决定了交互程序的流畅程度,它的公平性决定了多个任务能否按预期分享处理器,它的可扩展性决定了系统在多核高负载下的表现。

调度系统面对的根本矛盾不止于"任务多于核心"这一表层冲突,更深层的矛盾在于调度目标本身的多元性。公平性要求所有任务按其权重比例分享 CPU,任何任务不应被持续饿死;响应性要求交互任务能够快速从睡眠中被唤醒并获得 CPU,因为人类感知的延迟阈值在毫秒级别;吞吐量要求批处理任务能够长时间持续运行,因为频繁的上下文切换会污染 CPU 缓存、稀释指令流水线;实时性要求关键任务获得确定的时间保证,因为某些应用(音频处理、工业控制、机械臂运动)无法容忍调度延迟的不确定性。这四个目标在同一个调度算法中往往是冲突的，为公平性而频繁切换会损害吞吐量,为吞吐量而延长时间片会损害响应性,为响应性而抢占长任务会损害实时任务的时间保证。调度系统的设计本质上是在这些相互矛盾的目标之间做出权衡。

之所以将调度系统作为一个独立的章节讨论,而不是作为进程管理的一部分,是因为调度的关注点与进程的关注点是正交的。进程管理关心的是"任务作为资源单位如何创建、共享和销毁",调度系统关心的是"任务作为执行单位如何获得 CPU 时间"。这两个关注点的数据结构、锁的粒度和并发模型完全不同，进程管理使用集中式的亲缘关系锁,调度系统使用每 CPU 的运行队列锁;进程管理的操作频率是低频的(创建、销毁、wait),调度系统的操作频率是高频的(每次时钟中断、每次系统调用返回都可能触发调度)。将两者解耦使得它们可以独立演化,调度算法的更迭(从 O(1) 到 CFS 再到 EEVDF)不需要修改进程管理的代码。

== 调度系统的分层架构

调度系统采用分层设计,不同层次负责不同粒度的调度决策。最上层是调度类(SchedClass)的选择,它根据任务的属性和系统的策略决定应该使用哪一种调度算法。中间层是调度实体在运行队列中的排序,它决定同一个调度类内部任务的调度顺序。最下层是运行队列的管理,它提供任务的入队、出队、迁移等基本操作。再向下是 CPU 管理层,它负责多核之间的负载均衡和任务迁移。

#figure(caption: figure-caption("图", "6-1", [调度系统分层架构]))[
  #layer-card("调度类层", [根据任务属性选择 Fair / RT / DL 调度策略,定义任务在该类中的优先级计算方式与抢占规则。], fill: soft-fill)
  #flow-arrow(label: "委托内部排序")
  #layer-card("调度实体层", [EEVDF 调度实体维护虚拟时间、截止时间、权重和 lag,实体在运行队列中按 deadline 排序。], fill: soft-fill)
  #flow-arrow(label: "委托数据结构")
  #layer-card("运行队列层", [每 CPU 一个红黑树运行队列,O(log n) 入队 / 出队 / 取最小 deadline。], fill: warm-fill)
  #flow-arrow(label: "委托跨核协调")
  #layer-card("CPU 管理层", [多核负载均衡、任务迁移、CPU 亲和性。], fill: warm-fill)
]

之所以采用分层设计,是因为调度系统的不同层次面对的是不同性质的问题，调度类选择面对的是"策略多样性"(不同任务有不同的调度需求),调度实体排序面对的是"算法效率"(必须在 O(log n) 内做出决策),运行队列管理面对的是"并发安全"(高频访问下的锁竞争),CPU 管理面对的是"硬件拓扑"(NUMA、缓存共享、超线程)。如果将这些问题混在一起处理,任何一类问题的复杂度增加都会污染整个调度器的代码。分层之后,每一层只关心自己的问题,新的调度类不需要修改运行队列层,新的负载均衡算法不需要修改调度类。这种"职责分离"的设计哲学贯穿了整个内核,在第二章的内存管理、第三章的设备模型和第四章的 VFS 中都能看到同样的模式。

== 调度类与策略多样性

调度类是对调度策略的抽象。每个 Task 属于一个调度类,调度类决定了 Task 的优先级计算方式、入队和出队逻辑、时间片管理和抢占规则。系统提供三种调度类:公平调度类(Fair)处理大多数交互式和批处理任务,实时调度类(RT)处理需要硬性时间保证的任务,截止时间调度类(DL)处理需要精确截止时间约束的任务。三类调度类共存于同一个调度器中,通过严格的优先级顺序协作，即 DL 优先于 RT,RT 优先于 Fair。

#pseudo-sample("6-1", [调度类抽象与策略枚举], kind: "代码")[
  ```c
  // 调度策略枚举(与 POSIX 对齐)
  enum SchedPolicy {
      SCHED_NORMAL,    // EEVDF 公平调度
      SCHED_FIFO,      // 实时 FIFO
      SCHED_RR,        // 实时轮转
      SCHED_DEADLINE,  // 截止时间调度
      SCHED_BATCH,     // 批处理(公平类的低优先级变体)
      SCHED_IDLE,      // 空闲(公平类的最低优先级变体)
  };

  // 调度类接口(运行时 vtable)
  struct SchedClass {
      const char* name;
      int priority;  // 类间优先级(DL=0, RT=1, Fair=2)

      void  (*enqueue)   (Task* t, Runqueue* rq, u64 now_ns);
      void  (*dequeue)   (Task* t, Runqueue* rq);
      Task* (*pick_next) (Runqueue* rq, u64 now_ns);
      void  (*put_prev)  (Task* t, Runqueue* rq, u64 now_ns);
      void  (*tick)      (Task* t, Runqueue* rq, u64 now_ns);
      bool  (*check_preempt)(Task* curr, Task* wakee);
  };

  // 任务的调度参数
  struct SchedAttr {
      SchedPolicy policy;
      i8  nice;          // Fair 类的 nice 值(-20..19)
      u64 slice_ns;      // 时间片长度
      u8  rt_priority;   // RT 类的实时优先级(1..99)
      u64 dl_runtime;    // DL 类的运行预算
      u64 dl_deadline;   // DL 类的截止时间
      u64 dl_period;     // DL 类的周期
  };
  ```
]

调度类的接口设计借鉴了驱动框架中的 vtable 设计，调度核心通过函数指针调用具体的调度类方法,而不需要知道该方法的具体实现。当调度核心需要选择下一个任务时,它依次调用 DL、RT、Fair 三个调度类的 `pick_next` 方法,直到某个调度类返回非空结果。这种设计使得新的调度类可以通过实现这套接口接入调度器,而不需要修改调度核心的代码。这与第三章中字符设备和块设备的驱动接口设计是一脉相承的，接口规定语义,实现选择策略。

公平调度类是默认的调度类,适用于大多数任务。它的核心目标是按权重比例分享 CPU，nice 值低的任务权重大,获得更多的 CPU 时间;nice 值高的任务权重小,获得更少的 CPU 时间。公平调度类的具体算法是 EEVDF,它在保留权重公平性的同时引入了延迟敏感维度,使得睡眠时间长的任务不会获得不公平的"奖励"。公平调度类还包含两个低优先级变体，其中 SCHED_BATCH 适用于不关心交互延迟的批处理任务(允许更长的时间片),SCHED_IDLE 适用于只在 CPU 完全空闲时才运行的低优先级任务。

实时调度类适用于需要硬性时间保证的任务。它有两种策略:SCHED_FIFO 是先进先出策略,同一优先级的任务按入队顺序执行,没有时间片限制,任务一旦获得 CPU 就一直运行直到主动放弃或被更高优先级任务抢占;SCHED_RR 在 SCHED_FIFO 的基础上引入了时间片轮转,同一优先级的任务轮转执行,每个任务有一个固定的时间片,时间片用完后排到同优先级队列的末尾。实时调度类的优先级范围是 1 到 99,数字越大优先级越高,优先级是静态的,由 `sched_setscheduler` 系统调用设置,内核不会自动调整。

截止时间调度类适用于具有周期性时间约束的任务。每个 DL 任务有三个参数:运行时间预算(runtime)指定任务在每个周期内可以使用的最大 CPU 时间,截止时间(deadline)指定任务必须在多长时间内完成,周期(period)指定任务的重复周期。调度器保证在每个周期内,DL 任务至少能运行 runtime 时间,且不会超过 deadline。DL 类使用 EDF(Earliest Deadline First)算法,选择当前 deadline 最早的任务执行。如果任务超出预算或错过 deadline,调度器可以采取降级措施,如暂停任务或杀死任务。

== EEVDF 算法

EEVDF(Earliest Eligible Virtual Deadline First,最早合格虚拟截止时间优先)是公平调度类的核心算法。EEVDF 是 CFS(Completely Fair Scheduler)的继任者,它在保留 CFS 权重公平性的同时引入了"延迟敏感"维度,使得调度决策更精确。理解 EEVDF 需要理解三个核心概念:虚拟时间、合格性和截止时间。

=== 虚拟时间与权重

虚拟时间(vruntime)是任务在调度器视角下的运行时间。它的核心特性是推进速度与权重成反比，权重越大的任务,vruntime 推进越慢;权重越小的任务,vruntime 推进越快。在相同的物理运行时间内,低权重任务的 vruntime 会"走"得更远,而调度器倾向于选择 vruntime 较小的任务,因此低权重任务获得调度的机会更少。这种通过虚拟时间映射权重的设计相比于传统的优先级队列有一个显著的优势，它将"权重"这个静态属性转换成了"虚拟时间"这个动态属性,使得任务的调度顺序不仅取决于它的权重,还取决于它已经运行了多久。这就避免了优先级反转和饥饿问题，因为即使是低权重任务,只要它的 vruntime 小于其他任务,它就会被优先调度。

#pseudo-sample("6-2", [EEVDF 调度实体与虚拟时间推进], kind: "代码")[
  ```c
  // EEVDF 调度实体
  struct SchedEntity {
      u32 weight;       // 权重(由 nice 值映射,nice=0 → weight=1024)
      u64 vruntime;     // 虚拟运行时间
      u64 deadline;     // 虚拟截止时间
      u64 slice_ns;     // 时间片长度
      i64 lag;          // 离开运行队列时的 lag
      bool on_rq;       // 是否在运行队列上
      u64 exec_start;   // 本次开始执行的物理时间戳
  };

  // 物理时间到虚拟时间的换算
  // delta_vruntime = delta_exec * NICE_0_WEIGHT / weight
  u64 calc_delta_vruntime(u64 delta_exec_ns, u32 weight) {
      const u32 NICE_0_WEIGHT = 1024;
      return ((u128)delta_exec_ns * NICE_0_WEIGHT) / weight;
  }

  // 周期性更新当前任务的 vruntime
  void update_curr(Task* curr, u64 now_ns) {
      SchedEntity* se = &curr->sched;
      u64 delta_exec = now_ns - se->exec_start;

      se->vruntime += calc_delta_vruntime(delta_exec, se->weight);
      se->exec_start = now_ns;

      // 更新运行队列的平均 vruntime(EEVDF 关键)
      update_avg_vruntime(curr->rq);
  }
  ```
]

权重的具体数值由 nice 值通过查表映射得到。nice 值的范围是 -20 到 19,nice = 0 对应的权重是 1024(NICE_0_WEIGHT),每减少一个 nice 值,权重大约增加 25%。这种指数关系使得 nice 值的微小变化就能产生显著的调度差异，例如 nice = -1 的任务获得的 CPU 时间约是 nice = 0 任务的 1.25 倍,nice = -10 的任务获得的 CPU 时间约是 nice = 0 任务的 9 倍。指数权重映射相比于线性权重映射的优势在于:它使得整个 nice 值范围都有意义,无论是低 nice 值还是高 nice 值,改变一个单位都会带来可感知的调度差异;线性映射则会使得 nice 值的两端要么过于敏感,要么过于迟钝。

=== 合格性筛选与 deadline 选择

EEVDF 在 CFS 的基础上引入了"合格性"概念。一个任务是合格的(eligible)当且仅当它的 vruntime 小于等于运行队列的平均 vruntime(avg_vruntime)。avg_vruntime 是所有就绪任务 vruntime 的加权平均值,它代表了"如果所有任务公平分享 CPU,当前应该到达的虚拟时间"。一个任务的 vruntime 小于 avg_vruntime 意味着它"落后"于公平进度,应该被调度;一个任务的 vruntime 大于 avg_vruntime 意味着它"领先"于公平进度,应该等待。

#pseudo-sample("6-3", [EEVDF 选择下一个任务], kind: "代码")[
  ```c
  Task* eevdf_pick_next(Runqueue* rq, u64 now_ns) {
      Task* candidate = NULL;
      u64 min_deadline = U64_MAX;
      u64 avg = rq->avg_vruntime;

      // 第一遍:在所有合格任务中选 deadline 最小者
      for (Task* t = fair_first(rq); t; t = fair_next(rq, t)) {
          SchedEntity* se = &t->sched;
          if (se->vruntime <= avg) {  // 合格性检查
              if (se->deadline < min_deadline) {
                  min_deadline = se->deadline;
                  candidate = t;
              }
          }
      }
      if (candidate) return candidate;

      // 第二遍:无合格任务,选 vruntime 最接近 avg 者(避免空跑)
      i64 min_lag = I64_MAX;
      for (Task* t = fair_first(rq); t; t = fair_next(rq, t)) {
          SchedEntity* se = &t->sched;
          i64 lag = (i64)avg - (i64)se->vruntime;
          if (lag < min_lag) {  // 注:此处 lag 为负
              min_lag = lag;
              candidate = t;
          }
      }
      return candidate;
  }
  ```
]

之所以引入合格性这个概念,是因为在 CFS 中,长时间睡眠的任务醒来后其 vruntime 可能远小于当前 avg_vruntime,这导致调度器会持续选中这个任务,直到它的 vruntime 追赶上 avg_vruntime，而这一过程可能持续数百毫秒,期间其他任务无法获得 CPU,系统的响应性严重劣化。这种现象被称为"睡眠奖励",它源于 CFS"贪婪选最小 vruntime"的策略。EEVDF 通过合格性机制消除了这个问题:即使某个任务的 vruntime 很小,只要它大于 avg_vruntime - 某个阈值就被认为是合格的,而那些 vruntime 远小于 avg_vruntime 的任务则不参与调度,需要等待，但它们参与的方式不是"被忽略",而是通过 lag 机制在重新入队时被调整,后文将详细讨论。

deadline 是任务的虚拟截止时间,它决定了任务的调度紧迫度。deadline 的计算公式是 `deadline = vruntime + slice_ns * NICE_0_WEIGHT / weight`,这个公式的含义是:任务从当前 vruntime 开始,在使用完一个时间片后应该到达的虚拟时间。权重越小的任务,deadline 越远;权重越大的任务,deadline 越近。在所有合格任务中选择 deadline 最小者,等价于选择"最紧迫"的任务。这种"合格性筛选 + deadline 选择"的双重策略既保证了公平性(只有合格的任务参与调度),又保证了响应性(选择最紧迫的合格任务)。

=== lag 机制与重新入队

lag 是 EEVDF 的另一个关键概念,它解决了任务跨睡眠周期的公平性问题。当任务离开运行队列时(进入睡眠、被抢占或退出),调度器计算 `lag = avg_vruntime - vruntime` 并保存。当任务重新入队时,调度器根据保存的 lag 调整任务的 vruntime,使得它的"领先"或"落后"程度被保留下来。

具体地,如果离队时 lag 为正(任务此前 vruntime 落后于 avg,即应该获得更多 CPU 但被打断了),重新入队时调度器将任务的 vruntime 设置为新的 avg_vruntime - lag,使得任务保持原有的落后程度,从而能在追赶过程中获得更多的调度机会。如果离队时 lag 为负(任务此前 vruntime 领先于 avg,即已经获得了超额的 CPU 时间),重新入队时调度器将任务的 vruntime 设置为新的 avg_vruntime + |lag|,使得任务保持原有的领先程度,需要等待其他任务追赶上来才能继续获得调度。这种"保留 lag"的机制保证了公平性的跨周期一致性，任务跨多次睡眠和唤醒,它累积获得的 CPU 时间仍然符合权重比例。

lag 机制相比于 CFS 中的"睡眠唤醒补偿"机制的优势在于精确性。CFS 的补偿机制是启发式的，它将醒来任务的 vruntime 设置为 max(vruntime, min_vruntime - sched_latency / 2),其中 sched_latency 是一个固定常数。这种启发式既不能完全消除睡眠奖励(短暂睡眠后唤醒的任务仍然获得不公平的优先级),也不能完全消除睡眠惩罚(长时间睡眠后唤醒的任务可能等待过久)。lag 机制通过精确记录任务离队时的状态,并在入队时精确恢复,消除了启发式的不确定性。

#pseudo-sample("6-4", [EEVDF 入队与离队中的 lag 处理], kind: "代码")[
  ```c
  // 离队:计算并保存 lag
  void eevdf_dequeue(Task* task, Runqueue* rq) {
      SchedEntity* se = &task->sched;
      u64 avg = rq->avg_vruntime;

      // lag = avg - vruntime
      //   正值:任务落后,应被补偿
      //   负值:任务领先,应被惩罚
      se->lag = (i64)avg - (i64)se->vruntime;
      se->on_rq = false;

      rb_erase(&rq->fair_tree, se);
      rq->fair_nr--;
      update_avg_vruntime(rq);
  }

  // 入队:根据 lag 恢复 vruntime
  void eevdf_enqueue(Task* task, Runqueue* rq, u64 now_ns) {
      SchedEntity* se = &task->sched;
      u64 avg = rq->avg_vruntime;

      if (se->lag != 0) {
          // 恢复原有的落后/领先程度
          se->vruntime = (u64)((i64)avg - se->lag);
          se->lag = 0;
      } else {
          // 新任务:vruntime 设为 avg(避免起步即领先)
          se->vruntime = avg;
      }

      // 重新计算 deadline
      se->deadline = se->vruntime
                   + calc_delta_vruntime(se->slice_ns, se->weight);

      se->on_rq = true;
      rb_insert(&rq->fair_tree, se, /*key=*/se->deadline);
      rq->fair_nr++;
      update_avg_vruntime(rq);
  }
  ```
]

=== avg_vruntime 的增量维护

avg_vruntime 是所有就绪任务 vruntime 的加权平均值,它的精确定义是 `avg_vruntime = sum(weight_i * vruntime_i) / sum(weight_i)`。如果每次需要 avg_vruntime 时都重新遍历所有任务计算,代价是 O(n),在高频调度场景下不可接受。EEVDF 通过增量维护两个聚合量来摊销这个开销:`weight_sum` 是所有任务权重之和,`vload_sum` 是所有任务 `weight * vruntime` 之和。任务入队时,`weight_sum += weight`,`vload_sum += weight * vruntime`;任务离队时反向。avg_vruntime 在需要时通过 `vload_sum / weight_sum` 计算,这一除法本身是 O(1)。

vload_sum 还需要随着任务运行而更新。每次调度节拍中,当前任务的 vruntime 推进了 `delta_vruntime`,vload_sum 相应增加 `weight * delta_vruntime`。这一增量更新在 `update_curr` 中完成,与 vruntime 的更新合并,不引入额外的遍历开销。这种增量维护策略将 EEVDF 的合格性检查从 O(n) 摊销到 O(1),使得 EEVDF 在 n 较大时仍然高效。

== 实时调度类与截止时间调度类

实时调度类的核心特征是优先级驱动而非权重驱动。RT 任务有一个静态的优先级(rt_priority),取值范围是 1 到 99,数字越大优先级越高。调度器在选择 RT 任务时,总是选择优先级最高的任务,在同优先级任务之间根据策略(FIFO 或 RR)决定顺序。RT 任务可以抢占任何 Fair 任务,但只能被更高优先级的 RT 任务或任何 DL 任务抢占。

之所以 RT 类不使用权重而使用静态优先级,是因为 RT 类的应用场景需要确定性的时间保证。一个音频处理任务每 5 毫秒需要处理一帧音频数据,如果它使用权重调度,某个时刻其他任务的权重之和增大,音频任务获得的 CPU 时间就会减少,可能错过 5 毫秒的截止时间;如果使用静态优先级,只要音频任务的优先级足够高,它就能在需要时立即获得 CPU,不受其他任务的影响。这种"硬性优先"的语义对于实时任务是必需的,而 Fair 类的"软性公平"语义无法满足。

#pseudo-sample("6-5", [实时调度类的实现], kind: "代码")[
  ```c
  // RT 调度实体
  struct RtSchedEntity {
      u8  rt_priority;       // 静态优先级(1..99)
      u64 rr_remaining_ns;   // SCHED_RR 时间片剩余
      List<Task> rt_node;    // 同优先级链表节点
  };

  // RT 运行队列:每优先级一个 FIFO 队列
  struct RtRunqueue {
      List<Task> queues[100];   // 索引即优先级
      u128 active_bitmap;        // 哪些优先级有任务(bitmap)
      u32  nr_running;
  };

  void rt_enqueue(Task* t, Runqueue* rq, u64 now_ns) {
      RtSchedEntity* se = &t->rt;
      RtRunqueue* rt = &rq->rt;

      list_add_tail(&rt->queues[se->rt_priority], &se->rt_node);
      bitmap_set(&rt->active_bitmap, se->rt_priority);
      rt->nr_running++;

      if (t->policy == SCHED_RR) {
          se->rr_remaining_ns = t->slice_ns;
      }
  }

  Task* rt_pick_next(Runqueue* rq, u64 now_ns) {
      RtRunqueue* rt = &rq->rt;
      if (rt->nr_running == 0) return NULL;

      // 找到 active_bitmap 中最高的位
      int prio = bitmap_find_highest(&rt->active_bitmap);
      return list_first(&rt->queues[prio]);
  }

  void rt_tick(Task* t, Runqueue* rq, u64 now_ns) {
      if (t->policy != SCHED_RR) return;

      RtSchedEntity* se = &t->rt;
      if (se->rr_remaining_ns > TICK_NS) {
          se->rr_remaining_ns -= TICK_NS;
      } else {
          // 时间片用完:挪到队尾,重置时间片
          se->rr_remaining_ns = t->slice_ns;
          list_move_tail(&rq->rt.queues[se->rt_priority], &se->rt_node);
          set_need_resched(t);
      }
  }
  ```
]

RT 调度类同样使用有序树(`BTreeMap`)存储就绪任务,键的主分量是倒序优先级(优先级越高,键越小),次分量是入队序号(保证同优先级内的 FIFO 语义)。这种设计使得"取最高优先级任务"等价于取树的最左节点,时间复杂度为 O(log n)。之所以选择与 Fair 类相同的有序树结构而非 Linux 中的 bitmap+链表桶,是因为在本系统当前的 RT 任务规模下,O(log n) 与 O(1) 的差异微乎其微,而统一的数据结构简化了代码维护和正确性论证。如果未来 RT 任务数量显著增长,可以在不改变调度类接口的前提下将内部数据结构替换为桶结构。

截止时间调度类(DL)的语义比 RT 更进一步，它不仅保证任务能获得 CPU,还保证任务在指定的截止时间前完成指定的运行预算。DL 任务的三参数 (runtime, deadline, period) 描述了一个周期性时间约束:在每个 period 内,任务最多运行 runtime 时间;每次任务被激活后,它必须在 deadline 时间内消耗完 runtime 预算。这种约束要求调度器进行"准入控制"，当新的 DL 任务请求加入时,调度器检查系统的总 DL 利用率(所有 DL 任务的 runtime/period 之和)是否超过某个阈值(通常是 0.95),超过则拒绝加入。准入控制是 DL 类与其他调度类的关键区别，Fair 和 RT 类对任务的数量没有硬性限制,只是任务多了之后服务质量下降;DL 类则通过准入控制保证已接受的任务一定能满足时间约束。

DL 类使用 EDF(Earliest Deadline First,最早截止时间优先)算法选择下一个任务。EDF 是单核环境下的最优调度算法，只要系统的总利用率不超过 1,EDF 就能保证所有任务满足截止时间。EDF 的实现也是 O(log n) 的红黑树,键是任务的绝对截止时间(`abs_deadline = enqueue_time + relative_deadline`)。EDF 的最优性是通过严格的数学证明的,但它的最优性只在"任务到达时间已知"且"任务运行时间精确等于预算"的理想模型下成立，现实中的任务可能运行时间不准、预算可能被超出,因此 DL 类还需要预算监控和超额惩罚机制,这些机制将在第七章中讨论。

== 调度类的协作与抢占

三个调度类共存于同一个调度器中,通过严格的优先级顺序协作。调度核心的 `pick_next_task` 方法依次询问 DL、RT、Fair 三个调度类,直到某一类返回非空结果。这种"按优先级遍历"的策略保证了高优先级类的任务永远优先于低优先级类,即使低优先级类积累了再多的 vruntime。

#pseudo-sample("6-6", [调度核心的任务选择], kind: "代码")[
  ```c
  // 调度类的全局优先级数组(从高到低)
  static SchedClass* sched_classes[] = {
      &dl_class,
      &rt_class,
      &fair_class,
      &idle_class,  // 最后兜底:idle 任务
      NULL,
  };

  Task* pick_next_task(Runqueue* rq, u64 now_ns) {
      for (int i = 0; sched_classes[i]; i++) {
          Task* next = sched_classes[i]->pick_next(rq, now_ns);
          if (next) return next;
      }
      // 不可达:idle 类总是返回 idle 任务
      panic("no task picked");
  }

  // 抢占检查:新唤醒的任务是否应该抢占当前任务
  bool should_preempt(Task* curr, Task* wakee) {
      // 跨调度类:高优先级类抢占低优先级类
      if (wakee->sched_class->priority < curr->sched_class->priority)
          return true;
      if (wakee->sched_class->priority > curr->sched_class->priority)
          return false;

      // 同类内部:由调度类自己判断
      return curr->sched_class->check_preempt(curr, wakee);
  }
  ```
]

抢占的语义在不同调度类之间有所不同。Fair 类内部的抢占基于 EEVDF 的合格性和 deadline，新唤醒的任务如果是合格的且 deadline 早于当前任务,则抢占。RT 类内部的抢占基于优先级，新唤醒的任务优先级高于当前任务则抢占,同优先级任务不抢占。DL 类内部的抢占基于绝对截止时间，新唤醒的任务 deadline 早于当前任务则抢占。跨调度类的抢占总是高优先级类抢占低优先级类,无论低优先级类的任务积累了多大的"应得份额"。

这种抢占规则带来了一个重要的副作用，长时间运行的高优先级 RT 任务可能让 Fair 任务完全得不到 CPU 时间。这种现象被称为"RT 饥饿"。系统通过 `rt_runtime_us` 和 `rt_period_us` 两个参数限制 RT 任务的总 CPU 占比(默认是 950ms / 1000ms = 95%),保留至少 5% 的 CPU 时间给 Fair 类,防止 RT 任务完全垄断 CPU。这是一种"软实时"妥协，它牺牲了 RT 类的硬性优先语义,换取了系统的整体可用性。完全的硬实时系统(如 RTOS)不提供这种妥协,RT 任务可以无限占用 CPU,这要求 RT 任务的开发者自己保证不会过度占用，而这种保证通常通过形式化分析或仔细的代码审查来达成。

== 运行队列与每 CPU 设计

运行队列(Runqueue)是调度器持有就绪任务的数据结构。每个 CPU 核心拥有一个独立的运行队列,这是"每 CPU 运行队列"设计。每 CPU 运行队列相比于全局共享运行队列,最关键的优势在于消除了多核之间的锁竞争，CPU0 上的调度决策不需要等待 CPU1 上的调度操作完成,两者可以完全并行。在核心数较多的系统(如 32 核或 64 核)上,全局共享运行队列的锁竞争会成为严重的性能瓶颈,调度延迟会随着核心数线性增长;而每 CPU 运行队列将这种延迟限制在每个核心独立的范围内。

#pseudo-sample("6-7", [运行队列结构与每 CPU 数组], kind: "代码")[
  ```c
  // 单个 CPU 的运行队列
  struct Runqueue {
      Spinlock lock;            // 运行队列锁

      // 各调度类的子运行队列
      FairRunqueue  fair;       // EEVDF 红黑树
      RtRunqueue    rt;         // 每优先级 FIFO 队列
      DlRunqueue    dl;         // EDF 红黑树

      Task* current;            // 当前正在运行的任务
      Task* idle;               // 该 CPU 的 idle 任务

      // EEVDF 聚合量(增量维护)
      u64 avg_vruntime;
      u64 vload_sum;            // sum(weight_i * vruntime_i)
      u64 weight_sum;           // sum(weight_i)

      atomic_u32 nr_running;    // 总就绪任务数(跨所有调度类)
      u32 cpu_id;
      u64 clock_ns;             // 该 CPU 的调度时钟

      // 负载跟踪与迁移辅助
      u64 last_balance_ns;
      atomic_u8 balance_flags;
  };

  // 每 CPU 运行队列数组
  static Runqueue runqueues[NR_CPUS];

  // 访问当前 CPU 的运行队列(禁用抢占以保证 cpu_id 不变)
  Runqueue* this_rq(void) {
      preempt_disable();
      return &runqueues[smp_processor_id()];
  }
  ```
]

每 CPU 运行队列还带来了缓存亲和性的间接收益。一个任务被调度到某个 CPU 后,它访问的数据会被加载到该 CPU 的私有缓存中(L1、L2),如果任务持续在同一个 CPU 上运行,这些缓存命中率很高。如果任务被频繁迁移到其他 CPU,新 CPU 的缓存中没有该任务的数据,需要从主存或共享缓存(L3)加载,造成"冷启动"的性能损失。每 CPU 运行队列通过让任务"粘"在初始 CPU 上,自然地保留了缓存亲和性,只有在显式的负载均衡需要时才迁移任务。

之所以三个调度类的子运行队列共存于同一个 Runqueue 中,而不是各自独立的运行队列,是因为它们共享同一把锁 `rq->lock`。这把锁保护了 Runqueue 的所有字段,包括三个子队列、`current` 指针、`avg_vruntime` 等聚合量。共享锁的设计虽然增加了锁的范围,但简化了正确性论证，调度核心只需要持有一把锁就能完成跨调度类的操作(如从 Fair 切换到 RT),不需要按特定顺序获取多把锁。这与第五章亲缘关系的集中式锁设计是同样的权衡，用一定的并发度换取可调试性和正确性。

=== EEVDF 子队列的红黑树

Fair 调度类的子运行队列是一棵红黑树,键是调度实体的 deadline。红黑树是一种自平衡的二叉搜索树,它保证树的高度始终是 O(log n),因此插入、删除和查找操作的时间复杂度都是 O(log n)。在调度器场景中,选择"deadline 最小的合格任务"等价于在红黑树中查找最左节点(最小键),时间复杂度是 O(log n)。

之所以选择红黑树而不是堆或链表,是因为红黑树在三个关键操作上都达到了对数复杂度:插入 O(log n)(任务唤醒)、删除 O(log n)(任务睡眠或时间片用完)、查找最左 O(log n)(选择下一个任务)。堆虽然在"取最小"上是 O(1),但它不支持"删除任意元素"这种调度器需要的操作(任务可能从中间被移除,如响应抢占请求)。链表虽然在"插入"和"删除"上是 O(1),但"取最小"需要 O(n) 遍历。红黑树在所有三个操作上都达到了 O(log n),是最平衡的选择。Linux 内核的 CFS 和 EEVDF 都使用红黑树作为公平调度的数据结构,这一选择经过了大规模生产环境的验证。

#pseudo-sample("6-8", [运行队列入队与出队], kind: "代码")[
  ```c
  void rq_enqueue_task(Runqueue* rq, Task* task, u64 now_ns) {
      spin_lock(&rq->lock);

      // 委托给具体调度类
      task->sched_class->enqueue(task, rq, now_ns);

      atomic_inc(&rq->nr_running);
      task->on_rq = true;

      // 如果新任务能抢占当前任务,设置重调度标志
      if (rq->current && should_preempt(rq->current, task)) {
          set_need_resched(rq->current);
      }

      spin_unlock(&rq->lock);
  }

  Task* rq_pick_next(Runqueue* rq, u64 now_ns) {
      spin_lock(&rq->lock);

      Task* next = pick_next_task(rq, now_ns);

      // 从对应的子队列中暂时取出(标记为 running)
      if (next != rq->idle) {
          next->sched_class->dequeue(next, rq);
          atomic_dec(&rq->nr_running);
      }
      next->on_rq = false;
      rq->current = next;

      spin_unlock(&rq->lock);
      return next;
  }

  void rq_put_prev(Runqueue* rq, Task* prev, u64 now_ns) {
      spin_lock(&rq->lock);

      // 如果前一任务仍然可运行,重新入队
      if (prev->state == RUNNABLE && prev != rq->idle) {
          prev->sched_class->enqueue(prev, rq, now_ns);
          atomic_inc(&rq->nr_running);
          prev->on_rq = true;
      }

      spin_unlock(&rq->lock);
  }
  ```
]

== 时钟节拍与调度时机

调度器的"心跳"由定时器中断驱动。系统配置一个固定频率(通常是 100Hz、250Hz 或 1000Hz)的定时器,每次定时器触发都会调用调度节拍处理函数。节拍处理函数更新当前任务的 vruntime、检查时间片是否用完、检查是否有更紧迫的任务需要抢占,并在需要时设置 `TIF_NEED_RESCHED` 标志。这个标志不会立即触发上下文切换，切换发生在系统调用返回路径或中断返回路径上,当代码即将返回到一个安全的点(没有持有锁、没有处于关键区)时,调度器才真正执行上下文切换。

#pseudo-sample("6-9", [调度节拍与重调度时机], kind: "代码")[
  ```c
  // 节拍频率(Hz)
  #define HZ 250
  #define TICK_NS (1000000000ULL / HZ)

  // 定时器中断:每 TICK_NS 调用一次
  void scheduler_tick(void) {
      Runqueue* rq = this_rq();
      Task* curr = rq->current;
      u64 now = sched_clock_ns();

      spin_lock(&rq->lock);
      rq->clock_ns = now;

      // 1. 更新当前任务的运行时统计
      curr->sched_class->tick(curr, rq, now);

      // 2. 周期性触发负载均衡(每 N 个节拍)
      if (now - rq->last_balance_ns > BALANCE_INTERVAL_NS) {
          rq->balance_flags |= NEED_BALANCE;
      }

      spin_unlock(&rq->lock);

      // 3. 在中断返回路径上检查 TIF_NEED_RESCHED
  }

  // 中断或系统调用返回路径上的调度检查
  void check_resched_on_return(void) {
      Task* curr = current_task();
      if (test_tsk_thread_flag(curr, TIF_NEED_RESCHED)) {
          schedule();
      }
  }

  // 调度的主入口
  void schedule(void) {
      preempt_disable();

      Runqueue* rq = this_rq();
      Task* prev = rq->current;
      u64 now = sched_clock_ns();

      // 投递信号(可能改变任务状态为 RUNNABLE)
      deliver_pending_signals(prev);

      // 处理前一任务
      rq_put_prev(rq, prev, now);

      // 选择下一任务
      Task* next = rq_pick_next(rq, now);

      clear_tsk_thread_flag(prev, TIF_NEED_RESCHED);

      // 执行上下文切换
      if (next != prev) {
          context_switch(prev, next);
      }

      preempt_enable();
  }
  ```
]

之所以将"标记需要重调度"和"实际执行重调度"两步分离,是因为定时器中断可能发生在任意位置,包括内核的关键区中。如果中断处理函数直接执行上下文切换,可能导致任务在持有自旋锁的情况下被切换出去,其他等待该锁的任务永远无法获得锁，这就是第五章讨论过的"持锁睡眠"死锁。延迟到"返回用户态前"或"释放最后一把自旋锁后"再执行实际切换,保证了切换发生在安全点。这种"标记 + 延迟执行"的模式在内核中被广泛使用,它将异步事件(中断)与同步动作(上下文切换)解耦,使得异步事件的处理不会破坏代码执行的关键区不变量。

时钟节拍频率的选择是一个权衡。高频率(如 1000Hz)使得调度决策更精细,响应延迟更低,但中断本身的开销也更大，每秒 1000 次中断的开销在功耗敏感的设备上是不可忽略的。低频率(如 100Hz)减少了中断开销,但调度的最小粒度变粗,响应延迟增加。现代系统通常采用动态节拍机制(NO_HZ),在 CPU 空闲时关闭节拍以节省功耗,只在有任务运行时启用节拍。本系统采用 250Hz 作为默认频率,在响应性和开销之间取得平衡。

== 上下文切换

上下文切换是调度器最频繁的高代价操作。它将 CPU 的执行从一个任务切换到另一个任务,包括寄存器的保存和恢复、地址空间的切换、内核栈的切换。第五章已经讨论过 Task 层面的上下文切换数据结构和不变量,本节关注调度器视角的切换流程和优化技术。

#pseudo-sample("6-10", [上下文切换的完整流程], kind: "代码")[
  ```c
  void context_switch(Task* prev, Task* next) {
      Runqueue* rq = this_rq();

      // 1. 切换地址空间(如果不同)
      if (prev->mm != next->mm) {
          // 激活新地址空间的页表
          switch_mm(prev->mm, next->mm);

          // ASID 优化:不同地址空间使用不同的 ASID
          // 避免 TLB 完全 flush
          if (cpu_has_asid()) {
              load_asid(next->mm->asid);
          } else {
              flush_tlb();
          }
      } else {
          // 同地址空间(线程间切换):无需切换页表
      }

      // 2. 设置内核陷阱栈(下次中断写入正确的栈)
      set_kernel_trap_stack(next->kstack.top);

      // 3. 更新 per-CPU 的 current 指针
      __this_cpu_write(current_task, next);

      // 4. 架构相关的寄存器切换(汇编实现)
      arch_switch_context(&prev->ctx, &next->ctx);

      // 此处之后,代码已在 next 任务的内核栈上执行
      // prev 任务被挂起,等待下次被调度
  }
  ```
]

上下文切换的开销可以分为直接开销和间接开销两类。直接开销是切换本身的指令开销，包括保存和恢复寄存器、切换栈、切换页表,这部分通常在数百个时钟周期内完成。间接开销是切换导致的缓存效应，如 TLB(Translation Lookaside Buffer)失效、L1/L2 缓存被新任务的工作集污染、分支预测器被重置、流水线被打断。间接开销通常远大于直接开销,可以达到数千甚至数万个时钟周期,具体取决于任务的内存访问模式。

ASID(Address Space Identifier)是减少 TLB 切换开销的关键优化。在没有 ASID 的处理器上,每次切换地址空间都需要 flush 整个 TLB,使得新任务的所有地址转换都需要重新查询页表;有 ASID 的处理器为每个地址空间分配一个 ID,TLB 条目带有 ASID 标签,切换地址空间只需要更新当前 ASID 而不需要 flush TLB,旧 ASID 的条目仍然保留,任务再次被调度时可以直接使用。这种优化使得高频的任务切换不会导致 TLB 抖动,显著降低了切换的间接开销。RISC-V 的 SATP 寄存器和 ARM 的 TTBR0_EL1 寄存器都支持 ASID,本系统在这些架构上启用 ASID 优化。

线程间的切换比进程间的切换便宜，因为同一进程的线程共享地址空间,切换时不需要切换页表,也不会导致 TLB 失效。这种差异是 `clone(CLONE_VM)` 创建的线程相比于 `fork` 创建的进程的核心性能优势之一。在多线程程序中,线程之间的上下文切换的间接开销远小于进程间切换,这使得多线程模型在高并发场景下比多进程模型更高效。这一性能差异在 Web 服务器、数据库等高并发应用中尤为显著。

== 多核负载均衡

多核系统中,每 CPU 运行队列的设计带来了一个新的问题，即 CPU 之间的负载可能不均衡。某个 CPU 上可能堆积了几十个就绪任务,而另一个 CPU 上只有 idle 任务在跑;前者上的任务等待时间会很长,后者上的 CPU 资源被浪费。负载均衡机制定期检查各 CPU 的负载,在必要时将任务从高负载 CPU 迁移到低负载 CPU,以提高整体的 CPU 利用率和任务的服务质量。

=== 负载度量

负载均衡的第一个问题是"如何度量负载"。本系统采用最直接的度量方式：CPU 上就绪任务的数量(`nr_running`)。这种度量虽然不如 Linux 的 PELT（Per-Entity Load Tracking）那样精细（PELT 会考虑任务的权重和历史 CPU 占用率），但在当前的使用场景下已经足够，且实现简单、开销极低。均衡算法只需读取每个运行队列的任务计数，不需要维护额外的统计数据结构。

#pseudo-sample("6-11", [基于任务数量的负载均衡], kind: "代码")[
  ```c
  // 从最忙 CPU 拉一个任务到当前 CPU
  bool balance_once(int cpu_id) {
      int local_load = runqueues[cpu_id].nr_running;
      int busiest = -1;
      int busiest_load = local_load;

      for (int other = 0; other < NR_CPUS; other++) {
          if (other == cpu_id || !is_cpu_online(other))
              continue;
          int load = runqueues[other].nr_running;
          // 只在差距超过 1 时才触发迁移,避免乒乓
          if (load > busiest_load + 1) {
              busiest = other;
              busiest_load = load;
          }
      }
      if (busiest < 0) return false;

      // 从最忙队列中取一个可迁移的任务
      Task* task = runqueues[busiest].take_migratable(cpu_bit(cpu_id));
      if (!task) return false;

      task->current_cpu = cpu_id;
      runqueues[cpu_id].enqueue(task, now_ns());
      request_resched(cpu_id);
      return true;
  }
  ```
]

之所以选择"差距超过 1 才迁移"的阈值而非严格均衡,是为了避免任务在两个负载相近的 CPU 之间反复迁移（即"乒乓效应"）。每次迁移都会导致缓存冷启动,如果两个 CPU 各有 3 个任务就触发迁移,迁移后变成 2:4 又会触发反向迁移,系统将在无意义的迁移中浪费大量缓存预热时间。阈值为 1 保证了只有在明显不均衡时才介入。

=== 均衡策略与迁移

负载均衡的触发时机有两个：时钟节拍路径和 idle 路径。当 CPU 在时钟节拍中发现自己的运行队列为空或负载较低时,调用 `balance_once` 尝试从最忙的 CPU 拉取一个任务。idle 路径同理,当 CPU 即将进入 idle 循环前,先尝试一次均衡,避免在有任务可运行的情况下让 CPU 空转。

本系统的均衡算法采用扁平扫描策略,遍历所有在线 CPU 找到负载最高者,然后从中取出一个可迁移的任务。之所以不引入 Linux 式的调度域（SchedDomain）层次结构来感知 NUMA 拓扑,是因为当前目标平台的核心数有限（最多 8 核）,扁平扫描的开销可以忽略不计。调度域的价值在大型 NUMA 系统（32 核以上）中才会显现,在小规模系统中引入层次化均衡反而增加了代码复杂度而收益甚微。如果未来需要支持大规模多核,可以在 `balance_once` 之上叠加拓扑感知层,而不需要修改运行队列和调度类的代码。

=== 唤醒均衡

除了周期性的负载均衡,任务唤醒时也是一个重要的均衡时机。当一个睡眠任务被唤醒时,调度器需要决定将它放到哪个 CPU 的运行队列上。最简单的策略是放回它睡眠前所在的 CPU(保持缓存亲和性),但如果该 CPU 当前负载很高而其他 CPU 空闲,放到其他 CPU 上可能更好。唤醒均衡(`select_task_cpu`)算法在这两个考量之间做权衡:如果任务此前所在的 CPU 仍然在线且在亲和性掩码内,优先放回该 CPU;否则遍历所有允许的 CPU,选择 `nr_running` 最小的那个。

唤醒均衡相比于周期性均衡的优势在于响应速度，它在任务唤醒的瞬间就做出决策,使得任务一开始就在合适的 CPU 上运行,而不需要等待下一次周期性均衡。劣势是它只能基于当时的瞬时负载做决策,可能不如周期性均衡那样具有全局视角。两者结合使用,周期性均衡处理粗粒度的负载倾斜,唤醒均衡处理细粒度的任务放置,共同维持系统的整体平衡。

== 调度器初始化与 idle 任务

调度器的初始化发生在内核启动的中后期,在内存管理和中断子系统就绪之后,但在用户进程启动之前。初始化的核心任务包括:为每个 CPU 分配运行队列结构、创建 idle 任务、注册各调度类、设置定时器中断、激活 SMP(对称多处理)。

#pseudo-sample("6-13", [调度器初始化与 idle 任务], kind: "代码")[
  ```c
  void sched_init(void) {
      // 1. 初始化每 CPU 运行队列
      for (int cpu = 0; cpu < nr_cpus; cpu++) {
          Runqueue* rq = &runqueues[cpu];
          spinlock_init(&rq->lock);
          rb_root_init(&rq->fair.tree);
          rt_rq_init(&rq->rt);
          dl_rq_init(&rq->dl);
          rq->cpu_id = cpu;
          rq->avg_vruntime = 0;
          rq->vload_sum = 0;
          rq->weight_sum = 0;
      }

      // 2. 创建每 CPU 的 idle 任务
      for (int cpu = 0; cpu < nr_cpus; cpu++) {
          Task* idle = create_idle_task(cpu);
          runqueues[cpu].idle = idle;
          runqueues[cpu].current = idle;
      }

      // 3. 注册定时器中断
      setup_periodic_timer(HZ);

      // 5. 标记调度器就绪
      smp_wmb();
      sched_ready = true;
  }

  // idle 任务:CPU 空闲时执行
  Task* create_idle_task(int cpu) {
      Task* idle = task_alloc();
      idle->name = "swapper";
      idle->state = RUNNING;
      idle->sched_class = &idle_class;
      idle->cpu = cpu;
      cpumask_only(&idle->cpu_affinity, cpu);  // 绑定到指定 CPU
      idle->kstack = kstack_alloc(KSTACK_SIZE);
      arch_init_context(idle, /*entry=*/idle_loop);
      return idle;
  }

  // idle 任务的主循环
  void idle_loop(void) {
      while (1) {
          // 1. 检查是否有就绪任务(避免错过)
          if (atomic_read(&this_rq()->nr_running) > 0) {
              schedule();
              continue;
          }

          // 2. 进入低功耗状态(等待中断)
          // RISC-V: WFI 指令; x86: HLT 指令
          arch_cpu_idle();

          // 3. 中断唤醒后,检查是否需要重调度
          if (test_thread_flag(TIF_NEED_RESCHED)) {
              schedule();
          }
      }
  }
  ```
]

idle 任务是每个 CPU 上独有的特殊任务,它在 CPU 没有其他任务可运行时被调度。idle 任务的核心职责是让 CPU 进入低功耗状态，在 RISC-V 上执行 WFI(Wait For Interrupt)指令,在 x86 上执行 HLT 指令。这些指令使 CPU 进入低功耗模式,直到下一个中断到达。低功耗模式的存在是现代处理器的重要特性，它使得空闲的 CPU 不会无谓地消耗电力,显著延长了移动设备的电池寿命,也减少了数据中心的电力开销和散热成本。

idle 任务还有一个非显然的职责，它是系统启动的第一个任务。在内核启动过程中,引导代码运行在某个 CPU 的初始内核栈上,这个执行流随着调度器的初始化逐步演变为该 CPU 的 idle 任务。换句话说,idle 任务"包含"了内核的启动流程,启动完成后,这个执行流就退化为 idle 循环。这种设计使得内核的启动和运行使用同一套基础设施,不需要为启动阶段维护一套独立的"原始执行流"。

之所以 idle 任务是一个独立的调度类(`idle_class`)而不是一个普通的低优先级 Fair 任务,是因为 idle 任务的语义是"只在没有其他任务时运行",这种语义无法用权重精确表达，因为任何有限的低权重都意味着它仍然会获得一定的 CPU 时间份额,即使其他高权重任务存在。`idle_class` 的 `pick_next` 总是返回 idle 任务,但它在调度类优先级中排在最末,只有当 Fair、RT、DL 三类都没有就绪任务时才会被选中。这种"兜底"语义使得调度器永远有任务可选,简化了调度核心的边界条件处理。

== 工程设计总结

调度系统的设计围绕三个相互冲突的目标展开:公平性、响应性和吞吐量。EEVDF 算法通过虚拟时间映射权重保证了公平性,通过合格性筛选避免了睡眠奖励问题,通过 lag 机制实现了跨睡眠周期的精确公平,通过 deadline 选择保证了响应性。这些机制相比于 CFS 都是改进，CFS 的"贪婪选最小 vruntime"虽然简洁,但在长时间睡眠唤醒、抢占决策、延迟敏感任务等场景下都存在精度不足的问题。EEVDF 通过引入合格性和 lag 这两个独立的维度,使得调度决策更加精确,同时保留了 CFS 的简洁性，红黑树仍然按一个键(deadline)排序,主要的判断逻辑仍然是 O(log n) 的。

调度类的分层设计是策略多样性的关键。不同任务有不同的调度需求，交互任务需要响应性,批处理任务需要吞吐量,实时任务需要确定性,这些需求的本质差异使得任何单一的调度算法都无法同时满足。通过将调度策略抽象为调度类接口,并允许多个调度类共存于同一个调度器中,系统能够为每种任务提供最合适的策略。三类调度类按 DL > RT > Fair 的优先级协作,既保证了高优先级类的时间保证,又通过 RT 配额机制避免了低优先级类的饥饿。这种"分层 + 配额"的协作模式是一种典型的工程妥协，理论上完美的硬实时系统拒绝任何配额限制,但实践中的通用操作系统必须保证整体的可用性,因此引入软实时妥协。

每 CPU 运行队列设计是多核扩展性的基础。它通过将运行队列分布到每个 CPU,消除了多核之间在调度操作上的锁竞争,使得调度延迟不随核心数线性增长。每 CPU 设计带来的负载不均衡问题通过扁平的负载均衡机制解决，周期性均衡从最忙 CPU 拉取任务,唤醒均衡在任务放置时选择最空闲的 CPU,两者结合维持系统的整体平衡。这种设计在大多数情况下(任务在自己的 CPU 上运行)是无锁的,只在均衡时机才需要跨 CPU 协调,使得调度的快路径(每次时钟节拍、每次唤醒)可以达到极低的延迟。

上下文切换的优化是高频调度的性能基础。直接开销(寄存器保存恢复、栈切换)通过紧凑的汇编代码已经接近硬件下限;间接开销(TLB 失效、缓存污染)通过 ASID 和缓存亲和迁移等机制被显著降低。线程间切换比进程间切换便宜的特性,使得多线程模型在高并发场景下成为首选，这种特性在第五章的 `clone(CLONE_VM)` 设计中已经埋下伏笔,在调度层面得到充分利用。

回顾整个调度系统的设计,可以发现一个贯穿始终的主题，即"将复杂性分摊到合适的层次"。EEVDF 将权重公平性和延迟敏感性分摊到 vruntime 和 deadline 两个维度;调度类将策略多样性分摊到三个独立的实现;每 CPU 运行队列将并发竞争分摊到每个核心独立的局部空间;负载均衡将跨核协调限制在低频的均衡时机。每一层只解决自己面对的问题,不越界、不耦合,这种"分而治之"的设计哲学与第二章的内存管理、第三章的设备模型、第四章的 VFS、第五章的进程管理是一脉相承的。它不是某个章节的局部技巧,而是本系统应对复杂性的基本策略,贯穿所有子系统的设计。

调度系统是用户感知最直接的子系统之一，它的好坏直接决定了系统的"流畅度"。一个高效的调度器应该在大多数情况下"隐形",用户感觉不到它的存在;只有在极端负载下才需要用户调整调度参数(如 nice 值、CPU 亲和性、调度策略)。本系统的调度器通过 EEVDF 的精确算法、调度类的策略多样性、每 CPU 运行队列的可扩展性和负载均衡的拓扑感知,在大多数工作负载下都能达到这种"隐形"状态，这是工程设计的最高目标,也是调度系统几十年演化的终点。

