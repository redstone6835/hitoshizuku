use alloc::vec::Vec;

const LEVELS: usize = 4;
const SLOTS: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimerExpiry {
    pub owner: u16,
    pub generation: u32,
    pub deadline_ms: u64,
}

#[derive(Clone, Copy)]
struct TimerNode {
    active: bool,
    level: u8,
    slot: u8,
    previous: Option<u16>,
    next: Option<u16>,
    deadline_ms: u64,
    generation: u32,
}

impl TimerNode {
    const fn empty() -> Self {
        Self {
            active: false,
            level: 0,
            slot: 0,
            previous: None,
            next: None,
            deadline_ms: 0,
            generation: 0,
        }
    }
}

pub struct TimerWheel {
    now_ms: u64,
    heads: [[Option<u16>; SLOTS]; LEVELS],
    slot_deadlines: [[u64; SLOTS]; LEVELS],
    earliest_deadline_ms: Option<u64>,
    nodes: Vec<TimerNode>,
}

impl TimerWheel {
    pub fn new(owner_capacity: usize, now_ms: u64) -> Self {
        assert!(owner_capacity <= usize::from(u16::MAX));
        let mut nodes = Vec::with_capacity(owner_capacity);
        nodes.resize(owner_capacity, TimerNode::empty());
        Self {
            now_ms,
            heads: [[None; SLOTS]; LEVELS],
            slot_deadlines: [[u64::MAX; SLOTS]; LEVELS],
            earliest_deadline_ms: None,
            nodes,
        }
    }

    pub const fn now_ms(&self) -> u64 {
        self.now_ms
    }

    pub fn next_deadline_ns(&self) -> Option<u64> {
        self.earliest_deadline_ms
            .map(|deadline| deadline.saturating_mul(1_000_000))
    }

    pub fn schedule(&mut self, owner: u16, generation: u32, deadline_ns: u64) -> bool {
        if usize::from(owner) >= self.nodes.len() || generation == 0 {
            return false;
        }
        self.cancel(owner);
        let deadline_ms = deadline_ns.saturating_add(999_999) / 1_000_000;
        let deadline_ms = deadline_ms
            .max(self.now_ms.saturating_add(1))
            .min(self.now_ms.saturating_add(u32::MAX as u64));
        self.insert(owner, generation, deadline_ms);
        true
    }

    pub fn cancel(&mut self, owner: u16) -> bool {
        if usize::from(owner) >= self.nodes.len() || !self.nodes[usize::from(owner)].active {
            return false;
        }
        self.detach(owner);
        self.nodes[usize::from(owner)] = TimerNode::empty();
        true
    }

    /// 最多推进 `tick_budget` 个毫秒 tick，避免长时间停顿占满 worker turn。
    pub fn advance(
        &mut self,
        target_ns: u64,
        tick_budget: u32,
        mut fire: impl FnMut(TimerExpiry),
    ) -> bool {
        let target_ms = target_ns / 1_000_000;
        let mut ticks = 0u32;
        while self.now_ms < target_ms && ticks < tick_budget {
            self.now_ms += 1;
            ticks += 1;
            if self.now_ms & 0xff == 0 {
                self.cascade(1);
                if self.now_ms & 0xffff == 0 {
                    self.cascade(2);
                    if self.now_ms & 0xff_ffff == 0 {
                        self.cascade(3);
                    }
                }
            }
            let slot = (self.now_ms & 0xff) as usize;
            let mut current = self.heads[0][slot].take();
            let had_nodes = current.is_some();
            self.slot_deadlines[0][slot] = u64::MAX;
            while let Some(owner) = current {
                let node = self.nodes[usize::from(owner)];
                current = node.next;
                self.nodes[usize::from(owner)] = TimerNode::empty();
                if node.deadline_ms <= self.now_ms {
                    fire(TimerExpiry {
                        owner,
                        generation: node.generation,
                        deadline_ms: node.deadline_ms,
                    });
                } else {
                    self.insert(owner, node.generation, node.deadline_ms);
                }
            }
            if had_nodes {
                self.refresh_earliest();
            }
        }
        self.now_ms < target_ms
    }

    fn insert(&mut self, owner: u16, generation: u32, deadline_ms: u64) {
        let delta = deadline_ms.saturating_sub(self.now_ms);
        let level = if delta < 1 << 8 {
            0
        } else if delta < 1 << 16 {
            1
        } else if delta < 1 << 24 {
            2
        } else {
            3
        };
        let shift = level * 8;
        let slot = ((deadline_ms >> shift) & 0xff) as usize;
        let head = self.heads[level][slot];
        self.nodes[usize::from(owner)] = TimerNode {
            active: true,
            level: level as u8,
            slot: slot as u8,
            previous: None,
            next: head,
            deadline_ms,
            generation,
        };
        if let Some(head) = head {
            self.nodes[usize::from(head)].previous = Some(owner);
        }
        self.heads[level][slot] = Some(owner);
        self.slot_deadlines[level][slot] = self.slot_deadlines[level][slot].min(deadline_ms);
        self.earliest_deadline_ms = Some(
            self.earliest_deadline_ms
                .map_or(deadline_ms, |earliest| earliest.min(deadline_ms)),
        );
    }

    fn detach(&mut self, owner: u16) {
        let node = self.nodes[usize::from(owner)];
        let head = &mut self.heads[usize::from(node.level)][usize::from(node.slot)];
        match node.previous {
            Some(previous) => self.nodes[usize::from(previous)].next = node.next,
            None => *head = node.next,
        }
        if let Some(next) = node.next {
            self.nodes[usize::from(next)].previous = node.previous;
        }
        if self.slot_deadlines[usize::from(node.level)][usize::from(node.slot)] == node.deadline_ms
        {
            self.refresh_slot_deadline(usize::from(node.level), usize::from(node.slot));
        }
        if self.earliest_deadline_ms == Some(node.deadline_ms) {
            self.refresh_earliest();
        }
    }

    fn cascade(&mut self, level: usize) {
        let slot = ((self.now_ms >> (level * 8)) & 0xff) as usize;
        let mut current = self.heads[level][slot].take();
        self.slot_deadlines[level][slot] = u64::MAX;
        while let Some(owner) = current {
            let node = self.nodes[usize::from(owner)];
            current = node.next;
            self.nodes[usize::from(owner)] = TimerNode::empty();
            self.insert(owner, node.generation, node.deadline_ms);
        }
        self.refresh_earliest();
    }

    fn refresh_slot_deadline(&mut self, level: usize, slot: usize) {
        let mut deadline = u64::MAX;
        let mut current = self.heads[level][slot];
        while let Some(owner) = current {
            let node = self.nodes[usize::from(owner)];
            deadline = deadline.min(node.deadline_ms);
            current = node.next;
        }
        self.slot_deadlines[level][slot] = deadline;
    }

    fn refresh_earliest(&mut self) {
        let deadline = self
            .slot_deadlines
            .iter()
            .flat_map(|level| level.iter())
            .copied()
            .min()
            .unwrap_or(u64::MAX);
        self.earliest_deadline_ms = (deadline != u64::MAX).then_some(deadline);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timer_rounds_up_and_rejects_stale_generation_in_consumer() {
        let mut wheel = TimerWheel::new(8, 0);
        assert!(wheel.schedule(3, 7, 1));
        let mut expired = Vec::new();
        assert!(!wheel.advance(1_000_000, 4, |entry| expired.push(entry)));
        assert_eq!(
            expired,
            alloc::vec![TimerExpiry {
                owner: 3,
                generation: 7,
                deadline_ms: 1,
            }]
        );
    }

    #[test]
    fn cancellation_and_reschedule_leave_one_node() {
        let mut wheel = TimerWheel::new(4, 0);
        wheel.schedule(1, 1, 10_000_000);
        wheel.schedule(1, 2, 2_000_000);
        let mut expired = Vec::new();
        wheel.advance(20_000_000, 32, |entry| expired.push(entry));
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].generation, 2);
    }

    #[test]
    fn cached_deadline_tracks_cancel_and_cascade() {
        let mut wheel = TimerWheel::new(8, 0);
        wheel.schedule(1, 1, 300_000_000);
        wheel.schedule(2, 1, 20_000_000);
        assert_eq!(wheel.next_deadline_ns(), Some(20_000_000));
        assert!(wheel.cancel(2));
        assert_eq!(wheel.next_deadline_ns(), Some(300_000_000));
        wheel.advance(256_000_000, 256, |_| {});
        assert_eq!(wheel.next_deadline_ns(), Some(300_000_000));
        wheel.advance(300_000_000, 64, |_| {});
        assert_eq!(wheel.next_deadline_ns(), None);
    }
}
