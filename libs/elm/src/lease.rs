//! ELM 调用、绑定和资源的租约模型。
//!
//! 租约把 owner cell/generation、资源 kind、权限、可选 binding 和状态绑定在一起。任何会
//! 销毁、解绑、暂停或替换对象的事务都必须先查询相关活动租约；仍有引用时返回 busy，而不是
//! 回收仍可能被原生代码访问的对象。
//!
//! revoke 先阻止新 acquire，再等待 active reference 归零，最后进入 revoked。旧 generation
//! 的 lease 不能被新 generation 继承，除非热替换事务明确迁移并重新绑定。

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use crate::error::{ElmError, ElmResult};
use crate::ids::{BindingId, ElmId, Generation, LeaseId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// `LeaseKind` 列举该协议位置允许出现的全部稳定类别；未知数值不得直接转为此枚举。
pub enum LeaseKind {
    /// `Device` 表示 `LeaseKind` 的对象类别：`device`。
    Device,
    /// `Irq` 表示 `LeaseKind` 的对象类别：`irq`。
    Irq,
    /// `Dma` 表示 `LeaseKind` 的对象类别：`dma`。
    Dma,
    /// `Mmio` 表示 `LeaseKind` 的对象类别：`mmio`。
    Mmio,
    /// `VfsNode` 表示 `LeaseKind` 的对象类别：`vfs node`。
    VfsNode,
    /// `Network` 表示 `LeaseKind` 的对象类别：`network`。
    Network,
    /// `Block` 表示 `LeaseKind` 的对象类别：`block`。
    Block,
    /// `MenuItem` 表示 `LeaseKind` 的对象类别：`menu item`。
    MenuItem,
    /// `Provider` 表示 `LeaseKind` 的对象类别：`provider`。
    Provider,
    /// `RuntimePort` 表示 `LeaseKind` 的对象类别：`runtime port`。
    RuntimePort,
    /// `EventSubscription` 表示 `LeaseKind` 的对象类别：`event subscription`。
    EventSubscription,
    /// `Other` 表示 `LeaseKind` 的对象类别：`other`。
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// 一个租约授予的可组合读、写、调用、映射或管理权限位。
pub struct LeaseRights {
    /// `read` 表示该条件在当前快照或计划中是否成立。
    pub read: bool,
    /// `write` 表示该条件在当前快照或计划中是否成立。
    pub write: bool,
    /// 对应 owned resource 的受控生命周期操作表。
    pub control: bool,
}

impl LeaseRights {
    /// `READ` 是租约授予的同名基础权限位，可与其他 `LeaseRights` 按位组合。
    pub const READ: Self = Self {
        read: true,
        write: false,
        control: false,
    };

    /// `WRITE` 是租约授予的同名基础权限位，可与其他 `LeaseRights` 按位组合。
    pub const WRITE: Self = Self {
        read: false,
        write: true,
        control: false,
    };

    /// `CONTROL` 是租约授予的同名基础权限位，可与其他 `LeaseRights` 按位组合。
    pub const CONTROL: Self = Self {
        read: true,
        write: true,
        control: true,
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// `LeaseState` 列举该协议位置允许出现的全部稳定类别；未知数值不得直接转为此枚举。
pub enum LeaseState {
    /// `Active` 表示 `LeaseState` 的生命周期状态：`active`。
    Active,
    /// `Revoking` 表示 `LeaseState` 的生命周期状态：`revoking`。
    Revoking,
    /// `Revoked` 表示 `LeaseState` 的生命周期状态：`revoked`。
    Revoked,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// 绑定 owner generation、资源种类、权限、引用计数和撤销状态的单个租约。
pub struct ResourceLease {
    /// 该对象在所属表或运行时注册表中的稳定标识符。
    pub id: LeaseId,
    /// 拥有该对象的 cell id；所有生命周期和权限检查都归属于该 owner。
    pub owner: ElmId,
    /// 该记录、资源或关系的类别编码。
    pub kind: LeaseKind,
    /// 租约授予的可组合权限位集合。
    pub rights: LeaseRights,
    /// 对象当前代际；用于拒绝热替换前遗留的陈旧引用。
    pub generation: Generation,
    /// 对象或单元的当前状态编码。
    pub state: LeaseState,
    /// `active_refs` 是对应对象、调用或引用的数量。
    pub active_refs: usize,
    /// 该记录关联的 binding id。
    pub binding: Option<BindingId>,
}

impl ResourceLease {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
    pub const fn new(
        id: LeaseId,
        owner: ElmId,
        kind: LeaseKind,
        rights: LeaseRights,
        generation: Generation,
    ) -> Self {
        Self {
            id,
            owner,
            kind,
            rights,
            generation,
            state: LeaseState::Active,
            active_refs: 0,
            binding: None,
        }
    }

    /// 设置 `binding` 并返回更新后的值，便于构建器式初始化。
    pub const fn with_binding(mut self, binding: BindingId) -> Self {
        self.binding = Some(binding);
        self
    }

    /// 执行 `begin_revoke` 定义的模型或协议操作；返回值反映校验后的结果。
    pub fn begin_revoke(&mut self) -> ElmResult<()> {
        match self.state {
            LeaseState::Active => {
                self.state = LeaseState::Revoking;
                Ok(())
            }
            LeaseState::Revoking | LeaseState::Revoked => Err(ElmError::InvalidLeaseState),
        }
    }

    /// 执行 `finish_revoke` 定义的模型或协议操作；返回值反映校验后的结果。
    pub fn finish_revoke(&mut self) -> ElmResult<()> {
        if self.state != LeaseState::Revoking {
            return Err(ElmError::InvalidLeaseState);
        }
        if self.active_refs != 0 {
            return Err(ElmError::LeaseBusy);
        }
        self.state = LeaseState::Revoked;
        Ok(())
    }
}

#[derive(Debug, Default)]
/// 按 lease id 管理 acquire、release、revoke、查询和 owner 清理的注册表。
pub struct LeaseRegistry {
    leases: BTreeMap<LeaseId, ResourceLease>,
}

impl LeaseRegistry {
    /// 构造一个字段满足当前 ABI 基本不变量的新值。
    pub const fn new() -> Self {
        Self {
            leases: BTreeMap::new(),
        }
    }

    /// 执行 `insert` 定义的模型或协议操作；返回值反映校验后的结果。
    pub fn insert(&mut self, lease: ResourceLease) -> ElmResult<()> {
        if self.leases.contains_key(&lease.id) {
            return Err(ElmError::DuplicateLease);
        }
        self.leases.insert(lease.id, lease);
        Ok(())
    }

    /// 执行 `get` 定义的模型或协议操作；返回值反映校验后的结果。
    pub fn get(&self, id: LeaseId) -> Option<&ResourceLease> {
        self.leases.get(&id)
    }

    /// 查找 `mut`；不存在时返回空值或模型错误。
    pub fn get_mut(&mut self, id: LeaseId) -> Option<&mut ResourceLease> {
        self.leases.get_mut(&id)
    }

    /// 返回当前视图包含的有效记录或字节数量。
    pub fn len(&self) -> usize {
        self.leases.len()
    }

    /// 判断当前视图是否不含任何有效记录。
    pub fn is_empty(&self) -> bool {
        self.leases.is_empty()
    }

    /// 执行 `iter` 定义的模型或协议操作；返回值反映校验后的结果。
    pub fn iter(&self) -> impl Iterator<Item = &ResourceLease> {
        self.leases.values()
    }

    /// 执行 `iter_mut` 定义的模型或协议操作；返回值反映校验后的结果。
    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut ResourceLease> {
        self.leases.values_mut()
    }

    /// 执行 `busy_owned_by` 定义的模型或协议操作；返回值反映校验后的结果。
    pub fn busy_owned_by(&self, owner: ElmId) -> usize {
        self.leases
            .values()
            .filter(|lease| lease.owner == owner && lease.active_refs != 0)
            .count()
    }

    /// 查找 `by_binding`；不存在时返回空值或模型错误。
    pub fn get_by_binding(&self, binding: BindingId) -> Option<&ResourceLease> {
        self.leases
            .values()
            .find(|lease| lease.binding == Some(binding))
    }

    /// 向模型注册 `active_ref`，并拒绝重复 id、非法关系或环。
    pub fn add_active_ref(&mut self, id: LeaseId) -> ElmResult<usize> {
        let Some(lease) = self.leases.get_mut(&id) else {
            return Err(ElmError::InvalidLeaseState);
        };
        if lease.state != LeaseState::Active {
            return Err(ElmError::InvalidLeaseState);
        }
        lease.active_refs = lease
            .active_refs
            .checked_add(1)
            .ok_or(ElmError::LeaseBusy)?;
        Ok(lease.active_refs)
    }

    /// 执行 `release_active_ref` 定义的模型或协议操作；返回值反映校验后的结果。
    pub fn release_active_ref(&mut self, id: LeaseId) -> ElmResult<usize> {
        let Some(lease) = self.leases.get_mut(&id) else {
            return Err(ElmError::InvalidLeaseState);
        };
        if lease.active_refs == 0 {
            return Err(ElmError::InvalidLeaseState);
        }
        lease.active_refs -= 1;
        Ok(lease.active_refs)
    }

    /// 执行 `revoke_all_owned_by` 定义的模型或协议操作；返回值反映校验后的结果。
    pub fn revoke_all_owned_by(&mut self, owner: ElmId) -> ElmResult<usize> {
        let mut count = 0;
        for lease in self.leases.values_mut() {
            if lease.owner == owner && lease.state == LeaseState::Active {
                lease.begin_revoke()?;
                count += 1;
            }
        }
        Ok(count)
    }

    /// 执行 `revoke_and_remove` 定义的模型或协议操作；返回值反映校验后的结果。
    pub fn revoke_and_remove(&mut self, id: LeaseId) -> ElmResult<LeaseId> {
        let Some(lease) = self.leases.get_mut(&id) else {
            return Err(ElmError::InvalidLeaseState);
        };
        if lease.active_refs != 0 {
            return Err(ElmError::LeaseBusy);
        }
        if lease.state == LeaseState::Active {
            lease.begin_revoke()?;
        }
        if lease.state == LeaseState::Revoking {
            lease.finish_revoke()?;
        }
        self.leases.remove(&id);
        Ok(id)
    }

    /// 执行 `revoke_and_remove_owned_by` 定义的模型或协议操作；返回值反映校验后的结果。
    pub fn revoke_and_remove_owned_by(&mut self, owner: ElmId) -> ElmResult<Vec<LeaseId>> {
        let owned_count = self
            .leases
            .values()
            .filter(|lease| lease.owner == owner)
            .count();
        let mut revoked = Vec::new();
        revoked
            .try_reserve_exact(owned_count)
            .map_err(|_| ElmError::LeaseBusy)?;
        if self
            .leases
            .values()
            .any(|lease| lease.owner == owner && lease.active_refs != 0)
        {
            return Err(ElmError::LeaseBusy);
        }
        for lease in self.leases.values_mut() {
            if lease.owner != owner {
                continue;
            }
            if lease.state == LeaseState::Active {
                lease.begin_revoke()?;
            }
            if lease.state == LeaseState::Revoking {
                lease.finish_revoke()?;
            }
            revoked.push(lease.id);
        }
        for id in &revoked {
            self.leases.remove(id);
        }
        Ok(revoked)
    }
}
