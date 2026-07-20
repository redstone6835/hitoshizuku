//! pipe 容量查询、调整和环形数据保留测试。

extern crate std;

use alloc::sync::Arc;
use std::vec;

use errno::Errno;
use ktest::ktest;

use crate::cred::{Credentials, Gid, Uid};
use crate::error::VfsError;
use crate::file::PollEvents;
use crate::pipe::{self, F_GETPIPE_SZ, F_SETPIPE_SZ};

fn root_cred() -> Arc<Credentials> {
    Arc::new(Credentials::root())
}

fn unprivileged_cred() -> Credentials {
    Credentials::unprivileged(Uid(1000), Gid(1000))
}

/// 两个端点共享同一个容量，设置为零时按 Linux 规则收缩到一页。
#[ktest]
fn pipe_capacity_is_shared_and_zero_means_one_page() {
    let cred = root_cred();
    let (read_end, write_end) = pipe::new_pipe(Arc::clone(&cred), true).unwrap();

    assert_eq!(write_end.fcntl(F_SETPIPE_SZ, 0, cred.as_ref()), Ok(4096));
    assert_eq!(read_end.fcntl(F_GETPIPE_SZ, 0, cred.as_ref()), Ok(4096));
}

/// 请求值向上取整到页大小的二次幂。
#[ktest]
fn pipe_capacity_rounds_up() {
    let cred = root_cred();
    let (_read_end, write_end) = pipe::new_pipe(Arc::clone(&cred), true).unwrap();

    assert_eq!(write_end.fcntl(F_SETPIPE_SZ, 5000, cred.as_ref()), Ok(8192));
    assert_eq!(write_end.fcntl(F_GETPIPE_SZ, 0, cred.as_ref()), Ok(8192));
}

/// 容量不能缩到当前未读数据量以下。
#[ktest]
fn pipe_rejects_shrink_below_occupancy() {
    let cred = root_cred();
    let (_read_end, write_end) = pipe::new_pipe(Arc::clone(&cred), true).unwrap();
    write_end.write(&vec![0x5a; 8192]).unwrap();

    assert_eq!(
        write_end.fcntl(F_SETPIPE_SZ, 4096, cred.as_ref()),
        Err(Errno::EBUSY)
    );
}

/// 非特权调用者不能突破 `/proc/sys/fs/pipe-max-size`。
#[ktest]
fn pipe_rejects_unprivileged_capacity_over_limit() {
    let cred = root_cred();
    let (_read_end, write_end) = pipe::new_pipe(Arc::clone(&cred), true).unwrap();
    let requested = pipe::pipe_max_size() + 1;

    assert_eq!(
        write_end.fcntl(F_SETPIPE_SZ, requested, &unprivileged_cred()),
        Err(Errno::EPERM)
    );
}

/// 缩小后的满管道必须返回 WouldBlock，不能继续写入隐藏容量。
#[ktest]
fn pipe_shrink_changes_actual_write_capacity() {
    let cred = root_cred();
    let (_read_end, write_end) = pipe::new_pipe(Arc::clone(&cred), true).unwrap();
    write_end.fcntl(F_SETPIPE_SZ, 4096, cred.as_ref()).unwrap();
    write_end.write(&vec![0x33; 4096]).unwrap();

    assert_eq!(write_end.write(b"x"), Err(VfsError::WouldBlock));
}

/// 调整发生在环形缓冲区回绕后时，所有未读字节仍保持原顺序。
#[ktest]
fn pipe_resize_preserves_wrapped_data() {
    let cred = root_cred();
    let (read_end, write_end) = pipe::new_pipe(Arc::clone(&cred), true).unwrap();
    write_end.fcntl(F_SETPIPE_SZ, 4096, cred.as_ref()).unwrap();

    let first: std::vec::Vec<u8> = (0..3000).map(|index| (index % 251) as u8).collect();
    let second: std::vec::Vec<u8> = (0..2000).map(|index| (index % 239) as u8).collect();
    write_end.write(&first).unwrap();
    let mut consumed = vec![0u8; 2000];
    read_end.read(&mut consumed).unwrap();
    write_end.write(&second).unwrap();

    write_end.fcntl(F_SETPIPE_SZ, 8192, cred.as_ref()).unwrap();
    let mut remaining = vec![0u8; 3000];
    assert_eq!(read_end.read(&mut remaining), Ok(3000));

    let mut expected = first[2000..].to_vec();
    expected.extend_from_slice(&second);
    assert_eq!(remaining, expected);
}

/// pipe 只有至少腾出 PIPE_BUF 字节后才报告 POLLOUT。
#[ktest]
fn pipe_pollout_requires_pipe_buf_space() {
    let cred = root_cred();
    let (read_end, write_end) = pipe::new_pipe(Arc::clone(&cred), true).unwrap();
    write_end.fcntl(F_SETPIPE_SZ, 4096, cred.as_ref()).unwrap();
    write_end.write(&vec![0x44; 2048]).unwrap();

    assert!(write_end.poll(PollEvents::POLLOUT).is_empty());
    let mut consumed = vec![0u8; 2048];
    read_end.read(&mut consumed).unwrap();
    assert!(write_end.poll(PollEvents::POLLOUT).has(PollEvents::POLLOUT));
}

/// 不超过 PIPE_BUF 的写入不能在空间不足时产生部分写。
#[ktest]
fn pipe_buf_write_is_atomic() {
    let cred = root_cred();
    let (read_end, write_end) = pipe::new_pipe(Arc::clone(&cred), true).unwrap();
    write_end.fcntl(F_SETPIPE_SZ, 4096, cred.as_ref()).unwrap();
    write_end.write(&vec![0x55; 2048]).unwrap();

    assert_eq!(
        write_end.write(&vec![0x66; 3000]),
        Err(VfsError::WouldBlock)
    );
    let mut content = vec![0u8; 4096];
    assert_eq!(read_end.read(&mut content), Ok(2048));
    assert!(content[..2048].iter().all(|byte| *byte == 0x55));
}

/// 匿名管道不是可同步文件，fsync/fdatasync 必须按 Linux 语义返回 EINVAL。
#[ktest]
fn pipe_rejects_file_sync() {
    let cred = root_cred();
    let (read_end, write_end) = pipe::new_pipe(cred, true).unwrap();

    assert_eq!(read_end.sync(), Err(VfsError::InvalidArgument));
    assert_eq!(write_end.datasync(), Err(VfsError::InvalidArgument));
}

/// pipefs 必须提供有效的 statfs 结果，供 fstatfs(pipefd) 查询。
#[ktest]
fn pipe_filesystem_reports_statfs() {
    let (read_end, _) = pipe::new_pipe(root_cred(), true).unwrap();
    let sb = read_end.inode().superblock().unwrap();
    let stat = sb.statfs().unwrap();

    assert_eq!(stat.fs_type, 0x5049_5045);
    assert_eq!(stat.block_size, 4096);
    assert_eq!(stat.name_max, 255);
}
