//! System V `SEM_UNDO` 进程级撤销表。
//!
//! Linux 语义（`ipc/sem.c` `struct sem_undo_list`）：每个进程维护一张按
//! semaphore set 聚合的调整表。`semop` 中带 `SEM_UNDO` 标志的操作成功提交后，
//! 把 `-sem_op` 累计进本进程对 (set, semnum) 的调整值；进程退出时按 set
//! 逐项做一次原子 `semop`（聚合后的 delta 反向应用），保证进程被杀死后
//! 它占用的资源计数仍然归还。
//!
//! 与 Linux 一致：
//! - 同一进程的多个线程共享同一张表（`clone` 带 `CLONE_SYSVSEM` 时与父进程
//!   共享；不带时子进程得到一张**空**表，不继承父进程的撤销项）；
//! - `semctl(SETVAL/SETALL)` 与 `IPC_RMID` 会使对应 set 的撤销项失效；
//! - `execve` 保留撤销表；
//! - 撤销项按 set 对象（`SemId`）绑定，而不是按 key，避免 id 复用错配。

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;

use errno::Errno;
use sched::Task;
use spin::Mutex;
use vfs::cred::Credentials;

use super::sem::{SEM_UNDO, SemId, SemManager, SemOperation};

/// 进程级 `SEM_UNDO` 表。`inner` 映射 `SemId → per-sem 累计调整值`。
pub struct SemUndoTable {
    inner: Mutex<BTreeMap<SemId, Vec<i64>>>,
}

impl SemUndoTable {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.inner.lock().is_empty()
    }

    /// 整批 `semop` 成功提交后调用：把带 `SEM_UNDO` 标志的操作累计为撤销值。
    ///
    /// 调用方必须保证 `operations` 就是刚被原子提交的那一批。
    pub fn record(&self, id: SemId, operations: &[SemOperation]) {
        let mut inner = self.inner.lock();
        let entry = inner.entry(id).or_default();
        for operation in operations {
            if operation.sem_flg & SEM_UNDO == 0 {
                continue;
            }
            let index = operation.sem_num as usize;
            if entry.len() <= index {
                entry.resize(index + 1, 0);
            }
            // 撤销值 = -sem_op：+op 记 -1（退出时归还），-op 记 +1（退出时释放）。
            entry[index] = entry[index].saturating_add(-i64::from(operation.sem_op));
        }
    }

    /// `SETVAL`/`SETALL`/`IPC_RMID` 后调用：使该 set 的全部撤销项失效。
    pub fn clear(&self, id: SemId) {
        self.inner.lock().remove(&id);
    }

    /// 进程退出时应用全部撤销项（Linux `exit_sem`）。
    ///
    /// 按 set 聚合后对每个集合执行一次原子 `semop`；集合已被删除（`EIDRM`）
    /// 的撤销项静默跳过。应用过程可能因临时阻塞而睡眠（Linux `exit_sem` 同样
    /// 允许睡眠），不会被信号打断；`ERANGE` 等无法应用的调整按尽力归还处理，
    /// 不作为退出失败。
    pub fn apply_on_exit(
        &self,
        manager: &SemManager,
        cred: &Credentials,
        pid: i32,
        now_sec: i64,
        task: &Arc<Task>,
    ) {
        // 先整体取出并清空，避免应用过程中重入（例如等待路径再次访问本表）。
        let entries: Vec<(SemId, Vec<i64>)> = {
            let mut inner = self.inner.lock();
            let entries = inner.iter().map(|(id, v)| (*id, v.clone())).collect();
            inner.clear();
            entries
        };
        for (id, adjustments) in entries {
            let mut operations = Vec::with_capacity(adjustments.len());
            let mut overflow = false;
            for (sem_num, delta) in adjustments.iter().enumerate() {
                let delta = *delta;
                if delta == 0 {
                    continue;
                }
                // 撤销值 = -原操作；应用时执行与撤销值等价的 semop：
                // 原 +1 记 delta=-1，退出时执行 -1 使值还原。调整值超出
                // i16 范围时 Linux 对 undo 应用同样返回 ERANGE 并放弃。
                let Ok(sem_op) = i16::try_from(delta) else {
                    overflow = true;
                    break;
                };
                operations.push(SemOperation {
                    sem_num: sem_num as u16,
                    sem_op,
                    sem_flg: 0,
                });
            }
            if overflow || operations.is_empty() {
                continue;
            }
            let Ok(set) = manager.set_for_operation(id) else {
                continue;
            };
            loop {
                match set.try_apply(&operations, cred, pid, now_sec) {
                    Ok(super::sem::SemOpAttempt::Applied) => break,
                    Ok(super::sem::SemOpAttempt::WouldBlock { .. }) => {
                        let entry = set
                            .waiters()
                            .prepare_to_wait(task, sched::TaskState::Sleeping);
                        // 登记后 recheck，与 syscall 层协议一致。
                        match set.try_apply(&operations, cred, pid, now_sec) {
                            Ok(super::sem::SemOpAttempt::Applied) => {
                                set.waiters().finish_wait(&entry);
                                break;
                            }
                            _ => {}
                        }
                        sched::schedule_once(sched::now_ns_direct());
                        set.waiters().finish_wait(&entry);
                    }
                    Err(_) => break, // EIDRM/EAGAIN/ERANGE：尽力归还，失败忽略
                }
            }
            set.waiters().wake_all();
        }
    }
}

impl Default for SemUndoTable {
    fn default() -> Self {
        Self::new()
    }
}
