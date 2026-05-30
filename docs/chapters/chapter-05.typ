#import "../config.typ": project-name
#import "../styles/diagram.typ": flow-arrow, flow-node, layer-card
#import "../styles/figure.typ": continued-table, figure-caption, pseudo-sample
#import "../styles/tokens.typ": handoff-fill, soft-fill, stable-fill, warm-fill

= 第五章 进程与线程管理

操作系统内核的各个子系统，无论是 VFS、内存管理还是设备模型，都隐含了一个前提：存在一个"进程"的概念，每个进程拥有自己的文件描述符表、地址空间和凭据。VFS 的能力冻结模型在打开文件时固化了权限边界，文件描述符表管理着进程打开的所有文件，挂载命名空间为不同进程提供了不同的文件系统视图。本章将讨论进程与线程管理子系统如何实现这些前提，以及它如何与 VFS、信号和调度等子系统协同工作。

进程与线程是操作系统中最基本的执行实体抽象。进程是资源分配的单位，拥有独立的地址空间、文件描述符表和凭据；线程是调度的单位，共享所属进程的地址空间和资源，但拥有独立的执行上下文和栈。在现代操作系统中，进程和线程的边界并非固定不变，`clone` 系统调用允许调用方精确控制子任务与父任务之间共享哪些资源，从而在进程和线程之间创建出各种中间形态。本系统的进程与线程管理遵循同样的设计哲学：`Task` 是调度器管理的最小实体，它既可以代表一个独立的进程，也可以代表一个共享地址空间的线程，具体取决于创建时指定的共享标志。

这种统一的设计带来了显著的简化效果。调度器不需要区分进程和线程，它只看到 Task，所有调度决策都基于 Task 的状态和优先级。信号处理不需要为进程和线程维护两套机制，每个 Task 都有自己的信号掩码和待处理信号队列，线程组共享的信号则通过共享信号结构管理。等待和退出机制也不需要区分进程退出和线程退出，Task 的退出状态和回收逻辑对所有类型的 Task 都是一致的。

== 任务抽象与身份模型

Task 是调度器管理的最小实体。与传统设计不同的是，本系统不使用整数 PID 作为任务的主键，任务身份即引用计数指针本身。父子关系通过弱引用和强引用直接互引，信号传递、IPC 和等待都基于句柄而非全局整数表。

这种设计选择背后的动机是消除 PID 分配的全局竞争。在传统实现中，PID 分配需要获取全局的 PID 命名空间锁，这在大量并发 `fork` 的场景下会成为瓶颈。使用引用计数指针作为身份标识，任务的创建只需要分配内存，不需要获取任何全局锁。然而 POSIX ABI 要求进程拥有整数 PID，系统调用如 `getpid`、`kill` 和 `waitpid` 都使用整数 PID 标识目标进程。为了满足这一需求，系统引入了 PID 命名层，它在需要对外暴露数字名字时才登场，典型调用者是系统调用入口和 procfs。PID 命名层维护一张注册表，slot 数组存储弱引用，登记不保活，即任务生命由父的子任务列表决定。这种分层设计使得调度核心完全不依赖 PID，PID 只在 ABI 边界上被使用。

#pseudo-sample("5-1", [Task 核心数据结构], kind: "代码")[
  ```c
  // 任务状态机
  enum TaskState {
      NEW,              // 已创建，尚未入运行队列
      RUNNABLE,         // 在运行队列中等待调度
      RUNNING,          // 当前正在某个 CPU 上执行
      SLEEPING,         // 可中断睡眠（可被信号唤醒）
      UNINTERRUPTIBLE,  // 不可中断睡眠（等待同步 I/O）
      STOPPED,          // 被 SIGSTOP 暂停
      ZOMBIE,           // 已退出，等待父回收
      DEAD,             // 已被父回收，等最后引用释放
  };

  struct Task {
      // 调度状态
      SchedEntity sched;            // EEVDF 调度实体
      atomic_u8 state;              // 当前状态（原子 CAS 驱动转换）

      // 亲缘关系（集中式锁保护）
      Spinlock<Relations> rel;
      // Relations 包含：
      //   Weak<Task> parent;        // 父任务（弱引用）
      //   Vec<Arc<Task>> children;  // 子任务（强引用，保活至 wait）
      //   Arc<ThreadGroup> tgroup;  // 线程组
      //   Arc<ProcessGroup> pgroup; // 进程组

      // 执行上下文
      KernelStack kstack;           // 内核栈（16 KiB，ABI 对齐）
      ArchContextSlot ctx;          // 架构相关寄存器缓冲区

      // 信号
      SignalState signal;           // 私有待处理信号 + 屏蔽字
      Arc<SharedSignal> shared_signal;  // 线程组共享信号处理

      // 退出与等待
      atomic_i32 exit_code;         // 退出状态
      WaitQueue exit_waiters;       // 等待本任务退出的队列

      // 子系统侧表（解耦调度器与 VFS/MM）
      Vec<TaskExt> ext;             // 键值存储：VFS 上下文、FdTable、VmSpace
  };
  ```
]

=== 亲缘关系模型

Task 的亲缘关系通过父引用和子列表维护。父引用是弱引用，子列表是强引用向量。这种非对称的引用设计是有意为之的：子任务持有父任务的弱引用使得父任务可以先于子任务退出而不需要遍历所有子任务来清除它们对自己的强引用；父任务持有子任务的强引用保证了子任务在退出后仍然可以被父任务的 `wait` 系统调用回收，直到父任务显式地将子任务从列表中移除。

当父任务先于子任务退出时，子任务需要被移交给 init 任务。移交操作将子任务的父弱引用更新为 init 任务的弱引用，并将子任务加入 init 任务的子列表。这种移交保证了所有子任务最终都有一个存活的父任务来回收它们，避免了僵尸任务永远占用内核资源的情况。init 任务作为所有孤儿任务的收养者会定期调用 `wait` 回收已退出的子任务，防止列表无限增长。

亲缘关系字段由一把自旋锁保护，这把锁同时保护父引用、子列表、退出信息和退出等待队列。集中式锁的设计牺牲了一定的并发度，但消除了多锁反序死锁的风险。在内核开发中死锁是最难调试的问题之一，它不会产生崩溃日志，只会导致系统挂起。将亲缘关系字段集中在一把锁下虽然增加了锁的粒度，但保证了亲缘关系操作的串行化，任何涉及亲缘关系的操作都不需要担心锁的获取顺序。

=== 任务状态机

Task 的生命周期由一个严格的状态机管理。状态转换由调度器内部通过原子 CAS 操作驱动，避免为"取状态"而持有运行队列锁。这种设计的关键优势在于查询任务状态是一个高频操作，调度器的唤醒路径、信号投递路径和等待路径都需要读取任务状态，如果每次读取都需要获取运行队列锁，锁竞争将成为严重的性能瓶颈。原子 CAS 使得状态读取完全无锁，而状态修改在大多数情况下也只需要一次原子的比较并交换操作。

状态机的转换规则是严格的，不是所有状态之间都可以直接转换。例如 Zombie 状态的任务不能被唤醒回 Runnable 状态，New 状态的任务不能直接转换为 Running 状态。这些约束由状态转换方法隐式保证：它接受期望的当前状态和目标状态，只有当前状态与期望状态匹配时才执行转换。如果转换失败，调用方需要根据当前状态决定下一步操作而不是盲目重试。

=== 子系统侧表

Task 通过子系统侧表与 VFS、内存管理等子系统交互。侧表是一个键值存储，键是预定义的类型标识，值是类型擦除的引用计数对象。预定义的键包括 VFS 上下文、文件描述符表、进程地址空间和用户态陷阱帧。

侧表的设计解决了调度器与子系统之间的耦合问题。传统的内核实现中，任务结构体直接包含 VFS 上下文、文件描述符表和地址空间等字段，这意味着调度器的代码需要包含这些子系统的头文件，任何子系统的修改都可能影响调度器的编译。侧表设计将这种依赖关系反转了，子系统依赖调度器提供的侧表接口，但调度器不依赖任何子系统。这种反转使得调度器可以独立编译和测试，也使得新子系统可以在不修改调度器代码的情况下接入。

侧表在 `clone` 时的处理由钩子函数决定。调度器不知道侧表项的内部结构，它把拷贝策略的决策权委托给上层注册的钩子。例如 VFS 上下文在共享文件系统标志下共享否则深拷贝，文件描述符表在共享文件标志下共享否则深拷贝，地址空间在共享内存标志下共享否则写时复制。这种委托策略使得调度器不需要理解每个侧表项的拷贝语义，新侧表项的接入只需要注册对应的钩子函数。

== 线程组、进程组与会话

POSIX 定义了三个层次的任务分组：线程组、进程组和会话。在传统实现中它们分别对应 TGID、PGID 和 SID，在本系统的无 PID 模型里它们全部变成独立的引用计数对象，Task 持有各自的强引用，组成员索引统一使用弱引用。

=== 线程组

线程组是共享同一地址空间和文件描述符表的任务集合，对应 `CLONE_THREAD` 标志。线程组的 leader 是第一个创建的任务，后续通过 `clone(CLONE_THREAD)` 创建的任务加入线程组。`getpid()` 返回的实际是线程组 leader 的身份而不是单个任务的身份，这使得多线程程序中所有线程看到的 PID 是一致的。

线程组的核心数据结构是共享信号状态，它管理线程组共享的信号处理方式和共享待处理信号队列。当信号发送给整个线程组时，信号被投递到共享待处理队列，线程组中的某个任务在从内核返回用户态之前会检查共享待处理队列并处理信号。线程组的成员表使用弱引用，成员退出时弱引用自动失效，不需要显式地从成员表中移除。这种设计避免了成员退出时的锁竞争，成员表的清理是惰性的。

=== 进程组与会话

进程组是作业控制的基本单位。当用户按下 Ctrl-C 时，内核向前台进程组发送 SIGINT 信号，前台进程组中的所有任务都会收到信号。会话是一个登录会话，包含若干进程组，最多一个前台进程组，可关联一个控制终端。守护进程通常通过 `setsid` 创建新的会话脱离原始终端的控制。

== clone 与 fork 机制

`clone` 系统调用是创建新任务的唯一入口，`fork` 和 `vfork` 都是 `clone` 的特例。`clone` 接受一组标志精确控制子任务与父任务之间共享哪些资源。当所有共享标志都被设置时 `clone` 创建的是一个线程；当所有共享标志都不被设置时 `clone` 创建的是一个独立的进程。标志的数值与 Linux UAPI 严格对齐，上层系统调用翻译层可以直接透传而不必做中间映射。

#pseudo-sample("5-2", [clone 操作的三阶段流程], kind: "代码")[
  ```c
  // clone 标志定义（与 Linux UAPI 对齐）
  #define CLONE_VM       0x00000100  // 共享地址空间
  #define CLONE_FS       0x00000200  // 共享 VFS 上下文
  #define CLONE_FILES    0x00000400  // 共享文件描述符表
  #define CLONE_SIGHAND  0x00000800  // 共享信号处理方式
  #define CLONE_THREAD   0x00010000  // 加入父任务的线程组
  #define CLONE_VFORK    0x00004000  // vfork 语义
  #define CLONE_NEWPID   0x20000000  // 新 PID 命名空间

  struct CloneArgs {
      u64 flags;
      void* child_stack;
      u64 stack_size;
      int* parent_tid;
      int* child_tid;
  };

  // 三阶段 clone 实现
  int do_clone(Task* parent, CloneArgs* args) {
      // 阶段一：创建 Task 结构体
      Task* child = task_alloc();
      child->state = NEW;
      lock(&parent->rel);
      child->rel.parent = weak_ref(parent);
      vec_push(&parent->rel.children, arc_clone(child));
      unlock(&parent->rel);

      // 阶段二：复制或共享侧表项
      for (int i = 0; i < N_EXT_HOOKS; i++) {
          ext_clone_hooks[i](parent, child, args->flags);
      }
      // CLONE_VM → 共享 VmSpace
      // CLONE_FILES → 共享 FdTable
      // CLONE_FS → 共享 VfsContext
      // 否则各自深拷贝或写时复制

      // 阶段三：设置执行上下文并激活
      child->kstack = kstack_alloc(KSTACK_SIZE);
      arch_init_context(child, args->child_stack);
      child->state = RUNNABLE;
      enqueue_task(child);

      // vfork：父任务阻塞直到子任务 exec 或 exit
      if (args->flags & CLONE_VFORK) {
          wait_queue_sleep(&child->vfork_done);
      }

      return pid_of(child);  // 返回子任务的 PID
  }
  ```
]

`clone` 的实现分为三个阶段。第一阶段创建 Task 结构体并建立亲缘关系。第二阶段根据标志复制或共享父任务的侧表项，这一阶段的决策权完全委托给各子系统注册的钩子函数。第三阶段分配内核栈、初始化执行上下文并将子任务加入运行队列。

=== vfork 语义

`vfork` 等价于 `clone(CLONE_VFORK | CLONE_VM | SIGCHLD)`。`CLONE_VFORK` 标志使得父任务在子任务调用 `exec` 或 `_exit` 之前阻塞不参与调度。这种阻塞保证了子任务在使用父任务的地址空间期间父任务不会修改地址空间，避免了数据竞争。`vfork` 的存在是为了优化 `fork` 后立即 `exec` 的场景，在写时复制机制出现之前 `fork` 需要复制整个地址空间，`vfork` 通过共享地址空间和阻塞父进程避免了不必要的复制。在现代系统中写时复制已经使得 `fork` 的开销大大降低，但为了向后兼容仍然支持 `vfork`。

== 上下文切换

上下文切换是调度器最核心的操作，它将当前 CPU 的执行从一个任务切换到另一个任务。上下文切换分为三个步骤：保存当前任务的执行上下文到内核栈底部，更新当前 CPU 的当前任务指针，恢复新任务的执行上下文。

#pseudo-sample("5-3", [上下文切换流程], kind: "代码")[
  ```c
  // 架构相关的上下文操作（运行时注入）
  struct ArchContextOps {
      usize context_size;
      usize context_align;
      // 初始化新任务的内核上下文
      void (*init_kernel_context)(void* ctx, usize stack_top,
                                  void (*entry)(usize), usize arg);
      // 切换上下文：保存 prev 寄存器，恢复 next 寄存器
      void (*switch_context)(void* prev_ctx, void* next_ctx);
  };

  // 调度器主入口
  void schedule_once(u64 now_ns) {
      Task* prev = current_task();
      Runqueue* rq = this_cpu_rq();

      // 1. 投递待处理信号
      deliver_pending_signals(prev);

      // 2. 选择下一个任务
      Task* next = rq_pick_next(rq, now_ns);
      if (next == prev) return;  // 无需切换

      // 3. 更新当前任务指针
      set_current_task(next);

      // 4. 设置内核陷阱栈（防止中断写错栈）
      set_kernel_trap_stack(next->kstack.top);

      // 5. 切换地址空间（如果不同）
      if (prev->vm_space != next->vm_space) {
          vm_switch(next->vm_space);
      }

      // 6. 执行寄存器级切换（裸汇编）
      arch_ops->switch_context(prev->ctx, next->ctx);
      // 此处 prev 已被挂起，next 开始执行
  }
  ```
]

上下文切换的正确性依赖于一个不变量：每个任务的内核栈底部始终保存着完整的执行上下文。当任务在运行时栈底的执行上下文是过时的，但当下一次上下文切换发生时过时的执行上下文会被新的执行上下文覆盖。当任务不在运行时栈底的执行上下文是准确的，调度器可以从那里恢复任务的执行。

架构相关的上下文操作通过运行时注入机制与调度器解耦。调度器在启动时调用架构层提供的注册函数获取操作表指针，此后所有上下文操作都通过这个指针间接调用。这种设计使得调度器的代码完全不依赖具体的处理器架构，同一份调度器代码可以在不同架构上运行。

=== 上下文切换与锁的交互

如果任务在持有自旋锁的情况下发生上下文切换，其他等待该锁的任务将永远无法获得锁，因为持有锁的任务不在运行无法释放锁。这种"持锁睡眠"是内核开发中最常见的死锁原因之一。系统通过编码约定（自旋锁的持有时间必须足够短，不允许在持锁期间调用可能阻塞的函数）和调试支持（调试模式下调度器在切换任务时检查当前任务是否持有自旋锁）两种机制防止持锁睡眠。

上下文切换期间禁用本地 CPU 的中断，保证切换过程是原子的，中断处理程序不会看到中间状态。禁用中断的时间窗口很短，只包括寄存器的保存和恢复，不会影响中断的响应延迟。

== 退出与回收

任务的退出和回收是进程生命周期中最后也是最微妙的阶段。退出分为两个阶段：第一阶段是任务自己调用 `exit` 将状态设置为 Zombie 并保留退出信息等待父回收；第二阶段是父任务调用 `wait` 从子列表中移除子任务并释放其引用。

#pseudo-sample("5-4", [两阶段退出与 wait 回收], kind: "代码")[
  ```c
  // 阶段一：任务退出
  void do_exit(Task* task, int exit_code) {
      // 1. 关闭所有文件描述符
      fdtable_close_all(task_ext_get(task, FDTABLE));

      // 2. 释放地址空间
      vm_space_release(task_ext_get(task, VM_SPACE));

      // 3. 从线程组和进程组中移除
      thread_group_remove(task);
      process_group_remove(task);

      // 4. 将子任务移交给 init
      lock(&task->rel);
      for (int i = 0; i < task->rel.children.len; i++) {
          reparent_to_init(task->rel.children[i]);
      }
      task->rel.children.clear();

      // 5. 存储退出码，转换为 Zombie
      atomic_store(&task->exit_code, exit_code);
      atomic_cas(&task->state, RUNNING, ZOMBIE);
      unlock(&task->rel);

      // 6. 向父任务发送 SIGCHLD 并唤醒等待者
      Task* parent = weak_upgrade(task->rel.parent);
      if (parent) {
          signal_send(parent, SIGCHLD);
          wait_queue_wake_all(&task->exit_waiters);
      }

      // 7. 让出 CPU（永不返回）
      schedule_once(now_ns());
  }

  // 阶段二：父任务回收
  int do_wait(Task* parent, int options) {
      lock(&parent->rel);
      while (true) {
          // 扫描子列表寻找 Zombie
          for (int i = 0; i < parent->rel.children.len; i++) {
              Task* child = parent->rel.children[i];
              if (atomic_load(&child->state) == ZOMBIE) {
                  int code = atomic_load(&child->exit_code);
                  vec_remove(&parent->rel.children, i);
                  unlock(&parent->rel);
                  // 释放最后的强引用 → 触发 Task 内存释放
                  arc_drop(child);
                  return code;
              }
          }

          if (options & WNOHANG) {
              unlock(&parent->rel);
              return 0;  // 非阻塞模式，无 Zombie 子任务
          }

          // 阻塞等待子任务退出
          unlock(&parent->rel);
          wait_queue_sleep(&parent->rel.children_exit_waiters);
          lock(&parent->rel);
      }
  }
  ```
]

两阶段退出的设计保证了 `wait` 语义的正确性。子任务在 Zombie 状态下保留了退出信息，父任务可以在任意时刻通过 `wait` 获取这些信息。如果在 `exit` 时立即释放所有资源，父任务就无法获取退出码。僵尸任务不占用 CPU 时间也不占用地址空间，但仍然占用 Task 结构体的内存，如果父任务从不调用 `wait`，僵尸任务将永远存在。孤儿收养机制保证了所有僵尸任务最终都会被 init 回收。

== 等待队列

等待机制允许任务阻塞自己直到某个条件满足。等待机制由等待队列实现，等待队列是一个阻塞任务的列表。任务调用睡眠方法将自己加入等待队列并阻塞，状态从 Running 转换为 Sleeping。其他任务或中断处理程序调用唤醒方法唤醒等待队列中的一个或多个任务，被唤醒的任务状态从 Sleeping 转换为 Runnable。

#pseudo-sample("5-5", [等待队列与弱引用唤醒], kind: "代码")[
  ```c
  struct WaitQueue {
      Spinlock lock;
      Vec<Weak<Task>> waiters;  // 弱引用：不保活任务
  };

  // 阻塞当前任务
  void wait_queue_sleep(WaitQueue* wq) {
      Task* self = current_task();

      lock(&wq->lock);
      // 加入等待队列前先检查信号（消除竞态）
      if (signal_pending(self)) {
          unlock(&wq->lock);
          return;  // 立即返回，由调用方检查 EINTR
      }
      vec_push(&wq->waiters, weak_ref(self));
      atomic_cas(&self->state, RUNNING, SLEEPING);
      unlock(&wq->lock);

      // 让出 CPU
      schedule_once(now_ns());
  }

  // 唤醒一个等待者
  void wait_queue_wake_one(WaitQueue* wq) {
      Task* target = NULL;
      lock(&wq->lock);
      while (wq->waiters.len > 0) {
          Weak<Task> w = vec_pop_front(&wq->waiters);
          target = weak_upgrade(w);  // 失效的弱引用自动跳过
          if (target) break;
      }
      unlock(&wq->lock);

      // 在锁外执行唤醒（避免反向锁依赖）
      if (target) {
          atomic_cas(&target->state, SLEEPING, RUNNABLE);
          enqueue_task(target);
      }
  }
  ```
]

=== 弱引用设计

等待队列使用弱引用而非强引用存储等待者。这种设计选择基于两个考量：等待队列不应保活任务，否则"任务自己等自己"的场景会导致任务永远无法释放；已死任务的弱引用会自动失效，遍历时顺手清掉即可，不需要显式的"unregister"操作。弱引用设计的代价是唤醒时需要将弱引用升级为强引用，这个操作可能失败。唤醒路径在升级失败时跳过该等待者继续尝试下一个，这种"尽力而为"的唤醒策略在大多数场景下是足够的。

=== 信号中断与竞态消除

等待队列支持信号中断：当任务在等待队列上阻塞时如果收到信号应该被唤醒并返回错误。这种"先检查后睡眠"的模式存在竞态，信号可能在检查和睡眠之间到达导致信号丢失。系统的实现通过在持锁期间完成检查和睡眠两个操作来消除竞态条件，信号投递路径在唤醒任务时也需要获取等待队列的锁，保证了信号不会在检查和睡眠之间被遗漏。

=== 唤醒回调在锁外执行

唤醒操作的关键设计是回调在释放等待队列锁之后才被调用。如果唤醒回调在持锁期间执行而回调需要获取运行队列锁，那么锁的获取顺序就是"等待队列锁 → 运行队列锁"。如果其他路径的锁获取顺序是"运行队列锁 → 等待队列锁"，就会形成反序死锁。将回调延迟到锁外执行打破了这种反向依赖，保证了锁的获取顺序的一致性。

== 信号机制

信号是 POSIX 定义的异步通知机制，允许内核或进程向目标进程发送通知。信号的处理方式有三种：默认动作、捕获和忽略。`SIGKILL` 和 `SIGSTOP` 不能被捕获或忽略，它们总是执行默认动作，这是 POSIX 的硬性要求。

=== 信号的两阶段投递

信号的投递分为发送和接收两个阶段。发送阶段将信号加入目标的待处理信号队列；接收阶段在目标从内核返回用户态之前检查待处理队列、取出并处理信号。这种异步投递模型与同步系统调用的设计是分离的，信号的发送方不需要等待接收方处理信号，目标任务在自己的执行节奏中处理信号。

#pseudo-sample("5-6", [信号投递与处理], kind: "代码")[
  ```c
  // 信号发送
  int signal_send(Task* target, int sig) {
      // 1. 权限检查（基于发送者凭据）
      if (!can_signal(current_task(), target, sig))
          return -EPERM;

      // 2. SIGKILL/SIGSTOP 总是生效
      if (sig == SIGKILL || sig == SIGSTOP) {
          force_signal(target, sig);
          return 0;
      }

      // 3. 加入待处理队列
      lock(&target->signal.lock);
      sigset_add(&target->signal.pending, sig);
      unlock(&target->signal.lock);

      // 4. 唤醒目标（如在 SLEEPING 状态）
      if (atomic_load(&target->state) == SLEEPING) {
          wake_for_signal(target);
      }
      return 0;
  }

  // 在返回用户态前投递信号
  void deliver_pending_signals(Task* task) {
      while (true) {
          int sig = sigset_dequeue(&task->signal.pending,
                                   &task->signal.blocked);
          if (sig == 0) break;  // 无未屏蔽信号

          SigAction act = task->shared_signal->actions[sig];
          if (act.handler == SIG_DFL) {
              do_default_action(task, sig);  // 终止/停止/忽略
          } else if (act.handler == SIG_IGN) {
              continue;  // 忽略
          } else {
              // 在用户栈上构造信号帧，跳转到处理函数
              setup_signal_frame(task, sig, &act);
              break;  // 返回用户态执行处理函数
          }
      }
  }
  ```
]

=== 信号屏蔽与 SharedSignal

每个任务维护一个信号屏蔽字，被屏蔽的信号不会被投递而是留在待处理队列中直到屏蔽被解除。`SIGKILL` 和 `SIGSTOP` 不能被屏蔽。待处理信号队列分为私有队列和共享队列：私有队列存储发送给特定任务的信号，共享队列存储发送给整个线程组的信号。

`SharedSignal` 是线程组共享的信号状态，包括信号处理动作表和共享待处理队列。所有线程共享同一个动作表，任何一个线程修改信号处理方式都会影响其他线程。共享待处理队列的取出方法接受调用者的屏蔽字作为参数，跳过被屏蔽的信号，这保证了即使某个线程屏蔽了某种信号，该信号仍然可以被线程组中其他未屏蔽该信号的线程处理。

== PID 命名空间

PID 命名空间是容器隔离的基础设施之一。在传统的单一 PID 空间中，所有进程共享同一个整数 PID 空间，容器内的进程可以通过 `/proc` 看到容器外的所有进程，这违反了容器的隔离原则。PID 命名空间引入了嵌套的 PID 空间，每个 PID 命名空间维护自己的 PID 分配器，命名空间内的进程只能看到本命名空间和子命名空间中的进程，看不到父命名空间和兄弟命名空间中的进程。

PID 命名空间的嵌套结构形成了一棵树。根命名空间是初始命名空间，它包含整个系统的所有进程。子命名空间通过 `clone(CLONE_NEWPID)` 创建,新创建的任务成为子命名空间的 PID 1（init）。子命名空间中的任务在父命名空间中也有 PID,但在子命名空间中看到的 PID 与父命名空间中的 PID 是不同的。这种"一个任务多个 PID"的模型正是 PID 命名空间设计的核心。

#pseudo-sample("5-7", [PID 命名空间与多重 PID 映射], kind: "代码")[
  ```c
  struct PidNamespace {
      Arc<PidNamespace> parent;     // 父命名空间（根为 NULL）
      u32 level;                    // 嵌套深度（根为 0）
      Spinlock<PidAllocator> alloc; // 本命名空间的 PID 分配器
      Vec<Weak<Task>> registry;     // PID → Task 弱引用表
      Arc<Task> init_task;          // 本命名空间的 init（PID 1）
  };

  struct TaskPidInfo {
      // 任务在每一层命名空间中的 PID
      // 索引 0 = 根命名空间，索引 N = 任务所在的最深命名空间
      Vec<PidEntry> pid_in_ns;
  };

  struct PidEntry {
      Arc<PidNamespace> ns;
      u32 pid;
  };

  // 在指定命名空间中查询任务的 PID
  u32 task_pid_in_ns(Task* task, PidNamespace* ns) {
      for (PidEntry e : task->pid_info.pid_in_ns) {
          if (e.ns == ns) return e.pid;
      }
      return 0;  // 任务在该命名空间中不可见
  }

  // 创建新 PID 命名空间
  PidNamespace* pid_ns_create(PidNamespace* parent) {
      PidNamespace* ns = alloc_pid_ns();
      ns->parent = arc_clone(parent);
      ns->level = parent->level + 1;
      pid_alloc_init(&ns->alloc);
      return ns;
  }
  ```
]

任务在每一层 PID 命名空间中都有一个独立的 PID,这些 PID 存储在任务的 `pid_in_ns` 向量中。系统调用如 `getpid` 根据调用者所在的命名空间返回对应的 PID,而不是返回某个全局唯一的 ID。这种设计使得容器内的进程看到的 PID 序列是连续的从 1 开始的,与运行在裸金属上的进程没有区别。

PID 命名空间的销毁有特殊的语义。当一个 PID 命名空间的 init 任务（PID 1）退出时,该命名空间中的所有其他任务都会被强制终止。这是因为命名空间中的孤儿任务原本应该被 init 收养,而 init 已经不存在了,这些孤儿任务无法被回收。强制终止机制保证了 PID 命名空间的清理是确定的，因为一旦 init 退出,整个命名空间中的所有资源都会被释放。这种"init 死则全员死"的语义使得容器的销毁过程是原子的,不会留下游离的任务。

PID 命名空间的可见性规则是单向的，子命名空间中的任务可见于父命名空间,但父命名空间中的任务不可见于子命名空间。这种单向可见性使得宿主机可以监控容器内的所有进程,但容器内的进程无法看到宿主机的进程。容器逃逸攻击的一种常见手段就是绕过 PID 命名空间的隔离访问宿主机的进程,因此 PID 命名空间的隔离是容器安全的基础之一。

== 凭据与能力

每个任务都关联一个凭据(Credentials)对象,凭据描述了任务的身份和权限。凭据包括用户 ID(UID)、组 ID(GID)、有效用户 ID(EUID)、有效组 ID(EGID)、保存的用户 ID(SUID)、保存的组 ID(SGID)和能力集(CapSet)。UID 和 GID 用于文件系统的访问控制，VFS 在打开文件时检查文件的所有者和权限位,根据任务的 EUID 和 EGID 决定是否允许访问。能力集是 POSIX.1e 定义的细粒度权限模型，其引入的动机在于传统的 root/non-root 二元权限模型过于粗糙,能力集将 root 的特权拆分为若干独立的能力,使得程序可以只获取它需要的能力,降低了被攻破后的影响范围。

#pseudo-sample("5-8", [凭据与能力集结构], kind: "代码")[
  ```c
  // 能力集：单一 64 位位集合,每位对应一项 Linux capability
  struct CapSet {
      u64 bits;         // CAP_KILL=bit5, CAP_SETUID=bit7, CAP_SYS_NICE=bit23 ...
  };

  struct Credentials {
      u32 uid, gid;       // 真实 UID/GID
      u32 euid, egid;     // 有效 UID/GID（用于权限检查）
      u32 suid, sgid;     // 保存的 UID/GID（setuid 程序使用）
      Vec<u32> groups;    // 附加组列表
      CapSet caps;        // 能力集（单一位集合）
  };

  struct Task {
      // ...其他字段...
      Arc<Credentials> creds;  // 不可变快照，整体替换
  };

  // setuid 系统调用：构造新凭据后整体替换
  int sys_setuid(u32 new_uid) {
      Task* self = current_task();
      Credentials* old = arc_clone(self->creds);

      // 权限检查
      if (old->euid != 0 && new_uid != old->uid
                         && new_uid != old->suid)
          return -EPERM;

      // 构造新凭据
      Credentials* new = creds_clone(old);
      new->euid = new_uid;
      if (old->euid == 0) {
          new->uid = new_uid;
          new->suid = new_uid;
      }

      // 原子替换
      arc_swap(&self->creds, new);
      return 0;
  }
  ```
]

凭据采用不可变快照加整体替换的策略。每个 Credentials 对象一旦创建就不可修改,任何对凭据的修改都通过构造新的 Credentials 对象并原子地替换任务的凭据指针来实现。这种策略消除了读取凭据时的锁竞争，任务的其他部分(包括 VFS 的权限检查)只需要原子地读取凭据指针然后在快照上进行检查,不需要担心检查过程中凭据被修改。整体替换的策略也保证了凭据的多个字段在更新时是原子可见的，以 `setuid` 为例，它同时修改 EUID 和 SUID,如果这两个字段分别更新,中间状态可能违反 POSIX 的语义。

能力集(`CapSet`)是一个 64 位的位集合,每一位对应一项 Linux capability（如 `CAP_KILL`、`CAP_SETUID`、`CAP_SYS_NICE` 等）。凭据中只维护一个统一的能力集,内核在权限检查时直接测试对应位是否置位。这种简化设计省去了 Linux 中 effective/permitted/inheritable/bounding 四子集之间的复杂提升和继承规则,在当前的使用场景中已经足够：root 凭据携带全满的能力集,非特权凭据携带空集,权限检查只需一次位测试即可完成。

`exec` 系统调用对凭据的处理是凭据机制中最微妙的部分。当任务执行一个新程序时,如果程序文件设置了 setuid 位,新程序的 EUID 会被设置为程序文件的所有者 UID。setuid 程序的典型例子是 `passwd`，普通用户没有修改 `/etc/shadow` 的权限,但 `passwd` 是 setuid root 程序,它在执行时获得 root 权限,可以修改密码文件。setuid 机制虽然提供了权限提升的能力,但也是历史上大量安全漏洞的根源，因为 setuid 程序的任何漏洞都可能被利用提升到 root 权限。能力集机制的引入正是为了减少 setuid 的使用，使程序只获取它需要的特定能力,而不是获取完整的 root 权限。

== 工程设计总结

进程与线程管理子系统的设计围绕一个核心抽象展开，即 Task。Task 既可以代表进程也可以代表线程,具体语义由创建时的共享标志决定。这种统一的设计相较于传统的"进程结构体加线程结构体"的二元设计,最大的优势在于消除了进程和线程之间的重复代码，调度器、信号处理、等待和退出机制都不需要为进程和线程维护两套实现。Linux 在历史上选择了同样的统一设计,而 Windows 和某些 Unix 变种则保留了进程和线程的分离结构,代码的复杂度差异是显著的。

Task 身份基于引用计数指针而非整数 PID,这种设计消除了 PID 分配的全局竞争。在大量并发 `fork` 的场景下,传统实现的 PID 分配锁会成为瓶颈,而本系统的 Task 创建只需要分配内存,不需要获取任何全局锁。PID 命名层在 ABI 边界上才登场,它将引用计数指针映射为整数 PID,使得 POSIX 系统调用可以正常工作。这种"内核内部用引用、ABI 层用整数"的分层使得调度核心完全不依赖 PID,PID 只是 ABI 的一个翻译层。

亲缘关系的集中式锁设计牺牲了一定的并发度,但消除了多锁反序死锁的风险。在内核开发中死锁是最难调试的问题之一，因为它不会产生崩溃日志,只会导致系统挂起。集中式锁的代价是亲缘关系操作不能并行,但收益是任何涉及亲缘关系的代码路径都不需要担心锁的获取顺序。这种"用一定的性能换取可调试性"的权衡在内核开发中是常见的，可调试性的价值在出现问题时才会显现,而问题在内核中往往是灾难性的。

子系统侧表的设计将调度器与 VFS、内存管理等子系统解耦。传统实现中任务结构体直接包含各子系统的字段,这种紧耦合使得任何子系统的修改都可能影响调度器的编译。侧表设计将依赖关系反转，让子系统依赖调度器提供的侧表接口,而调度器不依赖任何子系统。这种反转使得调度器可以独立编译和测试,新子系统的接入只需要注册侧表项和钩子函数,不需要修改调度器的代码。这种可扩展性在内核开发的中后期尤为重要，因为随着内核功能的增加,如果调度器与每个子系统都紧耦合,代码的维护成本将快速增长。

`clone` 的三阶段实现和钩子机制是侧表设计的自然延伸。调度器不知道侧表项的内部结构,它把拷贝策略的决策权委托给子系统注册的钩子。这种委托使得每个子系统可以自主决定 `CLONE_*` 标志对自己的语义，例如 VFS 决定 `CLONE_FS` 是共享还是深拷贝,内存管理决定 `CLONE_VM` 是共享还是写时复制,文件描述符表决定 `CLONE_FILES` 是共享还是深拷贝。这种委托不仅减少了调度器的复杂度,也使得新的克隆语义可以在不修改调度器的情况下引入。

两阶段退出的设计保证了 `wait` 语义的正确性。子任务在 Zombie 状态下保留了退出信息,父任务可以在任意时刻通过 `wait` 获取这些信息。如果在 `exit` 时立即释放所有资源,父任务就无法获取退出码;如果父任务始终持有子任务的引用,内核就无法在子任务退出时回收资源。两阶段退出在这两种极端之间取得了平衡，具体做法是退出时释放大部分资源(地址空间、文件描述符),只保留 Task 结构体本身，而 `wait` 时释放最后的引用,Task 结构体被回收。孤儿收养机制保证了所有僵尸任务最终都会被 init 回收,避免了僵尸任务永远占用内核资源的情况。

等待队列的弱引用设计避免了任务自我保活的问题。如果等待队列使用强引用存储等待者,任务在等待自己退出的场景下将永远无法释放，因为任务持有等待队列,等待队列持有任务,形成循环引用。弱引用打破了这种循环,任务的生命周期不受等待队列的影响,任务退出时弱引用自动失效。唤醒路径在升级失败时跳过等待者继续尝试下一个,这种"尽力而为"的唤醒策略在大多数场景下是足够的,而代价是消除了循环引用的风险。

信号机制的两阶段投递将信号的发送和接收解耦。发送方不需要等待接收方处理信号,目标任务在自己的执行节奏中处理信号。这种异步性使得信号的发送是 O(1) 的，发送方只需要将信号加入待处理队列,不需要等待目标任务响应。线程组共享信号的设计保证了多线程程序中信号处理的一致性，即任何一个线程修改信号处理动作都会影响整个线程组,信号发送给线程组时由任何一个未屏蔽该信号的线程处理。

PID 命名空间的嵌套设计支持容器隔离。每个命名空间维护自己的 PID 分配器和注册表,任务在每一层命名空间中都有独立的 PID,系统调用根据调用者所在的命名空间返回对应的 PID。这种"一个任务多个 PID"的模型使得容器内的进程看到的 PID 序列与裸金属上没有区别,容器的隔离是透明的。命名空间的销毁语义保证了清理的确定性，一旦 init 退出，整个命名空间被强制终止,不会留下游离的任务。

凭据的不可变快照加整体替换策略消除了凭据读取的锁竞争。VFS 的权限检查、网络协议栈的访问控制和系统调用的权限验证都需要读取任务的凭据,如果凭据是可变的,这些读取路径都需要获取锁,而凭据的读取是高频操作,锁竞争将成为严重的性能瓶颈。整体替换策略使得凭据的读取完全无锁，读取者只需要原子地获取凭据指针,然后在不可变快照上进行检查。能力集的细粒度权限模型相较于传统的 root/non-root 二元模型,显著减少了 setuid 程序的特权范围,降低了被攻破后的影响。

回顾整个进程与线程管理子系统的设计,可以发现一个贯穿始终的主题，即"统一抽象、分层解耦"。Task 统一了进程和线程的抽象;`clone` 统一了创建进程和创建线程的接口;PID 命名层将整数 PID 与引用计数指针解耦;子系统侧表将调度器与各子系统解耦;两阶段退出将资源释放与状态保留解耦;凭据的不可变快照将权限检查与权限修改解耦。每一次统一都减少了代码的重复和复杂度,每一次解耦都使得被解耦的两个部分可以独立演化。这种设计哲学与第二章内存管理和第三章设备模型中的"分层递进"和"分离关注点"一脉相承，它们共同构成了本系统应对复杂性的基本策略。

