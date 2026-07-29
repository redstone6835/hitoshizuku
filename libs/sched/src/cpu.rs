//! CPU 位图、调度域与拓扑选择。
//!
//! 本模块只描述调度器需要的通用 CPU 拓扑：可支持 CPU 集、当前在线集、
//! 以及按调度域做任务放置和负载均衡的选择规则。面向用户态的 ABI 编码在
//! syscall 兼容层完成，这里只保留内核内部的稳定模型。

use core::sync::atomic::{AtomicUsize, Ordering};
use errno::Errno;

/// 平局打破计数器：当多个 CPU 利用率相同时轮询选择，避免所有新任务
/// 集中到编号最小的 CPU 上。
static PLACEMENT_ROUND_ROBIN: AtomicUsize = AtomicUsize::new(0);

/// 当前构建支持的最大 CPU 数。
///
/// 这里是调度器的固定容量，不表示所有 CPU 都已经上线；在线状态由
/// scheduler 运行期维护。固定容量让 per-CPU 数组可以静态分配，避免热路径分配。
pub const MAX_CPUS: usize = 8;

/// 启动 CPU。空亲和性在调度器内部不能存在，必要时统一退回到该 CPU。
pub const BOOT_CPU_ID: usize = 0;

/// 调度域最大数量。可容纳根域、CPU 叶子域以及中间的 package/cluster/core 域。
pub const MAX_SCHED_DOMAINS: usize = MAX_CPUS * 2;

/// 单个同构 CPU 的标准调度容量。
pub const SCHED_CAPACITY_SCALE: u64 = 1024;

/// 根调度域编号。根域必须覆盖所有受支持 CPU。
pub const ROOT_SCHED_DOMAIN_ID: usize = 0;

const BITS_PER_BYTE: usize = 8;

const fn supported_bits() -> u64 {
    if MAX_CPUS >= u64::BITS as usize {
        u64::MAX
    } else {
        (1u64 << MAX_CPUS) - 1
    }
}

const fn cpu_bit_raw(cpu_id: usize) -> u64 {
    if cpu_id < MAX_CPUS && cpu_id < u64::BITS as usize {
        1u64 << cpu_id
    } else {
        0
    }
}

/// 调度器内部使用的 CPU 编号。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct CpuId(usize);

impl CpuId {
    pub const fn new(raw: usize) -> Option<Self> {
        if raw < MAX_CPUS {
            Some(Self(raw))
        } else {
            None
        }
    }

    pub const fn boot() -> Self {
        Self(BOOT_CPU_ID)
    }

    pub const fn get(self) -> usize {
        self.0
    }

    pub const fn mask(self) -> CpuMask {
        CpuMask(cpu_bit_raw(self.0))
    }
}

/// 固定宽度 CPU 位图。
///
/// 位图始终被截断到当前构建支持的 CPU 范围内；空位图只在临时计算中使用，
/// 任务亲和性等持久状态必须通过 [`CpuMask::or_boot`] 转为非空。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuMask(u64);

impl CpuMask {
    pub const EMPTY: Self = Self(0);
    pub const SUPPORTED: Self = Self(supported_bits());
    pub const BOOT: Self = Self(cpu_bit_raw(BOOT_CPU_ID));

    pub const fn supported() -> Self {
        Self::SUPPORTED
    }

    pub const fn single(cpu: CpuId) -> Self {
        cpu.mask()
    }

    pub const fn single_raw(cpu_id: usize) -> Self {
        Self(cpu_bit_raw(cpu_id))
    }

    pub const fn from_bits_truncate(bits: u64) -> Self {
        Self(bits & supported_bits())
    }

    pub const fn from_bits_or_boot(bits: u64) -> Self {
        Self::from_bits_truncate(bits).or_boot()
    }

    pub const fn bits(self) -> u64 {
        self.0
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub const fn or_boot(self) -> Self {
        if self.is_empty() { Self::BOOT } else { self }
    }

    pub const fn intersection(self, other: Self) -> Self {
        Self(self.0 & other.0 & supported_bits())
    }

    pub const fn union(self, other: Self) -> Self {
        Self((self.0 | other.0) & supported_bits())
    }

    pub const fn without(self, cpu: CpuId) -> Self {
        Self(self.0 & !cpu.mask().0 & supported_bits())
    }

    pub const fn contains(self, cpu: CpuId) -> bool {
        (self.0 & cpu.mask().0) != 0
    }

    pub const fn contains_raw(self, cpu_id: usize) -> bool {
        (self.0 & cpu_bit_raw(cpu_id)) != 0
    }

    pub const fn intersects(self, other: Self) -> bool {
        (self.0 & other.0 & supported_bits()) != 0
    }

    pub const fn contains_mask(self, other: Self) -> bool {
        (other.0 & !self.0 & supported_bits()) == 0
    }

    pub fn count(self) -> usize {
        self.0.count_ones() as usize
    }

    pub fn first(self) -> Option<CpuId> {
        self.iter().next()
    }

    pub fn iter(self) -> CpuMaskIter {
        CpuMaskIter {
            bits: self.intersection(Self::SUPPORTED).0,
        }
    }

    pub const fn supported_storage_bytes() -> usize {
        let supported = if MAX_CPUS > u64::BITS as usize {
            u64::BITS as usize
        } else {
            MAX_CPUS
        };
        let bytes = supported.div_ceil(BITS_PER_BYTE);
        if bytes == 0 { 1 } else { bytes }
    }
}

/// CPU 位图迭代器，按 CPU 编号从小到大返回。
pub struct CpuMaskIter {
    bits: u64,
}

impl Iterator for CpuMaskIter {
    type Item = CpuId;

    fn next(&mut self) -> Option<Self::Item> {
        while self.bits != 0 {
            let raw = self.bits.trailing_zeros() as usize;
            self.bits &= self.bits - 1;
            if let Some(cpu) = CpuId::new(raw) {
                return Some(cpu);
            }
        }
        None
    }
}

/// 单个调度域。
///
/// `span` 表示该域覆盖的 CPU 集；`parent` 用于表达层级关系。调度器在任务放置
/// 与均衡时优先在当前 CPU 所属域内选择，域内没有可用 CPU 时才回退到根域。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchedDomain {
    id: usize,
    span: CpuMask,
    level: u8,
    parent: Option<usize>,
    capacity: u64,
}

impl SchedDomain {
    pub const fn empty() -> Self {
        Self {
            id: usize::MAX,
            span: CpuMask::EMPTY,
            level: 0,
            parent: None,
            capacity: 0,
        }
    }

    pub const fn root() -> Self {
        Self {
            id: ROOT_SCHED_DOMAIN_ID,
            span: CpuMask::SUPPORTED,
            level: 0,
            parent: None,
            capacity: MAX_CPUS as u64 * SCHED_CAPACITY_SCALE,
        }
    }

    pub fn new(id: usize, span: CpuMask, level: u8, parent: Option<usize>) -> Result<Self, Errno> {
        let capacity = span.count() as u64 * SCHED_CAPACITY_SCALE;
        Self::with_capacity(id, span, level, parent, capacity)
    }

    pub fn with_capacity(
        id: usize,
        span: CpuMask,
        level: u8,
        parent: Option<usize>,
        capacity: u64,
    ) -> Result<Self, Errno> {
        if id >= MAX_SCHED_DOMAINS || span.is_empty() || !CpuMask::SUPPORTED.contains_mask(span) {
            return Err(Errno::EINVAL);
        }
        if capacity == 0 {
            return Err(Errno::EINVAL);
        }
        Ok(Self {
            id,
            span,
            level,
            parent,
            capacity,
        })
    }

    pub const fn id(self) -> usize {
        self.id
    }

    pub const fn span(self) -> CpuMask {
        self.span
    }

    pub const fn level(self) -> u8 {
        self.level
    }

    pub const fn parent(self) -> Option<usize> {
        self.parent
    }

    pub const fn capacity(self) -> u64 {
        self.capacity
    }

    /// 返回只计入 active CPU 后的有效容量。
    pub fn effective_capacity(self, active: CpuMask) -> u64 {
        let total_cpus = self.span.count() as u64;
        let active_cpus = self.span.intersection(active).count() as u64;
        if total_cpus == 0 || active_cpus == 0 {
            return 0;
        }
        self.capacity.saturating_mul(active_cpus) / total_cpus
    }
}

/// 某个任务在当前拓扑下的调度放置快照。
///
/// 这个结构只描述调度器的通用事实，不包含任何用户态 ABI 编码。`affinity`
/// 是任务声明的 CPU 许可集，`effective` 是再与 active CPU 集相交后的实际可运行集；
/// `preferred_cpu` 是按当前负载、调度域和是否优先保持原 CPU 计算出的候选 CPU。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchedPlacement {
    pub current_cpu: Option<CpuId>,
    pub current_domain: Option<usize>,
    pub preferred_cpu: Option<CpuId>,
    pub affinity: CpuMask,
    pub effective: CpuMask,
}

/// 固定容量调度拓扑。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchedTopology {
    domains: [SchedDomain; MAX_SCHED_DOMAINS],
    len: usize,
    cpu_domain: [usize; MAX_CPUS],
}

impl SchedTopology {
    pub const fn bootstrap() -> Self {
        let mut domains = [SchedDomain::empty(); MAX_SCHED_DOMAINS];
        domains[ROOT_SCHED_DOMAIN_ID] = SchedDomain::root();
        Self {
            domains,
            len: 1,
            cpu_domain: [ROOT_SCHED_DOMAIN_ID; MAX_CPUS],
        }
    }

    /// 构造 `Root -> Cpu` 的合成拓扑。
    pub const fn with_cpu_domains() -> Self {
        let mut domains = [SchedDomain::empty(); MAX_SCHED_DOMAINS];
        let mut cpu_domain = [ROOT_SCHED_DOMAIN_ID; MAX_CPUS];
        domains[ROOT_SCHED_DOMAIN_ID] = SchedDomain::root();
        let mut cpu = 0usize;
        while cpu < MAX_CPUS {
            let domain_id = cpu + 1;
            domains[domain_id] = SchedDomain {
                id: domain_id,
                span: CpuMask::single_raw(cpu),
                level: 1,
                parent: Some(ROOT_SCHED_DOMAIN_ID),
                capacity: SCHED_CAPACITY_SCALE,
            };
            cpu_domain[cpu] = domain_id;
            cpu += 1;
        }
        Self {
            domains,
            len: MAX_CPUS + 1,
            cpu_domain,
        }
    }

    pub fn from_domains(input: &[SchedDomain]) -> Result<Self, Errno> {
        if input.is_empty() || input.len() > MAX_SCHED_DOMAINS {
            return Err(Errno::EINVAL);
        }
        if input[0].id != ROOT_SCHED_DOMAIN_ID
            || !input[0].span.contains_mask(CpuMask::SUPPORTED)
            || input[0].parent.is_some()
        {
            return Err(Errno::EINVAL);
        }

        let mut out = Self::bootstrap();
        out.len = input.len();
        out.domains = [SchedDomain::empty(); MAX_SCHED_DOMAINS];

        for (idx, domain) in input.iter().copied().enumerate() {
            if domain.id != idx
                || domain.span.is_empty()
                || !CpuMask::SUPPORTED.contains_mask(domain.span)
            {
                return Err(Errno::EINVAL);
            }
            if idx != ROOT_SCHED_DOMAIN_ID && domain.parent.is_none() {
                return Err(Errno::EINVAL);
            }
            out.domains[idx] = domain;
        }

        for idx in 1..out.len {
            let parent = out.domains[idx].parent.ok_or(Errno::EINVAL)?;
            if parent >= out.len || parent == idx {
                return Err(Errno::EINVAL);
            }
            let parent_span = out.domains[parent].span;
            if !parent_span.contains_mask(out.domains[idx].span) {
                return Err(Errno::EINVAL);
            }
            if out.domains[parent].level >= out.domains[idx].level {
                return Err(Errno::EINVAL);
            }
            let mut seen = 0usize;
            let mut cursor = Some(parent);
            while let Some(parent_id) = cursor {
                if parent_id == idx || seen >= out.len {
                    return Err(Errno::EINVAL);
                }
                seen += 1;
                cursor = out.domains[parent_id].parent;
            }
        }

        // 非根域可以嵌套，但不能形成“相交却互不隶属”的兄弟关系。否则同一
        // CPU 的最小域归属会依赖输入顺序，后续放置和均衡都无法得到稳定语义。
        for left in 1..out.len {
            for right in left + 1..out.len {
                let left_domain = out.domains[left];
                let right_domain = out.domains[right];
                if !left_domain.span.intersects(right_domain.span) {
                    continue;
                }
                if !out.domain_is_ancestor(left, right) && !out.domain_is_ancestor(right, left) {
                    return Err(Errno::EINVAL);
                }
            }
        }

        for cpu in CpuMask::SUPPORTED.iter() {
            out.cpu_domain[cpu.get()] = out.best_domain_for_cpu(cpu);
        }
        Ok(out)
    }

    pub const fn len(self) -> usize {
        self.len
    }

    pub fn domain(self, id: usize) -> Option<SchedDomain> {
        if id < self.len {
            Some(self.domains[id])
        } else {
            None
        }
    }

    pub fn domain_for_cpu(self, cpu: CpuId) -> Option<SchedDomain> {
        self.domain(self.cpu_domain[cpu.get()])
    }

    /// 返回 CPU 所属最小调度域折算到单个 CPU 的容量。
    pub fn cpu_capacity(self, cpu: CpuId) -> u64 {
        let domain = self
            .domain_for_cpu(cpu)
            .unwrap_or_else(|| self.root_domain());
        let cpus = domain.span().count().max(1) as u64;
        (domain.capacity() / cpus).max(1)
    }

    pub fn root_domain(self) -> SchedDomain {
        self.domains[ROOT_SCHED_DOMAIN_ID]
    }

    /// 在给定约束下选择目标 CPU。
    ///
    /// `prefer_current` 为真且当前 CPU 仍可运行时，直接保持原 CPU，避免无意义迁移；
    /// 否则先在当前调度域内挑选低负载 CPU，再回退到全部可用 CPU。
    pub fn select_cpu<F>(
        self,
        allowed: CpuMask,
        active: CpuMask,
        current: Option<CpuId>,
        prefer_current: bool,
        mut load_of: F,
    ) -> Option<CpuId>
    where
        F: FnMut(CpuId) -> usize,
    {
        let eligible = allowed.intersection(active);
        if eligible.is_empty() {
            return None;
        }
        if prefer_current
            && let Some(cpu) = current
            && eligible.contains(cpu)
        {
            return Some(cpu);
        }

        if let Some(cpu) = current
            && let Some(domain) = self.domain_for_cpu(cpu)
        {
            let local = domain.span.intersection(eligible);
            if let Some(cpu) = choose_least_loaded(self, local, &mut load_of) {
                return Some(cpu);
            }
        }

        choose_least_loaded(self, eligible, &mut load_of)
    }

    /// 计算任务在当前拓扑下的放置快照。
    ///
    /// 该函数不修改 runqueue，也不持有任何外部锁；调用方把实时负载通过
    /// `load_of` 闭包注入。这样 syscall 查询、调试输出和实际入队选择可以共享
    /// 同一套拓扑规则，避免各处重复硬编码 CPU 选择策略。
    pub fn describe_placement<F>(
        self,
        affinity: CpuMask,
        active: CpuMask,
        current: Option<CpuId>,
        prefer_current: bool,
        load_of: F,
    ) -> SchedPlacement
    where
        F: FnMut(CpuId) -> usize,
    {
        let affinity = affinity.or_boot();
        let effective = affinity.intersection(active);
        let current_domain = current
            .and_then(|cpu| self.domain_for_cpu(cpu))
            .map(|domain| domain.id());
        let preferred_cpu = self.select_cpu(affinity, active, current, prefer_current, load_of);
        SchedPlacement {
            current_cpu: current,
            current_domain,
            preferred_cpu,
            affinity,
            effective,
        }
    }

    /// 返回最近可拉取任务的调度域 CPU 集，不包含本 CPU。
    pub fn balance_sources(self, cpu: CpuId, active: CpuMask) -> CpuMask {
        let mut domain_id = self.cpu_domain[cpu.get()];
        loop {
            let domain = self.domain(domain_id).unwrap_or_else(|| self.root_domain());
            let sources = domain.span.intersection(active).without(cpu);
            if !sources.is_empty() || domain.id == ROOT_SCHED_DOMAIN_ID {
                return sources;
            }
            domain_id = domain.parent.unwrap_or(ROOT_SCHED_DOMAIN_ID);
        }
    }

    fn best_domain_for_cpu(self, cpu: CpuId) -> usize {
        let mut best = ROOT_SCHED_DOMAIN_ID;
        let mut best_size = usize::MAX;
        let mut best_level = 0u8;
        for idx in 0..self.len {
            let domain = self.domains[idx];
            if !domain.span.contains(cpu) {
                continue;
            }
            let size = domain.span.count();
            if size < best_size || (size == best_size && domain.level >= best_level) {
                best = idx;
                best_size = size;
                best_level = domain.level;
            }
        }
        best
    }

    fn domain_is_ancestor(self, ancestor: usize, child: usize) -> bool {
        let mut seen = 0usize;
        let mut cursor = self.domains[child].parent;
        while let Some(parent) = cursor {
            if parent == ancestor {
                return true;
            }
            if parent >= self.len || seen >= self.len {
                return false;
            }
            seen += 1;
            cursor = self.domains[parent].parent;
        }
        false
    }
}

fn choose_least_loaded<F>(topology: SchedTopology, mask: CpuMask, load_of: &mut F) -> Option<CpuId>
where
    F: FnMut(CpuId) -> usize,
{
    // Two-pass: first find minimum utilization, then pick a CPU at that minimum
    // via round-robin to avoid always returning the lowest-numbered idle CPU.
    let mut min_load = usize::MAX;
    let mut min_capacity = 1u64;
    let mut tie_count = 0usize;

    for cpu in mask.iter() {
        let load = load_of(cpu);
        let capacity = topology.cpu_capacity(cpu);
        let less = (load as u128).saturating_mul(min_capacity as u128)
            < (min_load as u128).saturating_mul(capacity as u128);
        let equal = !less
            && tie_count > 0
            && (load as u128).saturating_mul(min_capacity as u128)
                == (min_load as u128).saturating_mul(capacity as u128);
        if tie_count == 0 || less {
            min_load = load;
            min_capacity = capacity;
            tie_count = 1;
        } else if equal {
            tie_count += 1;
        }
    }
    if tie_count == 0 {
        return None;
    }

    // If only one candidate, or no contention, skip atomic.
    let target_rank = if tie_count > 1 {
        PLACEMENT_ROUND_ROBIN.fetch_add(1, Ordering::Relaxed) % tie_count
    } else {
        0
    };

    let mut rank = 0usize;
    for cpu in mask.iter() {
        let load = load_of(cpu);
        let capacity = topology.cpu_capacity(cpu);
        let is_min = (load as u128).saturating_mul(min_capacity as u128)
            == (min_load as u128).saturating_mul(capacity as u128);
        if is_min {
            if rank == target_rank {
                return Some(cpu);
            }
            rank += 1;
        }
    }
    // Fallback: first candidate
    mask.iter().next()
}
