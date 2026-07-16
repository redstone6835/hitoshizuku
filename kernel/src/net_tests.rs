//! 网络设备路径的最小内核测试。

use ktest::ktest;

use crate::net_runtime;

static STRESS_SENDER: sched::sync::Spinlock<Option<alloc::sync::Arc<net::SocketFacade>>> =
    sched::sync::Spinlock::new(None);
static STRESS_WRITER_DONE: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

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

fn wait_readable(file: &vfs::file::File) {
    let deadline = sched::now_ns_public().saturating_add(1_000_000_000);
    while sched::now_ns_public() < deadline {
        if file
            .poll(vfs::file::PollEvents::POLLIN)
            .has(vfs::file::PollEvents::POLLIN)
        {
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
    panic!("1 秒内 UDP socket 未变为可读");
}

#[ktest]
fn udp_vfs_fd_and_epoll_roundtrip() {
    let (context, table) = current_vfs();
    let unsupported = errno::Errno::Other(93);
    assert_eq!(
        vfs::socket::socket(
            &context,
            &table,
            vfs::addr::AF_INET as usize,
            vfs::socket::SOCK_STREAM,
            6,
        ),
        Err(unsupported)
    );
    assert_eq!(
        vfs::socket::socket(
            &context,
            &table,
            vfs::addr::AF_INET as usize,
            vfs::socket::SOCK_RAW,
            1,
        ),
        Err(unsupported)
    );

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
fn virtio_user_network_arp_roundtrip() {
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
    panic!("3 秒内未完成 UDP loopback frontend 闭环");
}

#[ktest]
fn virtio_udp_dns_roundtrip() {
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
fn running_loopback_detach_completes() {
    net_runtime::remove_loopback_for_test().expect("running loopback detach 必须完成");
    assert!(
        net::device::snapshot_devices()
            .iter()
            .all(|device| device.name.as_ref() != "lo")
    );
}
