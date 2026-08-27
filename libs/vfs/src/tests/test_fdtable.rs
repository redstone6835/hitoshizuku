//! 文件描述符与 FdFlags 测试。
//!
//! 验证标准流编号、Fd raw 转换、FdFlags CLOEXEC 位操作。

extern crate std;

use alloc::sync::Arc;

use crate::cred::Credentials;
use crate::fdtable::{Fd, FdFlags, FdTable};
use crate::pipe;
use ktest::ktest;

fn pipe_files() -> (Arc<crate::file::File>, Arc<crate::file::File>) {
    pipe::new_pipe(Arc::new(Credentials::root()), false).expect("create test pipe")
}

/// 标准流 STDIN=0, STDOUT=1, STDERR=2 与 POSIX 一致。
#[ktest]
fn fd_stdin_stdout_stderr() {
    assert_eq!(Fd::STDIN.as_raw(), 0);
    assert_eq!(Fd::STDOUT.as_raw(), 1);
    assert_eq!(Fd::STDERR.as_raw(), 2);
}

/// from_raw 构造后 as_raw 往返一致。
#[ktest]
fn fd_from_raw_as_raw_roundtrip() {
    for n in [0, 1, 2, 42, 1023] {
        assert_eq!(Fd::from_raw(n).as_raw(), n);
    }
}

/// 默认 FdFlags 的 raw 值为 0。
#[ktest]
fn fdflags_default_raw_zero() {
    assert_eq!(FdFlags::default().raw(), 0);
}

/// CLOEXEC 标志使 has(CLOEXEC) 返回 true。
#[ktest]
fn fdflags_cloexec_has() {
    assert!(FdFlags::CLOEXEC.has(FdFlags::CLOEXEC));
}

/// with 添加后 without 移除，has 返回 false。
#[ktest]
fn fdflags_with_without() {
    let f = FdFlags::default()
        .with(FdFlags::CLOEXEC)
        .without(FdFlags::CLOEXEC);
    assert!(!f.has(FdFlags::CLOEXEC));
}

/// exec 预检查必须看到标准流和额外 fd 的真实 descriptor flags，不能丢失 CLOEXEC。
#[ktest]
fn descriptor_snapshot_preserves_fd_flags_and_file_identity() {
    let table = FdTable::new_default();
    let (read_end, write_end) = pipe_files();
    table
        .install_fd(Fd::STDIN, Arc::clone(&read_end), FdFlags::default())
        .unwrap();
    table
        .install_fd(Fd::STDOUT, Arc::clone(&write_end), FdFlags::CLOEXEC)
        .unwrap();
    table
        .install_fd(Fd::STDERR, Arc::clone(&write_end), FdFlags::default())
        .unwrap();
    table
        .install_fd(Fd::from_raw(7), Arc::clone(&read_end), FdFlags::CLOEXEC)
        .unwrap();

    let snapshot = table.snapshot_descriptors().expect("描述符快照分配应成功");
    let descriptors = snapshot.descriptors();

    assert_eq!(descriptors.len(), 4);
    assert_eq!(
        descriptors
            .iter()
            .map(|descriptor| descriptor.fd().as_raw())
            .collect::<std::vec::Vec<_>>(),
        std::vec![0, 1, 2, 7]
    );
    assert!(!descriptors[0].flags().has(FdFlags::CLOEXEC));
    assert!(descriptors[1].flags().has(FdFlags::CLOEXEC));
    assert!(!descriptors[2].flags().has(FdFlags::CLOEXEC));
    assert!(descriptors[3].flags().has(FdFlags::CLOEXEC));
    assert!(Arc::ptr_eq(descriptors[0].file(), &read_end));
    assert!(Arc::ptr_eq(descriptors[1].file(), &write_end));
}

/// 共享 FdTable 在 prepare 后发生条目修改时，旧快照必须失效。
#[ktest]
fn descriptor_snapshot_detects_shared_table_mutation() {
    let table = Arc::new(FdTable::new_default());
    let (read_end, _) = pipe_files();
    table
        .install_fd(Fd::STDIN, read_end, FdFlags::default())
        .unwrap();
    let snapshot = table.snapshot_descriptors().expect("快照应成功");
    let shared = Arc::clone(&table);

    shared.set_fd_flags(Fd::STDIN, FdFlags::CLOEXEC).unwrap();

    assert!(!table.is_generation_current(snapshot.generation()));
}

/// 安装、dup 和关闭都会改变 exec 可见的描述符集合，因此必须使旧快照失效。
#[ktest]
fn descriptor_generation_tracks_entry_lifecycle() {
    let table = FdTable::new_default();
    let (read_end, _) = pipe_files();

    let before_install = table.snapshot_descriptors().expect("快照应成功");
    table
        .install_fd(Fd::STDIN, read_end, FdFlags::default())
        .unwrap();
    assert!(!table.is_generation_current(before_install.generation()));

    let before_dup = table.snapshot_descriptors().expect("快照应成功");
    let duplicate = table.dup_fd(Fd::STDIN).unwrap();
    assert!(!table.is_generation_current(before_dup.generation()));

    let before_close = table.snapshot_descriptors().expect("快照应成功");
    table.close_fd(duplicate).unwrap();
    assert!(!table.is_generation_current(before_close.generation()));

    let after_close = table.snapshot_descriptors().expect("快照应成功");
    assert!(table.close_fd(duplicate).is_err());
    assert!(table.is_generation_current(after_close.generation()));
}

/// 批量 CLOEXEC 只在 flags 真正变化时推进代际，随后关闭条目会再次推进。
#[ktest]
fn descriptor_generation_tracks_bulk_exec_mutations() {
    let table = FdTable::new_default();
    let (read_end, write_end) = pipe_files();
    table
        .install_fd(Fd::STDIN, read_end, FdFlags::default())
        .unwrap();
    table
        .install_fd(Fd::STDOUT, write_end, FdFlags::default())
        .unwrap();

    let before_cloexec = table.snapshot_descriptors().expect("快照应成功");
    table.close_range(0, 1, true);
    assert!(!table.is_generation_current(before_cloexec.generation()));

    let after_cloexec = table.snapshot_descriptors().expect("快照应成功");
    table.close_range(0, 1, true);
    assert!(table.is_generation_current(after_cloexec.generation()));

    table.close_on_exec(0);
    assert!(!table.is_generation_current(after_cloexec.generation()));
    assert!(
        table
            .snapshot_descriptors()
            .expect("快照应成功")
            .descriptors()
            .is_empty()
    );
}

/// exec 副本会继承 fdtable 的软硬限制，因此限制变化也必须使旧快照失效。
#[ktest]
fn descriptor_generation_tracks_limit_changes() {
    let table = FdTable::new_default();
    let before_change = table.snapshot_descriptors().expect("快照应成功");

    table.set_limits(128, 256).unwrap();

    assert!(!table.is_generation_current(before_change.generation()));
    let after_change = table.snapshot_descriptors().expect("快照应成功");
    table.set_limits(128, 256).unwrap();
    assert!(table.is_generation_current(after_change.generation()));
}

/// exec 通过代际重验后必须把表锁持有到资源发布，避免共享方插入最后一刻修改。
#[ktest]
fn generation_lease_blocks_mutation_until_commit_releases_it() {
    let table = Arc::new(FdTable::new_default());
    let (read_end, _) = pipe_files();
    table
        .install_fd(Fd::STDIN, read_end, FdFlags::default())
        .unwrap();
    let generation = table
        .snapshot_descriptors()
        .expect("快照应成功")
        .generation();
    let lease = table
        .lock_generation(generation)
        .expect("当前代际应能取得发布租约");
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (changed_tx, changed_rx) = std::sync::mpsc::channel();
    let shared = Arc::clone(&table);
    let changer = std::thread::spawn(move || {
        started_tx.send(()).unwrap();
        shared.set_fd_flags(Fd::STDIN, FdFlags::CLOEXEC).unwrap();
        changed_tx.send(()).unwrap();
    });

    started_rx.recv().unwrap();
    assert!(
        changed_rx
            .recv_timeout(std::time::Duration::from_millis(20))
            .is_err(),
        "发布租约存活期间不得修改 fdtable"
    );
    drop(lease);
    changed_rx
        .recv_timeout(std::time::Duration::from_secs(1))
        .expect("释放发布租约后修改应完成");
    changer.join().unwrap();
}

/// exec 副本只继承非 CLOEXEC 条目，且不得提前改变原表。
#[ktest]
fn fork_for_exec_filters_descriptors_without_mutating_source() {
    let table = FdTable::new_default();
    let (read_end, write_end) = pipe_files();
    table
        .install_fd(Fd::STDIN, Arc::clone(&read_end), FdFlags::default())
        .unwrap();
    table
        .install_fd(Fd::STDOUT, Arc::clone(&write_end), FdFlags::CLOEXEC)
        .unwrap();
    let source = table.snapshot_descriptors().expect("快照应成功");

    let prepared = table.fork_for_exec().expect("exec fdtable 预构造应成功");

    assert!(table.is_generation_current(source.generation()));
    assert_eq!(
        table
            .snapshot_descriptors()
            .expect("快照应成功")
            .descriptors()
            .len(),
        2
    );
    let inherited = prepared.snapshot_descriptors().expect("快照应成功");
    assert_eq!(inherited.descriptors().len(), 1);
    assert_eq!(inherited.descriptors()[0].fd(), Fd::STDIN);
    assert!(Arc::ptr_eq(inherited.descriptors()[0].file(), &read_end));
    assert!(!inherited.descriptors()[0].flags().has(FdFlags::CLOEXEC));
}

/// 降低 hard limit 不会截断已有高编号 fd，exec 副本也必须保持位图一致。
#[ktest]
fn fork_for_exec_preserves_high_fd_after_hard_limit_lowering() {
    let table = FdTable::new_default();
    let (read_end, _) = pipe_files();
    let high = Fd::from_raw(100);
    table
        .install_fd(high, Arc::clone(&read_end), FdFlags::default())
        .unwrap();

    table.set_limits(50, 50).unwrap();
    let prepared = table
        .fork_for_exec()
        .expect("降低 hard limit 后仍应能构造 exec 副本");

    assert!(table.get_file(high).is_some());
    assert!(prepared.get_file(high).is_some());
    assert!(
        prepared
            .install_fd(Fd::from_raw(50), read_end, FdFlags::default())
            .is_err()
    );
}
