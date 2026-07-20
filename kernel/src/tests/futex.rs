use super::*;
use ktest::ktest;
use mm::VmFlags;

const PROBE: usize = 0x0000_3800_0000_0000;

fn waiter(task: &Arc<Task>) -> FutexWaiter {
    FutexWaiter {
        task: Arc::downgrade(task),
        bitset: FUTEX_BITSET_MATCH_ANY,
        waitv_index: None,
        state: Arc::new(FutexWaitState::new()),
    }
}

#[ktest]
fn futex_wait_requeue_and_user_rmw_are_atomic() {
    let task = sched::current_task();
    let saved_vm = task.ext_remove(sched::TASKEXT_VM_SPACE);
    let vm = Arc::new(VmSpace::new());
    task.ext_install(sched::TASKEXT_VM_SPACE, vm.clone());
    let page_size = general::mm::page_size();
    vm.map_anon(
        PROBE..PROBE + page_size,
        VmFlags::EMPTY
            .with(VmFlags::READ)
            .with(VmFlags::WRITE)
            .with(VmFlags::USER),
    )
    .expect("futex 测试映射失败");

    let src = vm.futex_key_for(PROBE, true).expect("源 futex key 失败");
    let dst = vm
        .futex_key_for(PROBE + 4, true)
        .expect("目标 futex key 失败");
    write_user_u32(PROBE, 7).expect("初始化 futex word 失败");
    vm.prefault_user_u32(PROBE, true)
        .expect("写入 futex word 预缺页失败");
    assert_eq!(vm.compare_exchange_user_u32_nofault(PROBE, 7, 9), Ok(7));
    assert_eq!(vm.read_user_u32_nofault(PROBE), Ok(9));
    assert_eq!(vm.compare_exchange_user_u32_nofault(PROBE, 7, 11), Ok(9));
    assert_eq!(
        futex_atomic_update_user(&vm, PROBE + 4, |old| { Ok(old.wrapping_add(5)) }),
        Ok(0)
    );
    assert_eq!(vm.read_user_u32_nofault(PROBE + 4), Ok(5));
    write_user_u32(PROBE, 7).expect("恢复 futex word 失败");

    // 预检查完成后用户值发生变化时，普通 WAIT 不得留下 waiter。
    vm.prefault_user_u32(PROBE, false)
        .expect("读取 futex word 预缺页失败");
    write_user_u32(PROBE, 8).expect("修改等待值失败");
    let pending = futex_enqueue_waiter_if_equal(&vm, src, PROBE, 7, waiter(&task));
    assert_eq!(pending, Err(Errno::EAGAIN));
    assert!(!FUTEX_TABLE.lock().contains_key(&src));
    write_user_u32(PROBE, 7).expect("恢复等待值失败");

    {
        let mut table = FUTEX_TABLE.lock();
        table.remove(&src);
        table.remove(&dst);
        table.insert(
            src,
            FutexBucket {
                waiters: alloc::vec![waiter(&task), waiter(&task), waiter(&task)],
            },
        );
    }

    let mismatch = futex_cmp_requeue_key(&vm, PROBE, 8, src, dst, 0, 2, FUTEX_BITSET_MATCH_ANY);
    assert_eq!(mismatch, Err(Errno::EAGAIN));
    {
        let table = FUTEX_TABLE.lock();
        assert_eq!(table.get(&src).map(|bucket| bucket.waiters.len()), Some(3));
        assert!(!table.contains_key(&dst));
    }

    // 精确模拟旧 TOCTOU：预检查读到 7，进入表锁前用户把值改成 9。
    assert_eq!(read_user_u32(PROBE), Ok(7));
    write_user_u32(PROBE, 9).expect("修改 futex word 失败");
    let raced =
        futex_cmp_requeue_after_prefault(&vm, PROBE, 7, src, dst, 0, 2, FUTEX_BITSET_MATCH_ANY);
    assert_eq!(raced, Err(Errno::EAGAIN));
    {
        let table = FUTEX_TABLE.lock();
        assert_eq!(table.get(&src).map(|bucket| bucket.waiters.len()), Some(3));
        assert!(!table.contains_key(&dst));
    }

    write_user_u32(PROBE, 7).expect("恢复 futex word 失败");
    assert_eq!(
        futex_cmp_requeue_key(&vm, PROBE, 7, src, dst, 0, 2, FUTEX_BITSET_MATCH_ANY),
        Ok(2)
    );
    {
        let table = FUTEX_TABLE.lock();
        assert_eq!(table.get(&src).map(|bucket| bucket.waiters.len()), Some(1));
        assert_eq!(table.get(&dst).map(|bucket| bucket.waiters.len()), Some(2));
    }
    assert_eq!(futex_requeue_key(src, dst, 0, 1, FUTEX_BITSET_MATCH_ANY), 1);
    {
        let table = FUTEX_TABLE.lock();
        assert!(!table.contains_key(&src));
        assert_eq!(table.get(&dst).map(|bucket| bucket.waiters.len()), Some(3));
    }
    {
        let mut table = FUTEX_TABLE.lock();
        table.remove(&src);
        table.remove(&dst);
    }

    let waitv_entries = alloc::vec![
        FutexWaitvEntry {
            index: 0,
            uaddr: PROBE,
            expected: 7,
            key: src,
            wait_state: Arc::new(FutexWaitState::new()),
        },
        FutexWaitvEntry {
            index: 1,
            uaddr: PROBE + 4,
            expected: 6,
            key: dst,
            wait_state: Arc::new(FutexWaitState::new()),
        },
    ];
    write_user_u32(PROBE + 4, 5).expect("设置 WAITV 值失败");
    for entry in &waitv_entries {
        vm.prefault_user_u32(entry.uaddr, false)
            .expect("WAITV 预缺页失败");
    }
    assert_eq!(
        futex_waitv_enqueue_if_equal(&vm, &waitv_entries, &task),
        Err(Errno::EAGAIN)
    );
    {
        let table = FUTEX_TABLE.lock();
        assert!(!table.contains_key(&src));
        assert!(!table.contains_key(&dst));
    }
    write_user_u32(PROBE + 4, 6).expect("恢复 WAITV 值失败");
    futex_waitv_enqueue_if_equal(&vm, &waitv_entries, &task).expect("WAITV 登记失败");
    {
        let table = FUTEX_TABLE.lock();
        assert_eq!(table.get(&src).map(|bucket| bucket.waiters.len()), Some(1));
        assert_eq!(table.get(&dst).map(|bucket| bucket.waiters.len()), Some(1));
    }
    futex_waitv_remove_all(&waitv_entries, &task);
    {
        let table = FUTEX_TABLE.lock();
        assert!(!table.contains_key(&src));
        assert!(!table.contains_key(&dst));
    }

    let mut local = BTreeMap::new();
    local.insert(
        src,
        FutexBucket {
            waiters: alloc::vec![waiter(&task), waiter(&task), waiter(&task)],
        },
    );
    let (wake, requeued) = futex_requeue_locked(&mut local, src, src, 0, 2, FUTEX_BITSET_MATCH_ANY);
    assert!(wake.is_empty());
    assert_eq!(requeued, 2);
    assert_eq!(local.get(&src).map(|bucket| bucket.waiters.len()), Some(3));

    let (wake, requeued) = futex_requeue_locked(&mut local, dst, src, 1, 1, FUTEX_BITSET_MATCH_ANY);
    assert!(wake.is_empty());
    assert_eq!(requeued, 0);

    local.insert(
        dst,
        FutexBucket {
            waiters: alloc::vec![
                FutexWaiter {
                    task: Weak::new(),
                    bitset: FUTEX_BITSET_MATCH_ANY,
                    waitv_index: None,
                    state: Arc::new(FutexWaitState::new()),
                },
                waiter(&task),
            ],
        },
    );
    let (wake, requeued) = futex_requeue_locked(&mut local, dst, src, 1, 0, FUTEX_BITSET_MATCH_ANY);
    assert_eq!(wake.len(), 1);
    assert_eq!(requeued, 0);
    assert!(!local.contains_key(&dst));

    {
        let mut table = FUTEX_TABLE.lock();
        table.remove(&src);
        table.remove(&dst);
    }
    vm.unmap(PROBE..PROBE + page_size)
        .expect("清理 futex 测试映射失败");
    task.ext_remove(sched::TASKEXT_VM_SPACE);
    if let Some(saved_vm) = saved_vm {
        task.ext_install(sched::TASKEXT_VM_SPACE, saved_vm);
    }
}
