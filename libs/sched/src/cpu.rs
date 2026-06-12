//! CPU 位图、调度域与拓扑选择。
//!
//! 本模块只描述调度核心需要的通用 CPU 拓扑：可支持 CPU 集、当前在线集、
//! 以及按调度域做任务放置和负载均衡的选择规则。面向用户态的 ABI 编码在
//! syscall 兼容层完成，这里只保留内核内部的稳定模型。

use errno::Errno;

/// 当前构建支持的最大 CPU 数。
///
/// 这里是调度核心的固定容量，不表示所有 CPU 都已经上线；在线状态由
/// scheduler 运行期维护。固定容量让 per-CPU 数组可以静态分配，避免热路径分配。
pub const MAX_CPUS: usize = 8;

/// 启动 CPU。空亲和性在核心内部不能存在，必要时统一退回到该 CPU。
pub const BOOT_CPU_ID: usize = 0;

/// 调度域最大数量。当前以 CPU 数为上限，足够表达根域和每 CPU/每簇子域。
pub const MAX_SCHED_DOMAINS: usize = MAX_CPUS;

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

/// 调度核心内部使用的 CPU 编号。
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
}

impl SchedDomain {
    pub const fn empty() -> Self {
        Self {
            id: usize::MAX,
            span: CpuMask::EMPTY,
            level: 0,
            parent: None,
        }
    }

    pub const fn root() -> Self {
        Self {
            id: ROOT_SCHED_DOMAIN_ID,
            span: CpuMask::SUPPORTED,
            level: 0,
            parent: None,
        }
    }

    pub fn new(id: usize, span: CpuMask, level: u8, parent: Option<usize>) -> Result<Self, Errno> {
        if id >= MAX_SCHED_DOMAINS || span.is_empty() || !CpuMask::SUPPORTED.contains_mask(span) {
            return Err(Errno::EINVAL);
        }
        Ok(Self {
            id,
            span,
            level,
            parent,
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
}

/// 某个任务在当前拓扑下的调度放置快照。
///
/// 这个结构只描述调度核心的通用事实，不包含任何用户态 ABI 编码。`affinity`
/// 是任务声明的 CPU 许可集，`effective` 是再与在线 CPU 集相交后的实际可运行集；
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
        online: CpuMask,
        current: Option<CpuId>,
        prefer_current: bool,
        mut load_of: F,
    ) -> Option<CpuId>
    where
        F: FnMut(CpuId) -> usize,
    {
        let eligible = allowed.intersection(online);
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
            if let Some(cpu) = choose_least_loaded(local, &mut load_of) {
                return Some(cpu);
            }
        }

        choose_least_loaded(eligible, &mut load_of)
    }

    /// 计算任务在当前拓扑下的放置快照。
    ///
    /// 该函数不修改 runqueue，也不持有任何外部锁；调用方把实时负载通过
    /// `load_of` 闭包注入。这样 syscall 查询、调试输出和实际入队选择可以共享
    /// 同一套拓扑规则，避免各处重复硬编码 CPU 选择策略。
    pub fn describe_placement<F>(
        self,
        affinity: CpuMask,
        online: CpuMask,
        current: Option<CpuId>,
        prefer_current: bool,
        load_of: F,
    ) -> SchedPlacement
    where
        F: FnMut(CpuId) -> usize,
    {
        let affinity = affinity.or_boot();
        let effective = affinity.intersection(online);
        let current_domain = current
            .and_then(|cpu| self.domain_for_cpu(cpu))
            .map(|domain| domain.id());
        let preferred_cpu = self.select_cpu(affinity, online, current, prefer_current, load_of);
        SchedPlacement {
            current_cpu: current,
            current_domain,
            preferred_cpu,
            affinity,
            effective,
        }
    }

    /// 返回可从中拉取任务的同域 CPU 集，不包含本 CPU。
    pub fn balance_sources(self, cpu: CpuId, online: CpuMask) -> CpuMask {
        let span = self
            .domain_for_cpu(cpu)
            .map(|domain| domain.span)
            .unwrap_or_else(|| self.root_domain().span);
        span.intersection(online).without(cpu)
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
}

fn choose_least_loaded<F>(mask: CpuMask, load_of: &mut F) -> Option<CpuId>
where
    F: FnMut(CpuId) -> usize,
{
    let mut best = None;
    let mut best_load = usize::MAX;
    for cpu in mask.iter() {
        let load = load_of(cpu);
        if best.is_none() || load < best_load {
            best = Some(cpu);
            best_load = load;
        }
    }
    best
}
