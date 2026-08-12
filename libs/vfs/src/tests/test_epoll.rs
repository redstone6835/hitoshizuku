//! epoll 对象接纳、嵌套图和就绪轮转测试。

extern crate std;

use alloc::sync::Arc;

use errno::Errno;
use ktest::ktest;

use crate::cred::Credentials;
use crate::epoll::{self, EPOLL_CTL_ADD, EPOLL_CTL_DEL, EpollEvent};
use crate::fdtable::{FdFlags, FdTable};
use crate::file::PollEvents;
use crate::limits::VfsLimits;
use crate::{eventfd, memfd, pipe};

fn test_fdtable() -> FdTable {
    FdTable::new(&VfsLimits::default())
}

fn root_cred() -> Arc<Credentials> {
    Arc::new(Credentials::root())
}

/// 有限空 epoll 应只保留有界自旋尾段，不能按固定 10ms 分段唤醒。
#[ktest]
fn epoll_empty_finite_wait_uses_bounded_spin_tail() {
    assert_eq!(
        epoll::wait_recheck_deadline(1_000, Some(25_000_000), true, false),
        Some(23_000_000)
    );
}

/// 缺少精确 waiter 的事件源必须保留 10ms 周期复查。
#[ktest]
fn epoll_unregistered_source_uses_bounded_recheck() {
    assert_eq!(
        epoll::wait_recheck_deadline(1_000, Some(25_000_000), false, true),
        Some(10_001_000)
    );
}

/// 无限等待的空 epoll 仍需低频复查，以观察后续 epoll_ctl 变更。
#[ktest]
fn epoll_empty_infinite_wait_keeps_recheck() {
    assert_eq!(
        epoll::wait_recheck_deadline(1_000, None, true, false),
        Some(10_001_000)
    );
}

/// 普通文件即使对 poll 表现为立即可读写，也不能加入 epoll。
#[ktest]
fn epoll_rejects_regular_file() {
    let fdt = test_fdtable();
    let cred = root_cred();
    let epfd = epoll::create(&fdt, Arc::clone(&cred), false).unwrap();
    let memfd = memfd::create(&fdt, cred, false, false).unwrap();

    let result = epoll::ctl(
        &fdt,
        epfd,
        EPOLL_CTL_ADD,
        memfd,
        Some(EpollEvent {
            events: PollEvents::POLLIN.raw() as u32,
            data: 1,
        }),
    );

    assert_eq!(result, Err(Errno::EPERM));
}

/// 最多允许五个 epoll 实例形成单向嵌套链。
#[ktest]
fn epoll_rejects_sixth_nesting_level() {
    let fdt = test_fdtable();
    let cred = root_cred();
    let (read_end, _write_end) = pipe::new_pipe(Arc::clone(&cred), false).unwrap();
    let mut inner = fdt.alloc_fd(read_end, FdFlags::default()).unwrap();

    for depth in 0..5 {
        let outer = epoll::create(&fdt, Arc::clone(&cred), false).unwrap();
        epoll::ctl(
            &fdt,
            outer,
            EPOLL_CTL_ADD,
            inner,
            Some(EpollEvent {
                events: PollEvents::POLLIN.raw() as u32,
                data: depth,
            }),
        )
        .unwrap();
        inner = outer;
    }

    let sixth = epoll::create(&fdt, cred, false).unwrap();
    let result = epoll::ctl(
        &fdt,
        sixth,
        EPOLL_CTL_ADD,
        inner,
        Some(EpollEvent {
            events: PollEvents::POLLIN.raw() as u32,
            data: 6,
        }),
    );

    assert_eq!(result, Err(Errno::EINVAL));
}

/// 环检测优先于深度限制，形成闭环时必须返回 ELOOP。
#[ktest]
fn epoll_cycle_reports_eloop() {
    let fdt = test_fdtable();
    let cred = root_cred();
    let (read_end, _write_end) = pipe::new_pipe(Arc::clone(&cred), false).unwrap();
    let pipe_fd = fdt.alloc_fd(read_end, FdFlags::default()).unwrap();
    let origin = epoll::create(&fdt, Arc::clone(&cred), false).unwrap();
    epoll::ctl(
        &fdt,
        origin,
        EPOLL_CTL_ADD,
        pipe_fd,
        Some(EpollEvent {
            events: PollEvents::POLLIN.raw() as u32,
            data: 0,
        }),
    )
    .unwrap();

    let mut outermost = origin;
    for depth in 1..5 {
        let outer = epoll::create(&fdt, Arc::clone(&cred), false).unwrap();
        epoll::ctl(
            &fdt,
            outer,
            EPOLL_CTL_ADD,
            outermost,
            Some(EpollEvent {
                events: PollEvents::POLLIN.raw() as u32,
                data: depth,
            }),
        )
        .unwrap();
        outermost = outer;
    }
    epoll::ctl(&fdt, origin, EPOLL_CTL_DEL, pipe_fd, None).unwrap();

    let result = epoll::ctl(
        &fdt,
        origin,
        EPOLL_CTL_ADD,
        outermost,
        Some(EpollEvent {
            events: PollEvents::POLLIN.raw() as u32,
            data: 5,
        }),
    );

    assert_eq!(result, Err(Errno::ELOOP));
}

/// maxevents 较小时，持续就绪的描述符不能饿死后续描述符。
#[ktest]
fn epoll_rotates_level_triggered_ready_files() {
    let fdt = test_fdtable();
    let cred = root_cred();
    let (read_end, write_end) = pipe::new_pipe(Arc::clone(&cred), false).unwrap();
    let read_fd = fdt
        .alloc_fd(Arc::clone(&read_end), FdFlags::default())
        .unwrap();
    let write_fd = fdt
        .alloc_fd(Arc::clone(&write_end), FdFlags::default())
        .unwrap();
    let epfd = epoll::create(&fdt, cred, false).unwrap();

    epoll::ctl(
        &fdt,
        epfd,
        EPOLL_CTL_ADD,
        read_fd,
        Some(EpollEvent {
            events: PollEvents::POLLIN.raw() as u32,
            data: 10,
        }),
    )
    .unwrap();
    epoll::ctl(
        &fdt,
        epfd,
        EPOLL_CTL_ADD,
        write_fd,
        Some(EpollEvent {
            events: PollEvents::POLLOUT.raw() as u32,
            data: 20,
        }),
    )
    .unwrap();
    write_end.write(b"x").unwrap();

    let first = epoll::wait(&fdt, epfd, 1, 0).unwrap();
    let second = epoll::wait(&fdt, epfd, 1, 0).unwrap();

    assert_eq!(first.len(), 1);
    assert_eq!(second.len(), 1);
    assert_ne!(first[0].data, second[0].data);
}

/// 子进程 exec 关闭继承的 CLOEXEC fd 时，父进程仍持有的打开文件描述不能被
/// 误判为全局最后引用，也不能移除共享 epoll 实例中的 watch。
#[ktest]
fn epoll_watch_survives_forked_cloexec_close() {
    let fdt = test_fdtable();
    let cred = root_cred();
    let epfd = epoll::create(&fdt, Arc::clone(&cred), true).unwrap();
    let eventfd = eventfd::create(&fdt, cred, 0, false, true, true).unwrap();
    epoll::ctl(
        &fdt,
        epfd,
        EPOLL_CTL_ADD,
        eventfd,
        Some(EpollEvent {
            events: PollEvents::POLLIN.raw() as u32,
            data: 30,
        }),
    )
    .unwrap();

    let child = fdt.fork();
    child.close_on_exec();

    fdt.get_file(eventfd)
        .unwrap()
        .write(&1u64.to_ne_bytes())
        .unwrap();
    let ready = epoll::wait(&fdt, epfd, 1, 0).unwrap();
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].data, 30);
}

/// fd 进入另一张描述符表后，epoll watch 应在全局最后一个 fd 关闭时移除，
/// 不能依赖最后关闭者是否继承过创建 epoll 时的 FdTable 元数据。
#[ktest]
fn epoll_watch_tracks_last_fd_across_unrelated_tables() {
    let owner = test_fdtable();
    let receiver = test_fdtable();
    let cred = root_cred();
    let epfd = epoll::create(&owner, Arc::clone(&cred), false).unwrap();
    let eventfd = eventfd::create(&owner, cred, 0, false, true, false).unwrap();
    let event_file = owner.get_file(eventfd).unwrap();
    epoll::ctl(
        &owner,
        epfd,
        EPOLL_CTL_ADD,
        eventfd,
        Some(EpollEvent {
            events: PollEvents::POLLIN.raw() as u32,
            data: 31,
        }),
    )
    .unwrap();
    let received_fd = receiver
        .alloc_fd(Arc::clone(&event_file), FdFlags::default())
        .unwrap();

    owner.close_fd(eventfd).unwrap();
    event_file.write(&1u64.to_ne_bytes()).unwrap();
    assert_eq!(epoll::wait(&owner, epfd, 1, 0).unwrap()[0].data, 31);
    let mut counter = [0u8; 8];
    event_file.read(&mut counter).unwrap();

    receiver.close_fd(received_fd).unwrap();
    event_file.write(&1u64.to_ne_bytes()).unwrap();
    assert!(epoll::wait(&owner, epfd, 1, 0).unwrap().is_empty());
}
