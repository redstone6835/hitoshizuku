//! 文件 readiness 的发布源与 push 订阅边界。

#[cfg(test)]
use alloc::sync::Arc;
use alloc::sync::Weak;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::file::PollEvents;
use crate::sync::Spinlock;

static NEXT_SOURCE_ID: AtomicU64 = AtomicU64::new(1);

pub trait PollSubscriber: Send + Sync {
    fn readiness_changed(&self, source: u64, readiness: PollEvents, generation: u64);
}

struct Subscription {
    id: u64,
    subscriber: Weak<dyn PollSubscriber>,
}

pub struct PollSource {
    id: u64,
    state: Spinlock<PollState>,
    next_version: AtomicU64,
    next_subscription: AtomicU64,
    subscriptions: Spinlock<Vec<Subscription>>,
}

struct PollState {
    readiness: PollEvents,
    generation: u64,
    published_version: u64,
}

impl PollSource {
    pub fn new(initial: PollEvents) -> Self {
        let id = NEXT_SOURCE_ID.fetch_add(1, Ordering::Relaxed);
        assert!(id != 0, "PollSource id 已耗尽");
        Self {
            id,
            state: Spinlock::new(PollState {
                readiness: initial,
                generation: 1,
                published_version: 0,
            }),
            next_version: AtomicU64::new(1),
            next_subscription: AtomicU64::new(1),
            subscriptions: Spinlock::new(Vec::new()),
        }
    }

    pub const fn id(&self) -> u64 {
        self.id
    }

    pub fn snapshot(&self) -> (PollEvents, u64) {
        let state = self.state.lock();
        (state.readiness, state.generation)
    }

    /// 在修改 readiness 所属状态时预留发布版本。
    ///
    /// 调用方应在持有自身状态锁时取版本，释放状态锁后再发布，
    /// 从而使并发发布的顺序与状态修改顺序一致。
    pub fn reserve_version(&self) -> u64 {
        let version = self.next_version.fetch_add(1, Ordering::Relaxed);
        assert!(version != 0, "PollSource version 已耗尽");
        version
    }

    pub fn subscribe(&self, subscriber: Weak<dyn PollSubscriber>) -> u64 {
        let id = self.next_subscription.fetch_add(1, Ordering::Relaxed);
        assert!(id != 0, "PollSource subscription id 已耗尽");
        self.subscriptions
            .lock()
            .push(Subscription { id, subscriber });
        id
    }

    pub fn unsubscribe(&self, id: u64) {
        self.subscriptions.lock().retain(|entry| entry.id != id);
    }

    pub fn publish(&self, readiness: PollEvents) -> u64 {
        let version = self.reserve_version();
        self.publish_versioned(readiness, version)
    }

    /// 只在 `version` 新于已发布状态时更新 readiness，迟到发布会被忽略。
    pub fn publish_versioned(&self, readiness: PollEvents, version: u64) -> u64 {
        let generation = {
            let mut state = self.state.lock();
            if version <= state.published_version {
                return state.generation;
            }
            state.published_version = version;
            if state.readiness == readiness {
                return state.generation;
            }
            state.readiness = readiness;
            state.generation = state.generation.wrapping_add(1).max(1);
            state.generation
        };
        self.subscriptions
            .lock()
            .retain(|entry| entry.subscriber.strong_count() != 0);
        let mut after = 0u64;
        loop {
            let next = {
                let subscriptions = self.subscriptions.lock();
                let index = subscriptions.partition_point(|entry| entry.id <= after);
                subscriptions
                    .get(index)
                    .map(|entry| (entry.id, entry.subscriber.clone()))
            };
            let Some((id, subscriber)) = next else {
                break;
            };
            after = id;
            if let Some(subscriber) = subscriber.upgrade() {
                subscriber.readiness_changed(self.id, readiness, generation);
            }
        }
        generation
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::AtomicU64;

    struct Observer(AtomicU64);

    impl PollSubscriber for Observer {
        fn readiness_changed(&self, _source: u64, _readiness: PollEvents, generation: u64) {
            self.0.store(generation, Ordering::Release);
        }
    }

    #[test]
    fn publish_notifies_without_repeating_unchanged_level() {
        let source = PollSource::new(PollEvents::default());
        let observer = Arc::new(Observer(AtomicU64::new(0)));
        let subscriber: Arc<dyn PollSubscriber> = observer.clone();
        source.subscribe(Arc::downgrade(&subscriber));
        let generation = source.publish(PollEvents::POLLIN);
        assert_eq!(observer.0.load(Ordering::Acquire), generation);
        source.publish(PollEvents::POLLIN);
        assert_eq!(observer.0.load(Ordering::Acquire), generation);
    }

    #[test]
    fn stale_version_cannot_overwrite_newer_readiness() {
        let source = PollSource::new(PollEvents::default());
        let older = source.reserve_version();
        let newer = source.reserve_version();
        source.publish_versioned(PollEvents::POLLIN, newer);
        source.publish_versioned(PollEvents::default(), older);
        assert_eq!(source.snapshot().0, PollEvents::POLLIN);
    }
}
