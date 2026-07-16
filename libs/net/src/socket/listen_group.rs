use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use spin::Mutex;

use crate::{ListenGroupId, ShardId};

use super::SocketFacade;

const MAX_ACCEPT_BACKLOG: usize = 1024;
const MAX_SYN_BACKLOG: usize = 2048;

struct ListenShard {
    accepted: Mutex<VecDeque<Arc<SocketFacade>>>,
}

/// 一个用户可见 listener 在所有协议分片上的共享容量与 accept 目录。
pub struct ListenGroup {
    id: ListenGroupId,
    parent: Weak<SocketFacade>,
    shards: Box<[ListenShard]>,
    cpu_hints: Box<[u16]>,
    ready_shards: AtomicU64,
    accept_cursor: AtomicUsize,
    accept_limit: AtomicUsize,
    syn_limit: AtomicUsize,
    accept_count: AtomicUsize,
    syn_count: AtomicUsize,
    closing: AtomicBool,
}

impl ListenGroup {
    pub fn new(
        id: ListenGroupId,
        parent: &Arc<SocketFacade>,
        shard_count: usize,
        backlog: u32,
    ) -> Arc<Self> {
        let cpu_hints = (0..shard_count).collect::<Vec<_>>();
        Self::new_with_cpu_hints(id, parent, &cpu_hints, backlog)
    }

    pub fn new_with_cpu_hints(
        id: ListenGroupId,
        parent: &Arc<SocketFacade>,
        cpu_hints: &[usize],
        backlog: u32,
    ) -> Arc<Self> {
        assert!(
            (1..=64).contains(&cpu_hints.len()),
            "ListenGroup shard 数量非法"
        );
        let mut shards = Vec::with_capacity(cpu_hints.len());
        for _ in cpu_hints {
            shards.push(ListenShard {
                accepted: Mutex::new(VecDeque::new()),
            });
        }
        let accept_limit = effective_accept_backlog(backlog);
        Arc::new(Self {
            id,
            parent: Arc::downgrade(parent),
            shards: shards.into_boxed_slice(),
            cpu_hints: cpu_hints
                .iter()
                .map(|cpu| u16::try_from(*cpu).expect("CPU 编号超出 ListenGroup hint 范围"))
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            ready_shards: AtomicU64::new(0),
            accept_cursor: AtomicUsize::new(0),
            accept_limit: AtomicUsize::new(accept_limit),
            syn_limit: AtomicUsize::new(effective_syn_backlog(accept_limit)),
            accept_count: AtomicUsize::new(0),
            syn_count: AtomicUsize::new(0),
            closing: AtomicBool::new(false),
        })
    }

    pub const fn id(&self) -> ListenGroupId {
        self.id
    }

    pub fn parent(&self) -> Option<Arc<SocketFacade>> {
        self.parent.upgrade()
    }

    pub fn shard_count(&self) -> usize {
        self.shards.len()
    }

    pub fn accept_limit(&self) -> usize {
        self.accept_limit.load(Ordering::Acquire)
    }

    pub fn syn_limit(&self) -> usize {
        self.syn_limit.load(Ordering::Acquire)
    }

    pub fn accept_count(&self) -> usize {
        self.accept_count.load(Ordering::Acquire)
    }

    pub fn syn_count(&self) -> usize {
        self.syn_count.load(Ordering::Acquire)
    }

    pub fn is_closing(&self) -> bool {
        self.closing.load(Ordering::Acquire)
    }

    pub fn update_backlog(&self, backlog: u32) {
        let accept_limit = effective_accept_backlog(backlog);
        self.accept_limit.store(accept_limit, Ordering::Release);
        self.syn_limit
            .store(effective_syn_backlog(accept_limit), Ordering::Release);
    }

    pub fn reserve_syn(&self) -> bool {
        reserve_bounded(&self.syn_count, &self.syn_limit, &self.closing)
    }

    pub fn release_syn(&self) {
        let previous = self.syn_count.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous != 0, "ListenGroup SYN 计数下溢");
    }

    /// 将一个半连接原子转换为 accept backlog 项。
    pub fn publish_established(
        &self,
        shard: ShardId,
        child: Arc<SocketFacade>,
    ) -> Result<(), Arc<SocketFacade>> {
        self.release_syn();
        if !reserve_bounded(&self.accept_count, &self.accept_limit, &self.closing) {
            return Err(child);
        }
        let index = usize::from(shard.0);
        let Some(queue) = self.shards.get(index) else {
            self.accept_count.fetch_sub(1, Ordering::AcqRel);
            return Err(child);
        };
        {
            let mut accepted = queue.accepted.lock();
            if self.closing.load(Ordering::Acquire) {
                self.accept_count.fetch_sub(1, Ordering::AcqRel);
                return Err(child);
            }
            accepted.push_back(child);
            self.ready_shards.fetch_or(1u64 << index, Ordering::Release);
        }
        if let Some(parent) = self.parent() {
            parent.notify_accept_ready(usize::from(self.cpu_hints[index]));
        }
        Ok(())
    }

    pub fn accept(&self) -> Option<Arc<SocketFacade>> {
        let count = self.shards.len();
        let start = self.accept_cursor.fetch_add(1, Ordering::Relaxed) % count;
        let ready = self.ready_shards.load(Ordering::Acquire);
        for offset in 0..count {
            let index = (start + offset) % count;
            if ready & (1u64 << index) == 0 {
                continue;
            }
            let mut accepted = self.shards[index].accepted.lock();
            let child = accepted.pop_front();
            if accepted.is_empty() {
                self.ready_shards
                    .fetch_and(!(1u64 << index), Ordering::AcqRel);
            }
            drop(accepted);
            if let Some(child) = child {
                self.accept_count.fetch_sub(1, Ordering::AcqRel);
                return Some(child);
            }
        }
        None
    }

    pub fn has_ready(&self) -> bool {
        self.ready_shards.load(Ordering::Acquire) != 0
    }

    /// 关闭后禁止新增容量，并返回所有尚未被 accept 的连接。
    pub fn close(&self) -> Vec<Arc<SocketFacade>> {
        if self.closing.swap(true, Ordering::AcqRel) {
            return Vec::new();
        }
        let mut children = Vec::new();
        for shard in &self.shards {
            let mut accepted = shard.accepted.lock();
            children.extend(accepted.drain(..));
        }
        self.ready_shards.store(0, Ordering::Release);
        self.accept_count.store(0, Ordering::Release);
        children
    }
}

fn effective_accept_backlog(backlog: u32) -> usize {
    (backlog as usize).clamp(1, MAX_ACCEPT_BACKLOG)
}

fn effective_syn_backlog(accept_limit: usize) -> usize {
    accept_limit.saturating_mul(2).min(MAX_SYN_BACKLOG)
}

fn reserve_bounded(count: &AtomicUsize, limit: &AtomicUsize, closing: &AtomicBool) -> bool {
    if closing.load(Ordering::Acquire) {
        return false;
    }
    let mut current = count.load(Ordering::Acquire);
    loop {
        if current >= limit.load(Ordering::Acquire) || closing.load(Ordering::Acquire) {
            return false;
        }
        match count.compare_exchange_weak(current, current + 1, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => {
                if closing.load(Ordering::Acquire) {
                    count.fetch_sub(1, Ordering::AcqRel);
                    return false;
                }
                return true;
            }
            Err(observed) => current = observed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AddressFamily, SocketId, SocketKind};
    use std::thread;

    fn facade(counter: u64) -> Arc<SocketFacade> {
        Arc::new(SocketFacade::new(
            SocketId {
                boot_nonce: 1,
                counter,
            },
            AddressFamily::Ipv4,
            SocketKind::Stream,
        ))
    }

    #[test]
    fn global_backlog_is_shared_by_all_shards() {
        let parent = facade(1);
        let group = ListenGroup::new(ListenGroupId(1), &parent, 2, 1);
        assert_eq!(group.accept_limit(), 1);
        assert_eq!(group.syn_limit(), 2);
        assert!(group.reserve_syn());
        assert!(group.reserve_syn());
        assert!(!group.reserve_syn());

        let first = facade(2);
        let second = facade(3);
        assert!(
            group
                .publish_established(ShardId(0), Arc::clone(&first))
                .is_ok()
        );
        assert!(group.publish_established(ShardId(1), second).is_err());
        assert_eq!(group.syn_count(), 0);
        assert_eq!(group.accept_count(), 1);
        assert!(Arc::ptr_eq(&group.accept().unwrap(), &first));
        assert_eq!(group.accept_count(), 0);
    }

    #[test]
    fn ready_bitmap_visits_only_nonempty_shards() {
        let parent = facade(4);
        let group = ListenGroup::new(ListenGroupId(2), &parent, 2, 4);
        let first = facade(5);
        let second = facade(6);
        assert!(group.reserve_syn());
        assert!(group.reserve_syn());
        assert!(
            group
                .publish_established(ShardId(0), Arc::clone(&first))
                .is_ok()
        );
        assert!(
            group
                .publish_established(ShardId(1), Arc::clone(&second))
                .is_ok()
        );

        let accepted = [group.accept().unwrap(), group.accept().unwrap()];
        assert!(accepted.iter().any(|child| Arc::ptr_eq(child, &first)));
        assert!(accepted.iter().any(|child| Arc::ptr_eq(child, &second)));
        assert!(!group.has_ready());
        assert!(group.accept().is_none());
    }

    #[test]
    fn close_drains_accept_queues_and_rejects_new_syn() {
        let parent = facade(7);
        let group = ListenGroup::new(ListenGroupId(3), &parent, 2, 2);
        assert!(group.reserve_syn());
        assert!(group.publish_established(ShardId(1), facade(8)).is_ok());
        let children = group.close();
        assert_eq!(children.len(), 1);
        assert!(!group.has_ready());
        assert!(!group.reserve_syn());
    }

    #[test]
    fn concurrent_accept_pops_each_child_once() {
        let parent = facade(9);
        let group = ListenGroup::new(ListenGroupId(4), &parent, 2, 4);
        let first = facade(10);
        let second = facade(11);
        assert!(group.reserve_syn());
        assert!(group.reserve_syn());
        assert!(
            group
                .publish_established(ShardId(0), Arc::clone(&first))
                .is_ok()
        );
        assert!(
            group
                .publish_established(ShardId(1), Arc::clone(&second))
                .is_ok()
        );
        let left_group = Arc::clone(&group);
        let right_group = Arc::clone(&group);
        let left = thread::spawn(move || left_group.accept().unwrap());
        let right = thread::spawn(move || right_group.accept().unwrap());
        let accepted = [left.join().unwrap(), right.join().unwrap()];
        assert!(accepted.iter().any(|child| Arc::ptr_eq(child, &first)));
        assert!(accepted.iter().any(|child| Arc::ptr_eq(child, &second)));
        assert_eq!(group.accept_count(), 0);
        assert!(!group.has_ready());
    }
}
