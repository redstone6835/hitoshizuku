//! 无每项堆分配的有界多生产者单消费者队列。

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicUsize, Ordering};

struct Slot<T> {
    sequence: AtomicUsize,
    value: UnsafeCell<MaybeUninit<T>>,
}

/// 容量在创建时固定，满队列拒绝最新元素。
pub struct BoundedMpsc<T> {
    slots: Box<[Slot<T>]>,
    mask: usize,
    enqueue: AtomicUsize,
    dequeue: AtomicUsize,
}

impl<T> BoundedMpsc<T> {
    pub fn new(capacity: usize) -> Self {
        assert!(capacity.is_power_of_two(), "有界队列容量必须是 2 的幂");
        assert!(capacity >= 2, "有界队列容量过小");
        assert!(capacity <= isize::MAX as usize, "有界队列容量过大");
        let mut slots = Vec::with_capacity(capacity);
        for sequence in 0..capacity {
            slots.push(Slot {
                sequence: AtomicUsize::new(sequence),
                value: UnsafeCell::new(MaybeUninit::uninit()),
            });
        }
        Self {
            slots: slots.into_boxed_slice(),
            mask: capacity - 1,
            enqueue: AtomicUsize::new(0),
            dequeue: AtomicUsize::new(0),
        }
    }

    pub const fn capacity(&self) -> usize {
        self.mask + 1
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn len(&self) -> usize {
        self.enqueue
            .load(Ordering::Acquire)
            .wrapping_sub(self.dequeue.load(Ordering::Acquire))
            .min(self.capacity())
    }

    pub fn try_push(&self, value: T) -> Result<(), T> {
        let mut position = self.enqueue.load(Ordering::Relaxed);
        loop {
            let slot = &self.slots[position & self.mask];
            let sequence = slot.sequence.load(Ordering::Acquire);
            let distance = sequence.wrapping_sub(position) as isize;
            if distance == 0 {
                match self.enqueue.compare_exchange_weak(
                    position,
                    position.wrapping_add(1),
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => {
                        // 该生产者已独占 slot，发布 sequence 前消费者不可见。
                        unsafe { (*slot.value.get()).write(value) };
                        slot.sequence
                            .store(position.wrapping_add(1), Ordering::Release);
                        return Ok(());
                    }
                    Err(observed) => position = observed,
                }
            } else if distance < 0 {
                return Err(value);
            } else {
                position = self.enqueue.load(Ordering::Relaxed);
            }
        }
    }

    /// 只能由队列的唯一消费者调用。
    pub fn try_pop(&self) -> Option<T> {
        let position = self.dequeue.load(Ordering::Relaxed);
        let slot = &self.slots[position & self.mask];
        let sequence = slot.sequence.load(Ordering::Acquire);
        if sequence.wrapping_sub(position.wrapping_add(1)) as isize != 0 {
            return None;
        }
        self.dequeue
            .store(position.wrapping_add(1), Ordering::Relaxed);
        // sequence 的 acquire 已确认生产者完成写入，且只有本消费者读取。
        let value = unsafe { (*slot.value.get()).assume_init_read() };
        slot.sequence
            .store(position.wrapping_add(self.capacity()), Ordering::Release);
        Some(value)
    }
}

impl<T> Drop for BoundedMpsc<T> {
    fn drop(&mut self) {
        while let Some(value) = self.try_pop() {
            drop(value);
        }
    }
}

unsafe impl<T: Send> Send for BoundedMpsc<T> {}
unsafe impl<T: Send> Sync for BoundedMpsc<T> {}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use alloc::sync::Arc;
    use std::thread;

    #[test]
    fn full_queue_rejects_newest_and_wraps() {
        let ring = BoundedMpsc::new(4);
        for value in 0..4 {
            assert_eq!(ring.try_push(value), Ok(()));
        }
        assert_eq!(ring.try_push(4), Err(4));
        assert_eq!(ring.try_pop(), Some(0));
        assert_eq!(ring.try_pop(), Some(1));
        assert_eq!(ring.try_push(4), Ok(()));
        assert_eq!(ring.try_push(5), Ok(()));
        assert_eq!(ring.try_pop(), Some(2));
        assert_eq!(ring.try_pop(), Some(3));
        assert_eq!(ring.try_pop(), Some(4));
        assert_eq!(ring.try_pop(), Some(5));
        assert_eq!(ring.try_pop(), None);
    }

    #[test]
    fn concurrent_producers_do_not_lose_entries() {
        const PRODUCERS: usize = 4;
        const PER_PRODUCER: usize = 2_000;
        let ring = Arc::new(BoundedMpsc::new(1024));
        let mut producers = Vec::new();
        for producer in 0..PRODUCERS {
            let ring = Arc::clone(&ring);
            producers.push(thread::spawn(move || {
                for sequence in 0..PER_PRODUCER {
                    let mut value = producer * PER_PRODUCER + sequence;
                    loop {
                        match ring.try_push(value) {
                            Ok(()) => break,
                            Err(returned) => {
                                value = returned;
                                thread::yield_now();
                            }
                        }
                    }
                }
            }));
        }
        let mut seen = alloc::vec![false; PRODUCERS * PER_PRODUCER];
        let mut received = 0;
        while received != seen.len() {
            if let Some(value) = ring.try_pop() {
                assert!(!seen[value]);
                seen[value] = true;
                received += 1;
            } else {
                thread::yield_now();
            }
        }
        for producer in producers {
            producer.join().unwrap();
        }
        assert!(seen.into_iter().all(|value| value));
    }
}
