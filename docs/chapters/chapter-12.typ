#import "../config.typ": project-name
#import "../styles/diagram.typ": flow-arrow, layer-card
#import "../styles/figure.typ": continued-table, figure-caption, pseudo-sample
#import "../styles/tokens.typ": handoff-fill, soft-fill, stable-fill, warm-fill

= 第十二章 网络子系统

在第十一章中，终端把字符设备、VFS 和信号机制连接在一起。本章讨论网络子系统。网络的复杂性来自两个方向。向下，它依赖具体网卡驱动、DMA 或 virtqueue 队列、链路状态和 MTU。向上，它需要为套接字系统调用提供 TCP、UDP、原始套接字、地址绑定、监听、连接、收发和轮询语义。中间还需要协议栈状态机、路由表、接口配置和等待队列。

我们把网络子系统拆成两层。一层是 `libs/net`，负责网络设备抽象、接口管理、协议栈适配、路由和网络套接字句柄。另一层是 VFS 与套接字兼容层，负责把 POSIX 套接字文件描述符映射到网络栈或 Unix 套接字。第三章中的 `NetFunction` 把网络设备作为设备能力发布，协议栈再挂接对应 `NetDevice`。这样，驱动、协议栈和用户态套接字三者之间没有循环依赖。

== 12.1 分层结构

网络分层的底部是网络驱动接口（`NetDriver`）。具体驱动实现收包、发包、链路状态、MAC 地址和统计信息。`NetDevice` 保存接口身份、名称、MTU 和活跃或移除状态。`NetFunction` 把 `NetDevice` 适配为设备模型中的设备能力，并提供类型化控制请求。`NetStack` 管理所有已挂接的接口、路由表和套接字等待者。VFS 套接字层把系统调用转换为 `NetStack` 操作。

#figure(caption: figure-caption("图", "12-1", [网络子系统分层]))[
  #layer-card("POSIX 套接字系统调用", [创建、绑定、监听、接受连接、连接、发送、接收和轮询], fill: soft-fill)
  #flow-arrow(label: "文件描述符与套接字操作")
  #layer-card("VFS 套接字层", [NetSocketFileOps、Unix 套接字、netlink 套接字、等待队列], fill: soft-fill)
  #flow-arrow(label: "协议栈句柄")
  #layer-card("NetStack", [接口注册表、路由表、TCP/UDP/原始套接字、轮询调度], fill: handoff-fill)
  #flow-arrow(label: "协议引擎适配")
  #layer-card("ManagedInterface", [smoltcp 接口、套接字集合、设备适配器和时间转换], fill: warm-fill)
  #flow-arrow(label: "驱动抽象")
  #layer-card("NetDevice 与 NetDriver", [VirtIO-net 等驱动，链路状态、MTU、收发缓冲区], fill: stable-fill)
]

这个结构保留了替换协议栈的可能性。`NetDriver` 和公共配置类型不依赖 smoltcp。协议引擎适配集中在适配器、引擎和接口层。若未来替换协议引擎，驱动和 VFS 系统调用层不需要整体重写。当前公共 API 暴露的是 `Endpoint`、`IfConfig`、`NetSocketHandle`、`SocketState` 和错误类型，而不是 smoltcp 内部对象。

== 12.2 接口管理与路由

`NetStack` 使用读写锁保护接口注册表。挂接和分离需要写锁，轮询和套接字操作使用读锁。每个接口内部又有独立互斥锁，因为协议引擎的单个接口不可并发访问。这样不同接口可以并行，单个接口内部保持串行。接口挂接成功后，路由表会根据接口配置更新直连路由和网关路由。分离时移除接口，并清理对应路由。

#pseudo-sample("12-1", [NetStack 的核心结构], kind: "代码")[
  ```rust
  struct NetStack {
      tuning: NetTuning,
      interfaces: RwLock<BTreeMap<InterfaceId, Arc<Mutex<ManagedInterface>>>>,
      routes: RwLock<RouteTable>,
      notify_waiters: WaitQueue,
  }

  fn attach(&self, dev: Arc<NetDevice>, config: IfConfig) -> Result<(), NetError> {
      let id = dev.id();
      let managed = ManagedInterface::new(dev, config.clone(), self.tuning.tcp, self.tuning.tcp_listen)?;

      self.interfaces.write().insert(id, Arc::new(Mutex::new(managed)));
      self.routes.write().replace_connected(id, &config.addresses);
      self.routes.write().replace_gateway(id, config.gateway);
      Ok(())
  }
  ```
]

读写锁的选择来自网络输入输出的访问模式。轮询、发送、接收和状态查询远比挂接和分离高频。读锁允许多个读路径同时进入接口表，再在具体接口锁上串行化。若使用单一全局互斥锁，所有接口和套接字操作都会互相阻塞。若完全无锁，接口热移除时的一致性又难以保证。当前设计把注册表变化和单接口状态变化分开处理。

== 12.3 协议栈轮询与主动推进

协议栈需要周期性轮询。定时器或网络线程调用 `poll_ns`，它将调度器纳秒时间转换为网络层时间，然后驱动所有接口进行一轮收发。每个接口轮询后会产生结果，网络栈根据结果更新路由、状态或套接字可用性。轮询结束后，当前实现会唤醒全局套接字等待队列中的任务，让它们重新检查自身套接字状态。

#pseudo-sample("12-2", [协议栈轮询], kind: "代码")[
  ```rust
  fn poll(&self, timestamp: NetInstant) {
      let table = self.interfaces.read();
      for (id, iface_lock) in table.iter() {
          if let Some(mut iface) = iface_lock.try_lock() {
              let result = iface.poll(timestamp);
              drop(iface);
              self.apply_poll_result(*id, result);
          }
      }
      drop(table);
      self.wake_socket_waiters();
  }

  fn poll_now(&self) {
      let timestamp = NetInstant::from_nanos(now_ns_public());
      for _ in 0..self.tuning.active_poll.max_rounds {
          if !self.poll_one_active_round(timestamp) {
              break;
          }
      }
  }
  ```
]

`poll_now` 用于套接字操作刚刚产生出站数据之后主动推进协议栈。回环场景尤其需要它。一次发送可能立即回灌为接收包，若只等待定时器节拍，`connect`、`accept` 或 `read` 会出现不必要延迟。主动轮询至少执行两轮，第一轮负责发送，第二轮处理回灌或同接口接收。之后若没有套接字状态变化，就停止空转。

当前全局唤醒策略有意保持简单。所有阻塞在网络套接字上的任务会被唤醒，然后各自重新检查就绪状态。这个策略会有惊群，但实现可靠。文档必须明确它的边界。高并发网络负载下，应当进一步按套接字句柄和事件类型精确唤醒。当前实现先换取兼容性和可调试性。

== 12.4 套接字与 VFS 边界

POSIX 套接字在用户态表现为文件描述符。VFS 套接字层负责创建 `FileOps`，把 `read`、`write`、`sendmsg`、`recvmsg`、`ioctl` 和 `poll` 转换为套接字操作。网络套接字使用 `NetSocketHandle` 指向协议栈中的套接字。Unix 套接字则由 `libs/socket` 在内核内存中实现，不经过网络协议栈。二者共享文件描述符模型，但底层数据路径不同。

#continued-table(
  "12-1",
  [Socket 类型与实现边界],
  (1.1fr, 2fr, 2.2fr),
  (
    table.cell(fill: soft-fill)[#text(weight: "bold")[类型]],
    table.cell(fill: soft-fill)[#text(weight: "bold")[实现对象]],
    table.cell(fill: soft-fill)[#text(weight: "bold")[语义]],
  ),
  (
    table.cell(fill: warm-fill)[TCP 套接字],
    table.cell(fill: warm-fill)[NetStack 中的协议套接字句柄。],
    table.cell(fill: warm-fill)[连接、监听、accept、字节流收发和状态查询。],
    table.cell(fill: soft-fill)[UDP 套接字],
    table.cell(fill: soft-fill)[NetStack 中的数据报套接字。],
    table.cell(fill: soft-fill)[保留消息边界，sendto/recvfrom 处理远端地址。],
    table.cell(fill: handoff-fill)[原始套接字],
    table.cell(fill: handoff-fill)[协议栈原始套接字或 ICMP 句柄。],
    table.cell(fill: handoff-fill)[调用方提供完整 IP 包或协议相关负载。],
    table.cell(fill: stable-fill)[Unix 套接字],
    table.cell(fill: stable-fill)[`libs/socket` 内存对象。],
    table.cell(fill: stable-fill)[本机 IPC，不经过网络设备和 IP 路由。],
  ),
  kind: "表",
  continuation-kind: "续表",
  align: (left, left, left),
)

这个边界让套接字系统调用层保持统一。`socket` 返回文件描述符。`poll` 和 `epoll` 观察就绪状态。`close` 释放文件对象。具体数据路径由文件操作内部选择。网络套接字可能触发协议栈轮询和等待队列。Unix 套接字则在内核内存队列中完成连接和收发。用户态不需要知道二者差异。

== 12.5 网络设备能力与控制路径

网络设备通过 `NetFunction` 进入设备模型。它暴露接口 ID、名称、链路介质、链路状态、MAC 地址、MTU 和统计信息。控制请求如设置 MTU 或管理启用状态会进入类型化控制，而不是通过无结构的全局设备表查找。设备移除时，`NetFunction::mark_gone` 会标记底层 `NetDevice`，正在进行的收发可以看到设备不可用。

#pseudo-sample("12-3", [网络设备类型化控制], kind: "代码")[
  ```rust
  enum NetControlRequest {
      GetInterfaceId,
      GetName,
      GetLinkState,
      GetMacAddress,
      GetMtu,
      SetMtu { mtu: usize },
      SetAdminUp { up: bool },
  }

  fn control_net_device(dev: &Arc<NetDevice>, req: NetControlRequest)
      -> Result<NetControlResponse, ControlError>
  {
      if !dev.is_active() {
          return Err(ControlError::NoDevice);
      }

      match req {
          NetControlRequest::SetAdminUp { up } => {
              stack().set_iface_admin_up(dev.id(), up)?;
              Ok(NetControlResponse::Done)
          }
          NetControlRequest::SetMtu { mtu } => {
              dev.set_mtu(mtu)?;
              Ok(NetControlResponse::Done)
          }
          _ => query_device_property(dev, req),
      }
  }
  ```
]

控制路径的设计延续了第三章的类型化设备能力思路。网络设备的自然消费者是协议栈，而不是字符设备读写。用户态控制接口兼容层可以把传统命令翻译为类型化请求。设备核心则只处理结构化请求。这样可以减少命令码与参数结构不匹配的风险。

== 12.6 套接字生命周期

套接字的生命周期从 `socket()` 系统调用开始。用户态指定地址族、类型和协议。内核创建对应套接字对象，并把它包装成 VFS 文件对象。之后用户态通过文件描述符执行 `bind`、`listen`、`connect`、`accept`、`sendmsg`、`recvmsg`、`shutdown`、`poll` 和 `close`。每个操作都要维护套接字状态机。TCP、UDP、原始套接字和 Unix 套接字的状态机不同，但它们都要进入统一文件描述符生命周期。

TCP 套接字的状态最复杂。主动连接从已创建或已绑定进入连接中，握手完成后进入已建立。被动监听套接字进入监听状态，收到连接后产生待接受子连接，`accept` 再把子连接交给用户态。关闭时需要处理半关闭、FIN、RST 和本地关闭。当前实现可以先覆盖核心状态，但状态机边界必须清楚。若 `connect` 尚未完成，非阻塞 `connect` 应返回 `EINPROGRESS`，后续 `poll` 写事件表示连接完成或失败。若 `accept` 队列为空，阻塞 `accept` 睡眠，非阻塞 `accept` 返回 `EAGAIN`。

UDP 套接字保留消息边界。`bind` 决定本地地址和端口。`sendto` 可以指定远端地址。`connect` 对 UDP 只是设置默认远端并过滤接收来源，不建立握手。`recvfrom` 每次返回一个数据报及其来源地址。若把 UDP 当作字节流处理，用户态会看到错误的边界语义。套接字层因此要在文件操作统一入口下保留类型差异。

#continued-table(
  "12-2",
  [套接字生命周期中的关键状态],
  (1.1fr, 2.1fr, 2.3fr),
  (
    table.cell(fill: soft-fill)[#text(weight: "bold")[阶段]],
    table.cell(fill: soft-fill)[#text(weight: "bold")[主要操作]],
    table.cell(fill: soft-fill)[#text(weight: "bold")[状态约束]],
  ),
  (
    table.cell(fill: warm-fill)[创建],
    table.cell(fill: warm-fill)[`socket`，分配协议套接字和文件对象。],
    table.cell(fill: warm-fill)[检查地址族、类型和协议，失败返回明确错误码。],
    table.cell(fill: soft-fill)[绑定],
    table.cell(fill: soft-fill)[`bind`，设置本地地址和端口。],
    table.cell(fill: soft-fill)[端口冲突返回 `EADDRINUSE`，地址非法返回 `EADDRNOTAVAIL`。],
    table.cell(fill: handoff-fill)[连接或监听],
    table.cell(fill: handoff-fill)[`connect`、`listen`、`accept`。],
    table.cell(fill: handoff-fill)[TCP 进入握手或监听状态，UDP 记录默认远端。],
    table.cell(fill: stable-fill)[收发],
    table.cell(fill: stable-fill)[`sendmsg`、`recvmsg`、`read`、`write`。],
    table.cell(fill: stable-fill)[遵守阻塞、非阻塞、消息边界和错误状态。],
    table.cell(fill: warm-fill)[关闭],
    table.cell(fill: warm-fill)[`shutdown`、`close`。],
    table.cell(fill: warm-fill)[唤醒等待者，释放句柄，通知协议栈状态变化。],
  ),
  kind: "表",
  continuation-kind: "续表",
  align: (left, left, left),
)

关闭需要和 VFS 文件生命周期协调。用户关闭文件描述符时，文件对象引用可能仍被其它线程或轮询结构持有。套接字对象要先阻止新的输入输出，再唤醒等待者，最后在引用释放时让协议栈关闭句柄。若直接释放协议套接字，仍在等待的任务可能访问悬空对象。我们沿用第七章的生命周期收束原则，用状态标记和引用计数分离用户不可再发现和对象内存可释放。

== 12.7 阻塞、非阻塞与就绪状态

套接字系统调用必须处理阻塞和非阻塞模式。阻塞 `read` 在无数据时睡眠。非阻塞 `read` 返回 `EAGAIN`。阻塞 `connect` 可以等待握手完成。非阻塞 `connect` 返回 `EINPROGRESS`。`poll` 和 `epoll` 不直接执行输入输出，只报告就绪状态。就绪状态表达协议套接字的用户可见状态，不能简单等同于底层设备可读写。例如 TCP 监听套接字的可读表示 `accept` 队列非空，TCP 连接套接字的可读表示接收缓冲有数据或对端关闭。

我们把等待语义放在套接字文件层和 `NetStack` 的等待队列之间。每个套接字操作先检查协议状态。若可立即完成，直接返回。若不可完成且文件描述符非阻塞，返回 `EAGAIN` 或对应错误。若可阻塞，任务登记到网络等待队列，触发轮询或等待协议栈推进，再睡眠。醒来后重新检查套接字状态。全局唤醒会带来惊群，但每个等待者都重新检查自己的句柄，因此不会破坏正确性。

#pseudo-sample("12-4", [套接字阻塞读取模式], kind: "代码")[
  ```rust
  fn recv_blocking(sock: &NetSocketFile, buf: UserSliceMut<u8>) -> Result<usize, Errno> {
      loop {
          match sock.try_recv(buf) {
              Ok(n) => return Ok(n),
              Err(Errno::EAGAIN) if sock.nonblock() => return Err(Errno::EAGAIN),
              Err(Errno::EAGAIN) => {
                  stack().register_waiter(current_task());
                  stack().poll_now();
                  if sock.is_ready_for_read() {
                      stack().unregister_waiter(current_task());
                      continue;
                  }
                  schedule_interruptible()?;
                  stack().unregister_waiter(current_task());
              }
              Err(e) => return Err(e),
          }
      }
  }
  ```
]

就绪状态还要处理错误。连接失败、对端关闭、套接字被 `shutdown`，都应使 `poll` 返回相应事件，使等待者醒来并读取错误或文件结束。若只在数据到来时唤醒，关闭事件可能被忽略，用户态会永久阻塞。我们在轮询结果应用阶段统一唤醒套接字等待者，保证状态变化能被观察。后续优化可以按句柄精确唤醒，但必须保留错误状态唤醒。

信号打断同样适用。阻塞套接字操作进入可打断睡眠。若收到待处理信号，睡眠返回 `EINTR` 或参与系统调用重启。套接字层不直接构造信号帧，只把等待结果上交给系统调用分发器。这个边界与第十章一致。

== 12.8 路由、地址与接口状态

网络接口挂接后，`NetStack` 根据配置生成直连路由。配置网关后，路由表生成默认或指定网关路由。发送数据包时，协议栈根据目标地址查询路由，选择接口和下一跳。若没有路由，`connect` 或 `sendto` 应返回 `ENETUNREACH` 或相关错误。若接口关闭，返回 `ENETDOWN` 或让套接字进入错误状态。路由错误不能表现为静默丢包，否则用户态很难诊断。

接口状态分为设备活跃、链路可用和管理启用。设备活跃表示驱动对象仍然有效。链路可用表示物理或虚拟链路可用。管理启用表示内核配置允许该接口参与收发。三者不应混为一谈。设备移除时，所有操作都应失败。链路断开时，设备仍然存在，但收发可能不可用。管理关闭是用户或配置主动关闭接口。类型化控制可以分别查询和设置这些状态。

#continued-table(
  "12-3",
  [接口状态层次],
  (1.1fr, 2fr, 2.2fr),
  (
    table.cell(fill: soft-fill)[#text(weight: "bold")[状态]],
    table.cell(fill: soft-fill)[#text(weight: "bold")[来源]],
    table.cell(fill: soft-fill)[#text(weight: "bold")[影响]],
  ),
  (
    table.cell(fill: warm-fill)[设备活跃],
    table.cell(fill: warm-fill)[设备生命周期。],
    table.cell(fill: warm-fill)[移除后阻止新输入输出，分离后清理路由和等待者。],
    table.cell(fill: soft-fill)[链路可用],
    table.cell(fill: soft-fill)[驱动或虚拟后端报告。],
    table.cell(fill: soft-fill)[影响实际收发和用户态链路状态查询。],
    table.cell(fill: handoff-fill)[管理启用],
    table.cell(fill: handoff-fill)[内核配置或用户控制。],
    table.cell(fill: handoff-fill)[决定协议栈是否把接口作为可用出口。],
    table.cell(fill: stable-fill)[路由就绪],
    table.cell(fill: stable-fill)[地址和网关配置。],
    table.cell(fill: stable-fill)[决定 send/connect 是否能找到路径。],
  ),
  kind: "表",
  continuation-kind: "续表",
  align: (left, left, left),
)

地址配置也要分层。接口可以有 IPv4 地址、前缀长度、广播地址和网关。IPv6 支持加入后，还会有链路本地地址、全局地址和邻居发现。当前结构用 `IfConfig` 统一描述配置，使协议栈挂接时一次性获得地址信息。后续若支持运行时 `ifconfig` 或 netlink 配置，可以通过类型化控制或专门网络配置接口更新 `IfConfig`，再驱动路由表变化。

路由表更新需要和接口注册表保持一致。挂接先创建接口，再更新路由。分离先阻止接口继续参与路由，再移除接口。若路由指向已移除接口，发送路径可能访问无效句柄。我们用接口 ID 作为关联键，分离时清理对应路由。读路径通过锁保护看到一致快照。

== 12.9 回环与主动推进

本机通信大量依赖回环接口。发送和接收都发生在同一内核内。若协议栈只靠周期定时器轮询，发送方写入数据后要等下一个时钟节拍才能被接收方看到，吞吐和延迟都会变差。`poll_now` 的作用就是在套接字操作后主动推进协议栈，使本机数据尽快回灌。

主动推进需要限制轮数。若每次发送都无限轮询，用户态写入会被协议栈处理拖住，甚至在大量连接时长时间占用 CPU。当前调优参数中保留主动轮询参数，设置最大轮数。第一轮通常把出站数据交给设备或回环接口，第二轮处理回灌和状态变化。若没有更多变化，就停止。这个策略兼顾短连接延迟和 CPU 使用。

回环接口还暴露唤醒策略问题。发送方写入后，接收方可能正在 `recv` 阻塞。协议栈轮询处理到接收数据后，应唤醒等待者。全局唤醒会让所有网络等待者重新检查，简单可靠但可能惊群。高并发 TCP 服务器中，精确唤醒更合适。我们先使用全局唤醒保证正确性，再把精确唤醒作为后续优化方向。这个取舍符合当前工程阶段。

主动轮询也不能在持有过多锁时执行。套接字操作可能持有套接字文件锁，`NetStack` 轮询需要接口锁。若锁序设计不清，会出现套接字锁和接口锁互相等待。我们尽量在短临界区内更新套接字状态，调用协议栈推进时避免持有不必要的 VFS 锁。这个边界对网络性能和死锁预防都很重要。

== 12.10 并发、锁序与资源回收

网络对象生命周期复杂。套接字文件对象被文件描述符表持有，协议栈套接字集合持有句柄，等待队列持有等待者弱引用，接口表持有 `ManagedInterface`，设备模型持有 `NetFunction`。关闭、分离、进程退出和设备移除都可能同时发生。设计目标是让每条路径都能收束。关闭阻止新的套接字操作并释放句柄。分离标记接口不可用并唤醒等待者。进程退出通过文件描述符关闭触发套接字释放。设备移除通过 `NetDevice` 活跃状态让协议栈停止使用驱动。

锁序需要特别控制。接口注册表锁保护接口 ID 到接口的映射。接口锁保护协议引擎对象。套接字文件锁保护文件级标志和局部状态。等待队列锁只保护等待者列表。调用顺序应避免在接口锁内执行用户复制，也避免在等待队列锁内进入协议栈轮询。唤醒操作尽量锁外进行。这个模型与第七章同步规则一致。

资源回收时还要唤醒阻塞任务。一个线程阻塞在 `recv`，另一个线程关闭同一个套接字。关闭路径应让 `recv` 返回错误或文件结束，而不是永远睡眠。接口分离时，所有依赖该接口的套接字也应观察到错误。当前全局唤醒策略虽然粗，但能保证状态变化被等待者重新检查。后续精确唤醒不能牺牲这一点。

网络缓冲区分配失败应返回错误或触发背压，不应导致内核崩溃。用户态可以通过高频发送造成内存压力。协议栈和套接字层应返回 `ENOBUFS`、`ENOMEM` 或短写。只有内核基础结构损坏时才进入不可恢复路径。这个原则和前面章节对分配失败的处理一致。

== 12.11 性能与演进视角

网络性能主要受数据复制、协议栈推进、锁竞争、设备队列和唤醒延迟影响。连续 TCP/UDP 吞吐更关注批量收发能力，请求响应型负载更关注系统调用成本、主动轮询和等待者唤醒。边界参数和阻塞语义则更多依赖套接字系统调用兼容层。不同负载暴露的问题不同，因此分析网络性能时需要先判断瓶颈位于数据路径、控制路径还是 ABI 处理。

我们在网络设计中优先保证正确性和可收束。全局唤醒、主动轮询和清晰错误转换会带来一些额外成本，但能让状态变化可靠传递。性能优化可以沿三个方向推进。第一，精确套接字唤醒，减少惊群。第二，减少数据复制和锁持有时间。第三，根据网卡中断和队列能力改进轮询触发。每个方向都可以在现有分层中独立推进。

文件输入输出与网络看似无关，但它们共享用户复制和调度能力。若 `copy_from_user` 慢，网络 `sendmsg` 和 `recvmsg` 也会受影响。若调度唤醒慢，阻塞套接字的延迟也会增大。网络性能分析因此不能只看协议栈。它要结合第七章同步、第八章系统调用、第十二章轮询和驱动路径共同分析。

网络程序的诊断输出还依赖第十一章的 console。若终端输出慢，会影响日志观察和问题定位。文档把这些关系讲清楚，有助于后续解释网络路径和用户态可观察行为之间的差异。

== 12.12 工程设计总结

网络子系统连接设备驱动、协议栈、VFS 和用户态套接字。它的核心挑战是保持这些层的边界。驱动只提供链路与包收发。协议栈处理接口、路由和协议状态。VFS 负责文件描述符和 POSIX 套接字语义。设备模型负责生命周期和类型化控制。这样的设计使网络子系统可以在当前规模下保持简洁，同时为后续精确套接字唤醒、IPv6、更多驱动和更细粒度路由策略留下空间。网络不是单纯的设备驱动，也不是单纯的文件接口。它位于设备、协议和 POSIX ABI 的交界处，必须用清晰的分层来控制复杂度。
