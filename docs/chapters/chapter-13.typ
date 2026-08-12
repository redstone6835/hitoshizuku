#import "../config.typ": project-name
#import "../styles/diagram.typ": flow-arrow, layer-card
#import "../styles/figure.typ": continued-table, figure-caption, pseudo-sample
#import "../styles/tokens.typ": handoff-fill, soft-fill, stable-fill, warm-fill

= 第十三章 网络子系统

在第十一章中，终端把字符设备、VFS 和信号机制连接在一起。本章讨论网络子系统，但重点不再是重复列出 TCP/IP 功能，而是说明协议状态、用户可见 socket 和设备队列如何在当前工程中形成稳定边界。当前实现包含 IPv4/IPv6、TCP、UDP、ICMP、Raw、路由、邻居、PMTU、分片重组和 VirtIO-net；这些属于操作系统必须具备的常规能力，本章只作完整性证明。

当前网络结构由三类责任单元组成：`libs/net` 保存架构无关的地址、报文、流表、传输状态和设备契约；`net.stack` ELM 持有 FlowShard、路由与协议状态；常驻 host 和 VFS 保留 socket facade、文件描述符和等待队列。`net.virtio` 与 `net.loopback` 是独立设备 ELM。协议栈可以被替换，常驻用户接口和设备能力边界不随之变。

== 13.1 分层结构

网络分层的底部是 `NetQueuePair` 批量队列契约。驱动负责 virtqueue、DMA、IRQ 和 completion；`NetDeviceRegistration` 一次性交付设备身份、队列能力和 buffer pool。常驻 host 接管设备后，把收发批次交给协议 ELM。VFS 套接字层只接触稳定的 facade 和 `SocketRuntime`，不持有协议 ELM 内部对象。

#figure(caption: figure-caption("图", "13-1", [网络子系统分层]))[
  #layer-card("POSIX 套接字系统调用", [创建、绑定、监听、接受连接、连接、发送、接收和轮询], fill: soft-fill)
  #flow-arrow(label: "文件描述符与套接字操作")
  #layer-card("VFS 套接字层", [NetSocketFileOps、Unix 套接字、netlink 套接字、等待队列], fill: soft-fill)
  #flow-arrow(label: "协议栈句柄")
  #layer-card("net.stack ELM", [FlowShard、协议状态、路由、邻居、定时器和控制面], fill: handoff-fill)
  #flow-arrow(label: "代际绑定的 turn")
  #layer-card("常驻 Host / Socket facade", [文件描述符、readiness、VFS 生命周期和用户 ABI], fill: warm-fill)
  #flow-arrow(label: "批量 queue + buffer lease")
  #layer-card("net.virtio / net.loopback", [VirtIO 队列、DMA pool、回环队列和设备生命周期], fill: stable-fill)
]

这里的替换边界是实际运行边界，不是目录层面的抽象：协议状态属于 `net.stack` 代际，VFS facade 属于常驻 host，设备队列属于设备 ELM。调用帧会校验结构大小、拥有者、generation、提交位和宿主地址范围。当前网络 Nexus 的 `packet.rx/tx` provider 仍是 TODO，热路径使用私有 `direct-pinned` 导出，因此不能把它描述成已经完成的通用端口流水线。

== 13.2 接口管理与路由

当前配置面由不可变 `ConfigSnapshot` 和 `ConfigStore` 组成，路由表、地址、邻居和 PMTU 状态由 FlowShard 持有并在固定 turn 中修改。设备注册与移除由常驻 host 的 registrar 处理，协议 ELM 不直接操作 VFS 注册表。这样配置读取可以使用快照，状态写入仍然遵守 shard 单写者规则。

#pseudo-sample("13-1", [NetStack 的核心结构], kind: "代码")[
  ```rust
  struct NetStack {
      config: ConfigStore,
      flow_shards: Box<[FlowShard]>,
      generation: u64,
  }

  fn dispatch(turn: &mut NetStackShardTurn) -> Result<(), NetError> {
      verify_generation(turn)?;
      run_flow_commands(turn)
  }
  ```
]

读写锁的选择来自网络输入输出的访问模式。轮询、发送、接收和状态查询远比挂接和分离高频。读锁允许多个读路径同时进入接口表，再在具体接口锁上串行化。若使用单一全局互斥锁，所有接口和套接字操作都会互相阻塞。若完全无锁，接口热移除时的一致性又难以保证。当前设计把注册表变化和单接口状态变化分开处理。

== 13.3 协议栈轮询与主动推进

协议推进由每个 FlowShard 的网络 worker 负责。IRQ、队列提交、socket 发送和定时器只发布有界工作；worker 按 packet、byte 和时间预算排空 ingress、control、dirty flow 和 completion 队列。系统调用可以尝试走 local-turn，失败后发布 pending 并回退到 owner worker，不再把所有接口串在一个全局轮询循环中。

#pseudo-sample("13-2", [协议栈轮询], kind: "代码")[
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

本地 TCP/UDP 路径会在同一 shard 内尝试同步推进，避免短请求经过 worker 睡眠和重新唤醒；大流量、租约竞争、代际失效或 scratch 不足时回到异步 worker。这个快速路径与慢速路径共享同一个执行租约和 generation 检查，优化的是调度往返，不是放弃并发保护。

当前全局唤醒策略有意保持简单。所有阻塞在网络套接字上的任务会被唤醒，然后各自重新检查就绪状态。这个策略会有惊群，但实现可靠。文档必须明确它的边界。高并发网络负载下，应当进一步按套接字句柄和事件类型精确唤醒。当前实现先换取兼容性和可调试性。

== 13.4 套接字与 VFS 边界

POSIX 套接字在用户态表现为文件描述符。VFS 套接字层负责创建 `FileOps`，把 `read`、`write`、`sendmsg`、`recvmsg`、`ioctl` 和 `poll` 转换为套接字操作。网络套接字使用 `NetSocketHandle` 指向协议栈中的套接字。Unix 套接字则由 `libs/socket` 在内核内存中实现，不经过网络协议栈。二者共享文件描述符模型，但底层数据路径不同。

#continued-table(
  "13-1",
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

== 13.5 网络设备能力与控制路径

网络设备通过 `NetDeviceRegistration` 交给常驻 registrar，再由设备 ELM 的 queue endpoint 接入网络运行时。注册对象携带接口身份、MAC、MTU、队列能力、DMA pool 和 IRQ 控制；sysfs、procfs 和 ioctl 只读取其快照。设备移除时先停止队列调用并排空 buffer lease，随后让相关 socket 看到稳定的设备失效状态。

#pseudo-sample("13-3", [网络设备类型化控制], kind: "代码")[
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

== 13.6 套接字生命周期

套接字的生命周期从 `socket()` 系统调用开始。用户态指定地址族、类型和协议。内核创建对应套接字对象，并把它包装成 VFS 文件对象。之后用户态通过文件描述符执行 `bind`、`listen`、`connect`、`accept`、`sendmsg`、`recvmsg`、`shutdown`、`poll` 和 `close`。每个操作都要维护套接字状态机。TCP、UDP、原始套接字和 Unix 套接字的状态机不同，但它们都要进入统一文件描述符生命周期。

TCP 套接字的状态最复杂。主动连接从已创建或已绑定进入连接中，握手完成后进入已建立。被动监听套接字进入监听状态，收到连接后产生待接受子连接，`accept` 再把子连接交给用户态。关闭时需要处理半关闭、FIN、RST 和本地关闭。当前实现可以先覆盖核心状态，但状态机边界必须清楚。若 `connect` 尚未完成，非阻塞 `connect` 应返回 `EINPROGRESS`，后续 `poll` 写事件表示连接完成或失败。若 `accept` 队列为空，阻塞 `accept` 睡眠，非阻塞 `accept` 返回 `EAGAIN`。

UDP 套接字保留消息边界。`bind` 决定本地地址和端口。`sendto` 可以指定远端地址。`connect` 对 UDP 只是设置默认远端并过滤接收来源，不建立握手。`recvfrom` 每次返回一个数据报及其来源地址。若把 UDP 当作字节流处理，用户态会看到错误的边界语义。套接字层因此要在文件操作统一入口下保留类型差异。

#continued-table(
  "13-2",
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

== 13.7 阻塞、非阻塞与就绪状态

套接字系统调用必须处理阻塞和非阻塞模式。阻塞 `read` 在无数据时睡眠。非阻塞 `read` 返回 `EAGAIN`。阻塞 `connect` 可以等待握手完成。非阻塞 `connect` 返回 `EINPROGRESS`。`poll` 和 `epoll` 不直接执行输入输出，只报告就绪状态。就绪状态表达协议套接字的用户可见状态，不能简单等同于底层设备可读写。例如 TCP 监听套接字的可读表示 `accept` 队列非空，TCP 连接套接字的可读表示接收缓冲有数据或对端关闭。

我们把等待语义放在套接字文件层和 `NetStack` 的等待队列之间。每个套接字操作先检查协议状态。若可立即完成，直接返回。若不可完成且文件描述符非阻塞，返回 `EAGAIN` 或对应错误。若可阻塞，任务登记到网络等待队列，触发轮询或等待协议栈推进，再睡眠。醒来后重新检查套接字状态。全局唤醒会带来惊群，但每个等待者都重新检查自己的句柄，因此不会破坏正确性。

#pseudo-sample("13-4", [套接字阻塞读取模式], kind: "代码")[
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

== 13.8 路由、地址与接口状态

网络接口挂接后，`NetStack` 根据配置生成直连路由。配置网关后，路由表生成默认或指定网关路由。发送数据包时，协议栈根据目标地址查询路由，选择接口和下一跳。若没有路由，`connect` 或 `sendto` 应返回 `ENETUNREACH` 或相关错误。若接口关闭，返回 `ENETDOWN` 或让套接字进入错误状态。路由错误不能表现为静默丢包，否则用户态很难诊断。

接口状态分为设备活跃、链路可用和管理启用。设备活跃表示驱动对象仍然有效。链路可用表示物理或虚拟链路可用。管理启用表示内核配置允许该接口参与收发。三者不应混为一谈。设备移除时，所有操作都应失败。链路断开时，设备仍然存在，但收发可能不可用。管理关闭是用户或配置主动关闭接口。类型化控制可以分别查询和设置这些状态。

#continued-table(
  "13-3",
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

地址配置也要分层。接口可以同时保存 IPv4 地址、IPv6 地址、前缀长度、广播地址和网关。`IfConfig` 统一描述这些配置，使协议栈挂接时一次性获得地址信息。当前 `NetStack` 已经提供 IPv4 与 IPv6 地址设置、默认路由和静态路由更新入口，路由表也能按照 IPv4 或 IPv6 目标地址选择接口。用户态兼容管理面仍不完全对称。传统 `SIOC*` 接口和当前 netlink 写路径主要覆盖 IPv4 地址、子网掩码、MTU、管理启停和 IPv4 路由，IPv6 的运行期配置更多停留在内核内部接口和查询视图上。这个边界需要在文档中明确，否则容易把底层协议能力误读成完整的用户态网络管理能力。

路由表更新需要和接口注册表保持一致。挂接先创建接口，再更新路由。分离先阻止接口继续参与路由，再移除接口。若路由指向已移除接口，发送路径可能访问无效句柄。我们用接口 ID 作为关联键，分离时清理对应路由。读路径通过锁保护看到一致快照。

== 13.9 回环与主动推进

本机通信大量依赖回环接口。发送和接收都发生在同一内核内。若协议栈只靠周期定时器轮询，发送方写入数据后要等下一个时钟节拍才能被接收方看到，吞吐和延迟都会变差。`poll_now` 的作用就是在套接字操作后主动推进协议栈，使本机数据尽快回灌。

主动推进需要限制轮数。若每次发送都无限轮询，用户态写入会被协议栈处理拖住，甚至在大量连接时长时间占用 CPU。当前调优参数中保留主动轮询参数，设置最大轮数。第一轮通常把出站数据交给设备或回环接口，第二轮处理回灌和状态变化。若没有更多变化，就停止。这个策略兼顾短连接延迟和 CPU 使用。

回环接口还暴露唤醒策略问题。发送方写入后，接收方可能正在 `recv` 阻塞。协议栈轮询处理到接收数据后，应唤醒等待者。全局唤醒会让所有网络等待者重新检查，简单可靠但可能惊群。高并发 TCP 服务器中，精确唤醒更合适。我们先使用全局唤醒保证正确性，再把精确唤醒作为后续优化方向。这个取舍符合当前工程阶段。

主动轮询也不能在持有过多锁时执行。套接字操作可能持有套接字文件锁，`NetStack` 轮询需要接口锁。若锁序设计不清，会出现套接字锁和接口锁互相等待。我们尽量在短临界区内更新套接字状态，调用协议栈推进时避免持有不必要的 VFS 锁。这个边界对网络性能和死锁预防都很重要。

== 13.10 并发、锁序与资源回收

网络对象生命周期复杂。VFS 持有常驻 socket facade，FlowShard 持有协议 flow，设备 ELM 持有 queue 和 DMA pool。关闭、分离、进程退出和设备移除都可能同时发生。当前收束顺序是：停止新调用，标记 socket 和 stack generation 失效，排空 worker 与 queue，撤销 buffer lease，最后销毁对应 ELM 代际。旧 socket 不迁移到新代，而是得到稳定的 `NetworkDown` 或 hangup 语义。

锁序需要特别控制。接口注册表锁保护接口 ID 到接口的映射。接口锁保护协议引擎对象。套接字文件锁保护文件级标志和局部状态。等待队列锁只保护等待者列表。调用顺序应避免在接口锁内执行用户复制，也避免在等待队列锁内进入协议栈轮询。唤醒操作尽量锁外进行。这个模型与第七章同步规则一致。

资源回收时还要唤醒阻塞任务。一个线程阻塞在 `recv`，另一个线程关闭同一个套接字。关闭路径应让 `recv` 返回错误或文件结束，而不是永远睡眠。接口分离时，所有依赖该接口的套接字也应观察到错误。当前全局唤醒策略虽然粗，但能保证状态变化被等待者重新检查。后续精确唤醒不能牺牲这一点。

网络缓冲区分配失败应返回错误或触发背压，不应导致内核崩溃。用户态可以通过高频发送造成内存压力。协议栈和套接字层应返回 `ENOBUFS`、`ENOMEM` 或短写。只有内核基础结构损坏时才进入不可恢复路径。这个原则和前面章节对分配失败的处理一致。

== 13.11 性能与演进视角

网络性能主要受数据复制、协议栈推进、锁竞争、设备队列和唤醒延迟影响。连续 TCP/UDP 吞吐更关注批量收发能力，请求响应型负载更关注系统调用成本、主动轮询和等待者唤醒。边界参数和阻塞语义则更多依赖套接字系统调用兼容层。不同负载暴露的问题不同，因此分析网络性能时需要先判断瓶颈位于数据路径、控制路径还是 ABI 处理。

我们在网络设计中优先保证正确性和可收束。全局唤醒、主动轮询和清晰错误转换会带来一些额外成本，但能让状态变化可靠传递。性能优化可以沿三个方向推进。第一，精确套接字唤醒，减少惊群。第二，减少数据复制和锁持有时间。第三，根据网卡中断和队列能力改进轮询触发。每个方向都可以在现有分层中独立推进。

文件输入输出与网络看似无关，但它们共享用户复制和调度能力。若 `copy_from_user` 慢，网络 `sendmsg` 和 `recvmsg` 也会受影响。若调度唤醒慢，阻塞套接字的延迟也会增大。网络性能分析因此不能只看协议栈。它要结合第七章同步、第八章系统调用、本章轮询和驱动路径共同分析。

网络程序的诊断输出还依赖第十一章的 console。若终端输出慢，会影响日志观察和问题定位。文档把这些关系讲清楚，有助于后续解释网络路径和用户态可观察行为之间的差异。

== 13.12 工程设计总结

网络子系统连接设备驱动、协议栈、VFS 和用户态套接字。它的核心挑战是保持这些层的边界。驱动只提供链路与包收发。协议栈处理接口、路由和协议状态。VFS 负责文件描述符和 POSIX 套接字语义。设备模型负责生命周期和类型化控制。这样的设计使网络子系统可以在当前规模下保持简洁，同时为后续精确套接字唤醒、更多驱动、更完整的 IPv6 用户态管理接口和更细粒度路由策略留下空间。网络不是单纯的设备驱动，也不是单纯的文件接口。它位于设备、协议和 POSIX ABI 的交界处，必须用清晰的分层来控制复杂度。
