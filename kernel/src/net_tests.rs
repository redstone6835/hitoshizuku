//! 网络设备路径的最小内核测试。

use ktest::ktest;

use crate::net_runtime;

static STRESS_SENDER: sched::sync::Spinlock<Option<alloc::sync::Arc<net::SocketFacade>>> =
    sched::sync::Spinlock::new(None);
static STRESS_WRITER_DONE: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);
static BLOCKING_STREAM_SENDER: sched::sync::Spinlock<Option<alloc::sync::Arc<net::SocketFacade>>> =
    sched::sync::Spinlock::new(None);
static BLOCKING_STREAM_WRITTEN: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(usize::MAX);

const BLOCKING_STREAM_BYTES: usize = 256 * 1024;

unsafe extern "C" fn udp_stress_writer(_arg: usize) -> ! {
    let sender = STRESS_SENDER
        .lock()
        .as_ref()
        .cloned()
        .expect("UDP stress sender 未安装");
    for sequence in 0..256u32 {
        sender
            .send(&sequence.to_ne_bytes(), None, false, None)
            .expect("UDP stress send");
    }
    STRESS_WRITER_DONE.store(true, core::sync::atomic::Ordering::Release);
    sched::kthread_finish(sched::ExitCode(0));
}

unsafe extern "C" fn blocking_stream_writer(_arg: usize) -> ! {
    let sender = BLOCKING_STREAM_SENDER
        .lock()
        .as_ref()
        .cloned()
        .expect("blocking TCP sender 未安装");
    let payload = (0..BLOCKING_STREAM_BYTES)
        .map(|index| index.wrapping_mul(17) as u8)
        .collect::<alloc::vec::Vec<_>>();
    let deadline = sched::now_ns_public().saturating_add(5_000_000_000);
    let written = sender
        .send_stream(&payload, false, Some(deadline))
        .expect("blocking TCP send 失败");
    BLOCKING_STREAM_WRITTEN.store(written, core::sync::atomic::Ordering::Release);
    sched::kthread_finish(sched::ExitCode(0));
}

fn current_vfs() -> (
    alloc::sync::Arc<vfs::VfsContext>,
    alloc::sync::Arc<vfs::fdtable::FdTable>,
) {
    let task = sched::current_task();
    let context = task
        .ext_lookup(sched::TASKEXT_VFS_CONTEXT)
        .expect("当前任务缺少 VFS context")
        .downcast::<vfs::VfsContext>()
        .expect("VFS context 类型错误");
    let table = task
        .ext_lookup(sched::TASKEXT_VFS_FDTABLE)
        .expect("当前任务缺少 fdtable")
        .downcast::<vfs::fdtable::FdTable>()
        .expect("fdtable 类型错误");
    (context, table)
}

fn sockaddr_in(address: [u8; 4], port: u16) -> [u8; 16] {
    let mut raw = [0u8; 16];
    raw[..2].copy_from_slice(&vfs::addr::AF_INET.to_ne_bytes());
    raw[2..4].copy_from_slice(&port.to_be_bytes());
    raw[4..8].copy_from_slice(&address);
    raw
}

fn sockaddr_in6(address: [u8; 16], port: u16) -> [u8; 28] {
    let mut raw = [0u8; 28];
    raw[..2].copy_from_slice(&vfs::addr::AF_INET6.to_ne_bytes());
    raw[2..4].copy_from_slice(&port.to_be_bytes());
    raw[8..24].copy_from_slice(&address);
    raw
}

fn wait_poll(file: &vfs::file::File, events: vfs::file::PollEvents) {
    let deadline = sched::now_ns_public().saturating_add(1_000_000_000);
    while sched::now_ns_public() < deadline {
        if file.poll(events).intersect(events).raw() != 0 {
            return;
        }
        let task = sched::current_task();
        if task.cas_state(sched::TaskState::Running, sched::TaskState::Sleeping) {
            let wake = sched::now_ns_public().saturating_add(100_000);
            let _ = sched::register_sleep_deadline(&task, wake);
            drop(task);
            sched::schedule_once(sched::now_ns_public());
        }
    }
    panic!("1 秒内 socket 未出现预期 poll 事件");
}

fn wait_readable(file: &vfs::file::File) {
    wait_poll(file, vfs::file::PollEvents::POLLIN);
}

#[ktest]
fn net_stack_elm_builds_udp_and_tcp_tx_headers() {
    let payload_bytes = b"elm tx payload";
    let payload = net::buf::PacketChain::from_owned(payload_bytes.to_vec());
    let cases = [
        net::stack::NetStackTxInputV1::udp(
            net::IpAddr::V4(net::Ipv4Addr::new(10, 0, 2, 15)),
            net::IpAddr::V4(net::Ipv4Addr::new(10, 0, 2, 2)),
            19002,
            53,
            [1; 6],
            [2; 6],
            64,
            7,
            payload_bytes.len() as u32,
        )
        .unwrap(),
        net::stack::NetStackTxInputV1::udp(
            net::IpAddr::V6(net::Ipv6Addr::LOCALHOST),
            net::IpAddr::V6(net::Ipv6Addr([
                0x20, 1, 0xdb, 8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 2,
            ])),
            19002,
            53,
            [1; 6],
            [2; 6],
            64,
            7,
            payload_bytes.len() as u32,
        )
        .unwrap(),
        net::stack::NetStackTxInputV1::tcp(
            net::IpAddr::V4(net::Ipv4Addr::new(10, 0, 2, 15)),
            net::IpAddr::V4(net::Ipv4Addr::new(10, 0, 2, 2)),
            19003,
            443,
            [1; 6],
            [2; 6],
            1,
            2,
            net::transport::TcpFlags::SYN.bits(),
            32768,
            &[2, 4, 0x05, 0xb4],
            payload_bytes.len() as u32,
        )
        .unwrap(),
        net::stack::NetStackTxInputV1::tcp(
            net::IpAddr::V6(net::Ipv6Addr::LOCALHOST),
            net::IpAddr::V6(net::Ipv6Addr([
                0x20, 1, 0xdb, 8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 2,
            ])),
            19003,
            443,
            [1; 6],
            [2; 6],
            1,
            2,
            net::transport::TcpFlags::SYN.bits(),
            32768,
            &[2, 4, 0x05, 0xb4],
            payload_bytes.len() as u32,
        )
        .unwrap(),
    ];

    for input in cases {
        let output = net::stack::build_tx_header(&payload, input).expect("ELM TX header 构造失败");
        let mut frame = output.header_bytes().to_vec();
        frame.extend_from_slice(payload_bytes);
        let frame = net::buf::PacketChain::from_owned(frame);
        let transport_offset = if input.family == net::stack::NET_STACK_ADDRESS_FAMILY_IPV4 {
            assert_eq!(net::pipeline::packet_checksum(&frame, 14, 20), Ok(0));
            34
        } else {
            54
        };
        let protocol = if input.kind == net::stack::NET_STACK_TX_UDP {
            17
        } else {
            6
        };
        let transport_len = frame.total_len() - transport_offset;
        let source = if input.family == net::stack::NET_STACK_ADDRESS_FAMILY_IPV4 {
            net::IpAddr::V4(net::Ipv4Addr(input.source[..4].try_into().unwrap()))
        } else {
            net::IpAddr::V6(net::Ipv6Addr(input.source))
        };
        let destination = if input.family == net::stack::NET_STACK_ADDRESS_FAMILY_IPV4 {
            net::IpAddr::V4(net::Ipv4Addr(input.destination[..4].try_into().unwrap()))
        } else {
            net::IpAddr::V6(net::Ipv6Addr(input.destination))
        };
        assert_eq!(
            net::pipeline::transport_checksum(
                &frame,
                transport_offset,
                transport_len,
                source,
                destination,
                protocol,
            ),
            Ok(0)
        );
    }
}

#[ktest]
fn net_stack_elm_builds_udp_and_raw_fragment_headers() {
    let payload_bytes = (0u32..2200)
        .map(|index| index.wrapping_mul(13) as u8)
        .collect::<alloc::vec::Vec<_>>();
    for (source, destination, family) in [
        (
            net::IpAddr::V4(net::Ipv4Addr::new(10, 0, 2, 15)),
            net::IpAddr::V4(net::Ipv4Addr::new(10, 0, 2, 2)),
            net::stack::NET_STACK_ADDRESS_FAMILY_IPV4,
        ),
        (
            net::IpAddr::V6(net::Ipv6Addr::LOCALHOST),
            net::IpAddr::V6(net::Ipv6Addr([
                0x20, 1, 0xdb, 8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 2,
            ])),
            net::stack::NET_STACK_ADDRESS_FAMILY_IPV6,
        ),
    ] {
        let payload = net::buf::PacketChain::from_owned(payload_bytes.clone());
        let mut offset = 0u32;
        let mut reconstructed = alloc::vec::Vec::new();
        loop {
            let input = net::stack::NetStackTxFragmentInputV1::udp(
                source,
                destination,
                19002,
                53,
                [1; 6],
                [2; 6],
                64,
                7,
                payload_bytes.len() as u32,
                600,
                7,
                offset,
            )
            .unwrap();
            let output = net::stack::build_tx_fragment_header(&payload, input)
                .expect("ELM UDP 分片 header 构造失败");
            assert!(output.header_len as usize + output.payload_len as usize <= 614);
            assert_eq!(output.payload_offset, offset);
            assert!(output.payload_len != 0);
            if family == net::stack::NET_STACK_ADDRESS_FAMILY_IPV4 {
                assert_eq!(output.header_len, if offset == 0 { 42 } else { 34 });
                assert_eq!(
                    net::pipeline::packet_checksum(
                        &net::buf::PacketChain::from_owned(output.header_bytes().to_vec()),
                        14,
                        20,
                    ),
                    Ok(0)
                );
            } else {
                assert_eq!(output.header_len, if offset == 0 { 70 } else { 62 });
            }
            let mut bytes = alloc::vec![0; output.payload_len as usize];
            payload
                .copy_out(output.payload_offset as usize, &mut bytes)
                .expect("复制 ELM 分片 payload");
            reconstructed.extend_from_slice(&bytes);
            if output.more_fragments == 0 {
                break;
            }
            offset = output.next_fragment_offset;
        }
        assert_eq!(reconstructed, payload_bytes);
    }

    let raw_header_len = 24usize;
    let mut raw_bytes = alloc::vec![0; raw_header_len + 1600];
    raw_bytes[0] = 0x46;
    let raw_len = raw_bytes.len() as u16;
    raw_bytes[2..4].copy_from_slice(&raw_len.to_be_bytes());
    raw_bytes[4..6].copy_from_slice(&7u16.to_be_bytes());
    raw_bytes[8] = 64;
    raw_bytes[9] = 99;
    raw_bytes[12..16].copy_from_slice(&[0, 0, 0, 0]);
    raw_bytes[16..20].copy_from_slice(&[10, 0, 2, 2]);
    raw_bytes[20..24].copy_from_slice(&[1, 1, 0, 0]);
    let raw = net::buf::PacketChain::from_owned(raw_bytes.clone());
    let mut offset = 0u32;
    let mut reconstructed = alloc::vec::Vec::new();
    loop {
        let input = net::stack::NetStackTxFragmentInputV1::raw_ipv4(
            net::IpAddr::V4(net::Ipv4Addr::new(10, 0, 2, 15)),
            net::IpAddr::V4(net::Ipv4Addr::new(10, 0, 2, 2)),
            [1; 6],
            [2; 6],
            raw_bytes.len() as u32,
            600,
            1,
            offset,
            0,
            0,
        )
        .unwrap();
        let output = net::stack::build_tx_fragment_header(&raw, input)
            .expect("ELM raw 分片 header 构造失败");
        assert_eq!(output.header_len, 14 + raw_header_len as u16);
        assert!(output.header_len as usize + output.payload_len as usize <= 614);
        let header = net::buf::PacketChain::from_owned(output.header_bytes().to_vec());
        assert_eq!(
            net::pipeline::packet_checksum(&header, 14, raw_header_len),
            Ok(0)
        );
        let mut bytes = alloc::vec![0; output.payload_len as usize];
        raw.copy_out(output.payload_offset as usize, &mut bytes)
            .expect("复制 ELM raw 分片 payload");
        reconstructed.extend_from_slice(&bytes);
        if output.more_fragments == 0 {
            break;
        }
        offset = output.next_fragment_offset;
    }
    assert_eq!(reconstructed, raw_bytes[raw_header_len..]);
}

#[ktest]
fn udp_vfs_fd_and_epoll_roundtrip() {
    let (context, table) = current_vfs();
    let raw = vfs::socket::socket(
        &context,
        &table,
        vfs::addr::AF_INET as usize,
        vfs::socket::SOCK_RAW | vfs::socket::SOCK_NONBLOCK,
        1,
    )
    .expect("创建 IPv4 ICMP raw socket");
    table.close_fd(raw).expect("关闭 IPv4 ICMP raw socket");

    let ipv6 = vfs::socket::socket(
        &context,
        &table,
        vfs::addr::AF_INET6 as usize,
        vfs::socket::SOCK_DGRAM | vfs::socket::SOCK_NONBLOCK,
        17,
    )
    .expect("创建 IPv6 UDP socket");
    table.close_fd(ipv6).expect("关闭 IPv6 UDP socket");

    let receiver = vfs::socket::socket(
        &context,
        &table,
        vfs::addr::AF_INET as usize,
        vfs::socket::SOCK_DGRAM | vfs::socket::SOCK_NONBLOCK,
        0,
    )
    .expect("创建 UDP receiver");
    let local = sockaddr_in([127, 0, 0, 1], 19_002);
    vfs::socket::bind(&context, &table, receiver, &local).expect("绑定 UDP receiver");

    let conflict = vfs::socket::socket(
        &context,
        &table,
        vfs::addr::AF_INET as usize,
        vfs::socket::SOCK_DGRAM | vfs::socket::SOCK_NONBLOCK,
        17,
    )
    .expect("创建冲突 UDP socket");
    assert_eq!(
        vfs::socket::bind(&context, &table, conflict, &local),
        Err(errno::Errno::EADDRINUSE)
    );
    table.close_fd(conflict).expect("关闭冲突 UDP socket");

    let sender = vfs::socket::socket(
        &context,
        &table,
        vfs::addr::AF_INET as usize,
        vfs::socket::SOCK_DGRAM | vfs::socket::SOCK_NONBLOCK,
        17,
    )
    .expect("创建 UDP sender");
    vfs::socket::connect(&context, &table, sender, &local).expect("连接 UDP receiver");
    let sender_alias = table.dup_fd(sender).expect("dup UDP sender");
    assert!(alloc::sync::Arc::ptr_eq(
        &table.get_file(sender).unwrap(),
        &table.get_file(sender_alias).unwrap()
    ));
    let forked = table.fork();
    assert!(alloc::sync::Arc::ptr_eq(
        &table.get_file(sender_alias).unwrap(),
        &forked.get_file(sender_alias).unwrap()
    ));
    table.close_fd(sender).expect("关闭原 sender fd");

    let protocol = vfs::socket::getsockopt(
        &table,
        sender_alias,
        vfs::socket::SOL_SOCKET,
        vfs::socket::SO_PROTOCOL,
    )
    .expect("读取 SO_PROTOCOL");
    assert_eq!(i32::from_ne_bytes(protocol[..4].try_into().unwrap()), 17);
    vfs::socket::setsockopt(
        &table,
        sender_alias,
        vfs::socket::SOL_SOCKET,
        vfs::socket::SO_SNDBUF,
        &(1024 * 1024i32).to_ne_bytes(),
    )
    .expect("设置 SO_SNDBUF");
    let send_buffer = vfs::socket::getsockopt(
        &table,
        sender_alias,
        vfs::socket::SOL_SOCKET,
        vfs::socket::SO_SNDBUF,
    )
    .expect("读取 SO_SNDBUF");
    assert_eq!(
        i32::from_ne_bytes(send_buffer[..4].try_into().unwrap()),
        512 * 1024
    );

    let epoll = vfs::epoll::create(&table, context.cred(), false).expect("创建 epoll");
    vfs::epoll::ctl(
        &table,
        epoll,
        vfs::epoll::EPOLL_CTL_ADD,
        receiver,
        Some(vfs::epoll::EpollEvent {
            events: vfs::file::PollEvents::POLLIN.raw() as u32,
            data: 0x55,
        }),
    )
    .expect("注册 UDP epoll watch");
    assert_eq!(
        vfs::socket::send(&context, &table, sender_alias, b"abcdefgh", &[], None, 0,),
        Ok(8)
    );
    let receiver_file = table.get_file(receiver).unwrap();
    wait_readable(&receiver_file);
    let events = vfs::epoll::wait(&table, epoll, 1, 0).expect("读取 UDP epoll event");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data, 0x55);

    let mut short = [0u8; 4];
    let peeked = vfs::socket::recv(
        &table,
        receiver,
        &mut short,
        0,
        false,
        vfs::socket::MSG_PEEK | vfs::socket::MSG_TRUNC,
        None,
    )
    .expect("peek UDP datagram");
    assert_eq!(peeked.len, 8);
    assert_ne!(peeked.msg_flags & vfs::socket::MSG_TRUNC, 0);
    let received = vfs::socket::recv(&table, receiver, &mut short, 0, false, 0, None)
        .expect("消费 UDP datagram");
    assert_eq!(received.len, 4);
    assert_eq!(short, *b"abcd");

    let jumbo = (0usize..60_000)
        .map(|index| index.wrapping_mul(31) as u8)
        .collect::<alloc::vec::Vec<_>>();
    assert_eq!(
        vfs::socket::send(&context, &table, sender_alias, &jumbo, &[], None, 0),
        Ok(jumbo.len())
    );
    wait_readable(&receiver_file);
    let mut jumbo_received = alloc::vec![0; jumbo.len()];
    let received = vfs::socket::recv(&table, receiver, &mut jumbo_received, 0, false, 0, None)
        .expect("接收 jumbo UDP datagram");
    assert_eq!(received.len, jumbo.len());
    assert_eq!(jumbo_received, jumbo);
    assert_eq!(
        vfs::socket::send(
            &context,
            &table,
            sender_alias,
            b"x",
            &[],
            None,
            vfs::socket::MSG_MORE,
        ),
        Err(errno::Errno::EOPNOTSUPP)
    );

    table.close_fd(epoll).expect("关闭 epoll");
    table.close_fd(receiver).expect("关闭 receiver");
    table.close_fd(sender_alias).expect("关闭 sender alias");
}

#[ktest]
fn tcp_vfs_loopback_connect_accept_stream_and_eof() {
    let (context, table) = current_vfs();
    let server = vfs::socket::socket(
        &context,
        &table,
        vfs::addr::AF_INET as usize,
        vfs::socket::SOCK_STREAM | vfs::socket::SOCK_NONBLOCK,
        6,
    )
    .expect("创建 TCP server");
    let local = sockaddr_in([127, 0, 0, 1], 19_004);
    vfs::socket::bind(&context, &table, server, &local).expect("绑定 TCP server");
    vfs::socket::listen(&table, server, 4).expect("监听 TCP server");

    let epoll = vfs::epoll::create(&table, context.cred(), false).expect("创建 TCP epoll");
    vfs::epoll::ctl(
        &table,
        epoll,
        vfs::epoll::EPOLL_CTL_ADD,
        server,
        Some(vfs::epoll::EpollEvent {
            events: vfs::file::PollEvents::POLLIN.raw() as u32,
            data: 0x66,
        }),
    )
    .expect("注册 TCP listener epoll watch");

    let client = vfs::socket::socket(
        &context,
        &table,
        vfs::addr::AF_INET as usize,
        vfs::socket::SOCK_STREAM | vfs::socket::SOCK_NONBLOCK,
        6,
    )
    .expect("创建 TCP client");
    assert_eq!(
        vfs::socket::connect(&context, &table, client, &local),
        Err(errno::Errno::EINPROGRESS)
    );
    assert_eq!(
        vfs::socket::connect(&context, &table, client, &local),
        Err(errno::Errno::EALREADY)
    );

    let server_file = table.get_file(server).unwrap();
    wait_readable(&server_file);
    let events = vfs::epoll::wait(&table, epoll, 1, 0).expect("读取 TCP accept epoll event");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data, 0x66);
    let client_file = table.get_file(client).unwrap();
    wait_poll(&client_file, vfs::file::PollEvents::POLLOUT);
    let error = vfs::socket::getsockopt(
        &table,
        client,
        vfs::socket::SOL_SOCKET,
        vfs::socket::SO_ERROR,
    )
    .expect("读取 TCP connect SO_ERROR");
    assert_eq!(i32::from_ne_bytes(error[..4].try_into().unwrap()), 0);

    let (accepted, peer) =
        vfs::socket::accept(&context, &table, server, vfs::socket::SOCK_NONBLOCK)
            .expect("accept TCP child");
    assert!(peer.is_some());

    let client_facade = client_file
        .downcast_ops::<vfs::net_socket::NetSocketFileOps>()
        .expect("TCP client 缺少网络 socket ops")
        .facade()
        .clone();
    client_facade.set_buffer_limits(Some(16 * 1024), None);
    BLOCKING_STREAM_WRITTEN.store(usize::MAX, core::sync::atomic::Ordering::Release);
    *BLOCKING_STREAM_SENDER.lock() = Some(client_facade.clone());
    let blocking_writer = sched::kthread_create(
        blocking_stream_writer,
        0,
        sched::SchedParams {
            nice: 0,
            slice_ns: 0,
        },
    );
    sched::activate_task(&blocking_writer).expect("启动 blocking TCP writer");
    let accepted_file = table.get_file(accepted).unwrap();
    let deadline = sched::now_ns_public().saturating_add(5_000_000_000);
    let mut blocking_received = 0usize;
    let mut blocking_window = alloc::vec![0; 64 * 1024];
    while blocking_received < BLOCKING_STREAM_BYTES {
        match vfs::socket::recv(
            &table,
            accepted,
            &mut blocking_window,
            0,
            false,
            vfs::socket::MSG_DONTWAIT,
            None,
        ) {
            Ok(output) if output.len != 0 => {
                for (offset, byte) in blocking_window[..output.len].iter().enumerate() {
                    assert_eq!(*byte, (blocking_received + offset).wrapping_mul(17) as u8);
                }
                blocking_received += output.len;
            }
            Ok(_) | Err(errno::Errno::EAGAIN) => {
                if sched::now_ns_public() >= deadline {
                    panic!(
                        "blocking TCP send 超时: received={} total={}",
                        blocking_received, BLOCKING_STREAM_BYTES
                    );
                }
                let task = sched::current_task();
                if task.cas_state(sched::TaskState::Running, sched::TaskState::Sleeping) {
                    let wake = sched::now_ns_public().saturating_add(100_000);
                    let _ = sched::register_sleep_deadline(&task, wake);
                    drop(task);
                    sched::schedule_once(sched::now_ns_public());
                }
            }
            Err(error) => panic!("blocking TCP recv 失败: {:?}", error),
        }
    }
    while BLOCKING_STREAM_WRITTEN.load(core::sync::atomic::Ordering::Acquire) == usize::MAX {
        if sched::now_ns_public() >= deadline {
            panic!("blocking TCP writer 未按时结束");
        }
        let task = sched::current_task();
        if task.cas_state(sched::TaskState::Running, sched::TaskState::Sleeping) {
            let wake = sched::now_ns_public().saturating_add(100_000);
            let _ = sched::register_sleep_deadline(&task, wake);
            drop(task);
            sched::schedule_once(sched::now_ns_public());
        }
    }
    assert_eq!(
        BLOCKING_STREAM_WRITTEN.load(core::sync::atomic::Ordering::Acquire),
        BLOCKING_STREAM_BYTES
    );
    *BLOCKING_STREAM_SENDER.lock() = None;
    client_facade.set_buffer_limits(Some(256 * 1024), None);

    assert_eq!(
        vfs::socket::send(&context, &table, client, b"ping", &[], None, 0),
        Ok(4)
    );
    wait_readable(&accepted_file);
    let mut bytes = [0u8; 8];
    let received = vfs::socket::recv(&table, accepted, &mut bytes, 0, false, 0, None)
        .expect("server 接收 TCP payload");
    assert_eq!(received.len, 4);
    assert_eq!(&bytes[..4], b"ping");

    assert_eq!(
        vfs::socket::send(&context, &table, accepted, b"pong", &[], None, 0),
        Ok(4)
    );
    wait_readable(&client_file);
    bytes.fill(0);
    let received = vfs::socket::recv(&table, client, &mut bytes, 0, false, 0, None)
        .expect("client 接收 TCP payload");
    assert_eq!(received.len, 4);
    assert_eq!(&bytes[..4], b"pong");

    let stream = (0usize..2 * 1024 * 1024)
        .map(|index| index.wrapping_mul(13) as u8)
        .collect::<alloc::vec::Vec<_>>();
    let deadline = sched::now_ns_public().saturating_add(5_000_000_000);
    let mut sent = 0usize;
    let mut consumed = 0usize;
    let mut receive_window = alloc::vec![0; 64 * 1024];
    while consumed < stream.len() {
        let mut progressed = false;
        if sent < stream.len() {
            match vfs::socket::send(
                &context,
                &table,
                client,
                &stream[sent..],
                &[],
                None,
                vfs::socket::MSG_DONTWAIT,
            ) {
                Ok(written) => {
                    sent += written;
                    progressed |= written != 0;
                }
                Err(errno::Errno::EAGAIN) => {}
                Err(error) => panic!("jumbo TCP send 失败: {:?}", error),
            }
        }
        loop {
            match vfs::socket::recv(
                &table,
                accepted,
                &mut receive_window,
                0,
                false,
                vfs::socket::MSG_DONTWAIT,
                None,
            ) {
                Ok(output) if output.len != 0 => {
                    assert_eq!(
                        &receive_window[..output.len],
                        &stream[consumed..consumed + output.len]
                    );
                    consumed += output.len;
                    progressed = true;
                }
                Ok(_) | Err(errno::Errno::EAGAIN) => break,
                Err(error) => panic!("jumbo TCP recv 失败: {:?}", error),
            }
        }
        if sched::now_ns_public() >= deadline {
            panic!(
                "jumbo TCP loopback 超时: sent={} consumed={} total={}",
                sent,
                consumed,
                stream.len()
            );
        }
        if !progressed {
            let task = sched::current_task();
            if task.cas_state(sched::TaskState::Running, sched::TaskState::Sleeping) {
                let wake = sched::now_ns_public().saturating_add(100_000);
                let _ = sched::register_sleep_deadline(&task, wake);
                drop(task);
                sched::schedule_once(sched::now_ns_public());
            }
        }
    }
    assert_eq!(sent, stream.len());

    vfs::socket::shutdown(&table, client, vfs::socket::SHUT_WR).expect("关闭 client 写方向");
    wait_readable(&accepted_file);
    let eof = vfs::socket::recv(&table, accepted, &mut bytes, 0, false, 0, None)
        .expect("server 观察 TCP EOF");
    assert_eq!(eof.len, 0);

    table.close_fd(accepted).expect("关闭 accepted TCP fd");
    table.close_fd(client).expect("关闭 TCP client");
    table.close_fd(server).expect("关闭 TCP server");
    table.close_fd(epoll).expect("关闭 TCP epoll");
}

#[ktest]
fn udp_blocking_reader_writer_stress() {
    STRESS_WRITER_DONE.store(false, core::sync::atomic::Ordering::Release);
    let receiver = net::new_socket_facade(net::AddressFamily::Ipv4).expect("创建 UDP receiver");
    let local = net::Endpoint {
        addr: net::IpAddr::V4(net::Ipv4Addr::LOCALHOST),
        port: 19_003,
    };
    receiver
        .bind(local, None, net::control::BindOptions::default())
        .expect("绑定 UDP stress receiver");
    let sender = net::new_socket_facade(net::AddressFamily::Ipv4).expect("创建 UDP sender");
    sender
        .connect(local, None, net::control::BindOptions::default())
        .expect("连接 UDP stress receiver");
    *STRESS_SENDER.lock() = Some(alloc::sync::Arc::clone(&sender));

    let writer = sched::kthread_create(
        udp_stress_writer,
        0,
        sched::SchedParams {
            nice: 0,
            slice_ns: 0,
        },
    );
    sched::activate_task(&writer).expect("启动 UDP stress writer");
    let deadline = sched::now_ns_public().saturating_add(5_000_000_000);
    for expected in 0..256u32 {
        let mut bytes = [0u8; 4];
        let received = receiver
            .recv(&mut bytes, false, false, false, Some(deadline))
            .expect("UDP stress recv");
        assert_eq!(received.len, 4);
        assert_eq!(u32::from_ne_bytes(bytes), expected);
    }
    assert!(STRESS_WRITER_DONE.load(core::sync::atomic::Ordering::Acquire));
    *STRESS_SENDER.lock() = None;
    sender.close();
    receiver.close();
}

#[ktest]
fn udp_socket_facade_loopback_roundtrip() {
    let receiver = net::new_socket_facade(net::AddressFamily::Ipv4).expect("创建 UDP facade");
    let receiver_alias = alloc::sync::Arc::clone(&receiver);
    let local = net::Endpoint {
        addr: net::IpAddr::V4(net::Ipv4Addr::LOCALHOST),
        port: 19_001,
    };
    receiver
        .bind(local, None, net::control::BindOptions::default())
        .expect("绑定 UDP receiver");
    let sender = net::new_socket_facade(net::AddressFamily::Ipv4).expect("创建 UDP sender");
    sender
        .connect(local, None, net::control::BindOptions::default())
        .expect("连接 UDP receiver");
    let mut empty = [0u8; 4];
    assert!(matches!(
        receiver.recv(&mut empty, false, false, true, None),
        Err(net::SocketError::WouldBlock)
    ));
    assert_eq!(sender.send(b"ping", None, false, None), Ok(4));
    let deadline = sched::now_ns_public().saturating_add(1_000_000_000);
    let mut payload = [0u8; 4];
    let received = receiver_alias
        .recv(&mut payload, false, false, false, Some(deadline))
        .expect("接收 UDP datagram");
    assert_eq!(payload, *b"ping");
    assert_eq!(received.source.port, sender.local_endpoint().unwrap().port);
    receiver.shutdown(true, false).expect("关闭 UDP 读方向");
    assert_eq!(
        receiver
            .recv(&mut empty, false, false, true, None)
            .unwrap()
            .len,
        0
    );
    sender.close();
    receiver.close();
}

#[ktest]
fn udp_send_then_close_preserves_accepted_datagram() {
    let receiver = net::new_socket_facade(net::AddressFamily::Ipv4).expect("创建 UDP receiver");
    let local = net::Endpoint {
        addr: net::IpAddr::V4(net::Ipv4Addr::LOCALHOST),
        port: 19_004,
    };
    receiver
        .bind(local, None, net::control::BindOptions::default())
        .expect("绑定 UDP receiver");
    let sender = net::new_socket_facade(net::AddressFamily::Ipv4).expect("创建 UDP sender");
    sender
        .connect(local, None, net::control::BindOptions::default())
        .expect("连接 UDP receiver");

    assert_eq!(sender.send(b"last", None, false, None), Ok(4));
    sender.close();

    let deadline = sched::now_ns_public().saturating_add(1_000_000_000);
    let mut payload = [0u8; 4];
    let received = receiver
        .recv(&mut payload, false, false, false, Some(deadline))
        .expect("close 后仍应收到已被 send 接受的 UDP datagram");
    assert_eq!(received.len, payload.len());
    assert_eq!(payload, *b"last");
    receiver.close();
}

#[ktest]
fn virtio_user_network_arp_roundtrip() {
    if !net_runtime::physical_network_available() {
        return;
    }
    net_runtime::request_arp_probe();
    let deadline = sched::now_ns_public().saturating_add(3_000_000_000);
    while sched::now_ns_public() < deadline {
        if net_runtime::arp_probe_complete() {
            return;
        }
        let task = sched::current_task();
        if task.cas_state(sched::TaskState::Running, sched::TaskState::Sleeping) {
            let wake = sched::now_ns_public().saturating_add(1_000_000);
            let _ = sched::register_sleep_deadline(&task, wake);
            drop(task);
            sched::schedule_once(sched::now_ns_public());
        }
    }
    panic!("3 秒内未观察到 QEMU user networking 的 ARP reply");
}

#[ktest]
fn udp_loopback_frontend_roundtrip() {
    net_runtime::request_udp_loopback_probe();
    let deadline = sched::now_ns_public().saturating_add(3_000_000_000);
    while sched::now_ns_public() < deadline {
        if net_runtime::udp_loopback_probe_complete() {
            return;
        }
        let task = sched::current_task();
        if task.cas_state(sched::TaskState::Running, sched::TaskState::Sleeping) {
            let wake = sched::now_ns_public().saturating_add(1_000_000);
            let _ = sched::register_sleep_deadline(&task, wake);
            drop(task);
            sched::schedule_once(sched::now_ns_public());
        }
    }
    let stats = net::device::snapshot_stats();
    let value = |key| {
        stats
            .iter()
            .find(|stat| stat.key == key)
            .map_or(0, |stat| stat.value)
    };
    panic!(
        "3 秒内未完成 UDP loopback frontend 闭环: poll={} rx={} tx={} tx_err={} fatal_gone={} proto_udp={} proto_tx={} dirty={} time_budget={}",
        value("poll_total"),
        value("rx_packets"),
        value("tx_packets"),
        value("tx_errors"),
        value("fatal_device_gone"),
        value("protocol_udp_delivered"),
        value("protocol_tx_formed"),
        value("protocol_dirty_runs"),
        value("budget_time"),
    );
}

#[ktest]
fn virtio_udp_dns_roundtrip() {
    if !net_runtime::physical_network_available() {
        return;
    }
    net_runtime::request_physical_udp_probe();
    let deadline = sched::now_ns_public().saturating_add(5_000_000_000);
    while sched::now_ns_public() < deadline {
        if net_runtime::physical_udp_probe_complete() {
            return;
        }
        let task = sched::current_task();
        if task.cas_state(sched::TaskState::Running, sched::TaskState::Sleeping) {
            let wake = sched::now_ns_public().saturating_add(1_000_000);
            let _ = sched::register_sleep_deadline(&task, wake);
            drop(task);
            sched::schedule_once(sched::now_ns_public());
        }
    }
    if net_runtime::physical_udp_probe_complete() {
        return;
    }
    panic!(
        "5 秒内未完成 QEMU DNS UDP 收发与 buffer 回收: {:?}",
        net_runtime::physical_udp_probe_state()
    );
}

#[ktest]
fn socket_priority_maps_to_four_tx_classes() {
    assert_eq!(net_runtime::tx_priority_class(-1), 0);
    assert_eq!(net_runtime::tx_priority_class(0), 0);
    assert_eq!(net_runtime::tx_priority_class(2), 1);
    assert_eq!(net_runtime::tx_priority_class(5), 2);
    assert_eq!(net_runtime::tx_priority_class(6), 3);
}

#[ktest]
fn multicast_reports_have_valid_checksums() {
    let interface = net::InterfaceId(77);
    let ipv6 = net::Ipv6Addr([0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xfe, 0, 0, 0, 1]);
    let config = net::control::ConfigSnapshot::new(
        1,
        alloc::vec![net::control::InterfaceSnapshot {
            id: interface,
            device: net::NetDeviceId(77),
            mac_address: [2, 0, 0, 0, 0, 1],
            mtu: 1500,
            running: true,
            loopback: false,
        }],
        alloc::vec![
            net::control::AddressEntry {
                interface,
                address: net::IpAddr::V4(net::Ipv4Addr::new(10, 0, 0, 1)),
                prefix_len: 24,
                primary: true,
            },
            net::control::AddressEntry {
                interface,
                address: net::IpAddr::V6(ipv6),
                prefix_len: 64,
                primary: true,
            },
        ],
        alloc::vec![],
        alloc::vec![],
    )
    .unwrap();
    let igmp = net_runtime::build_multicast_control_frame(
        interface,
        net::IpAddr::V4(net::Ipv4Addr::new(239, 1, 2, 3)),
        true,
        &config,
    )
    .unwrap();
    assert_eq!(igmp[38], 0x16);
    assert_eq!(net::pipeline::checksum_bytes(&igmp[14..38]), 0);
    assert_eq!(net::pipeline::checksum_bytes(&igmp[38..]), 0);

    let group = net::Ipv6Addr([0xff, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3]);
    let mld = net_runtime::build_multicast_control_frame(
        interface,
        net::IpAddr::V6(group),
        true,
        &config,
    )
    .unwrap();
    assert_eq!(mld[62], 131);
    assert_eq!(
        net::pipeline::transport_checksum(
            &net::buf::PacketChain::from_owned(mld),
            62,
            24,
            net::IpAddr::V6(ipv6),
            net::IpAddr::V6(group),
            58,
        )
        .unwrap(),
        0
    );
}

#[ktest]
fn ipv6_wildcard_listener_accepts_ipv4_mapped_peer() {
    let (context, table) = current_vfs();
    let server = vfs::socket::socket(
        &context,
        &table,
        vfs::addr::AF_INET6 as usize,
        vfs::socket::SOCK_STREAM | vfs::socket::SOCK_NONBLOCK,
        6,
    )
    .expect("创建 dual-stack TCP server");
    let local6 = sockaddr_in6([0; 16], 19_006);
    vfs::socket::bind(&context, &table, server, &local6).expect("绑定 IPv6 wildcard");
    vfs::socket::listen(&table, server, 4).expect("监听 IPv6 wildcard");

    let client = vfs::socket::socket(
        &context,
        &table,
        vfs::addr::AF_INET as usize,
        vfs::socket::SOCK_STREAM | vfs::socket::SOCK_NONBLOCK,
        6,
    )
    .expect("创建 IPv4 TCP client");
    let local4 = sockaddr_in([127, 0, 0, 1], 19_006);
    assert_eq!(
        vfs::socket::connect(&context, &table, client, &local4),
        Err(errno::Errno::EINPROGRESS)
    );
    let server_file = table.get_file(server).unwrap();
    wait_readable(&server_file);
    let (accepted, peer) =
        vfs::socket::accept(&context, &table, server, vfs::socket::SOCK_NONBLOCK)
            .expect("accept IPv4-mapped child");
    let peer = peer.expect("accept 返回 peer address");
    assert_eq!(
        u16::from_ne_bytes(peer[..2].try_into().unwrap()),
        vfs::addr::AF_INET6
    );
    assert_eq!(&peer[8..20], &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff]);
    assert_eq!(&peer[20..24], &[127, 0, 0, 1]);
    table.close_fd(accepted).expect("关闭 mapped child");
    table.close_fd(client).expect("关闭 IPv4 client");
    table.close_fd(server).expect("关闭 IPv6 server");
}

#[ktest]
fn dhcp_rebind_deadline_stays_between_renew_and_expiry() {
    assert_eq!(net_runtime::dhcp_rebind_seconds(800, 400, None), 700);
    assert_eq!(net_runtime::dhcp_rebind_seconds(800, 400, Some(600)), 600);
    assert_eq!(net_runtime::dhcp_rebind_seconds(800, 799, Some(1)), 799);
}

#[ktest]
fn running_loopback_detach_completes() {
    assert_eq!(
        net_runtime::remove_loopback_for_test(),
        Err(net::device::NetDeviceRemoveError::Busy),
        "ELM-owned loopback must reject registrar-only detach"
    );
}
