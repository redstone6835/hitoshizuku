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
use crate::{memfd, pipe};

fn test_fdtable() -> FdTable {
    FdTable::new(&VfsLimits::default())
}

fn root_cred() -> Arc<Credentials> {
    Arc::new(Credentials::root())
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
