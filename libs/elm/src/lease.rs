//! 资源租约模型。

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use crate::error::{ElmError, ElmResult};
use crate::ids::{ElmId, Generation, LeaseId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseKind {
    Device,
    Irq,
    Dma,
    Mmio,
    VfsNode,
    Network,
    Block,
    MenuItem,
    Provider,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeaseRights {
    pub read: bool,
    pub write: bool,
    pub control: bool,
}

impl LeaseRights {
    pub const READ: Self = Self {
        read: true,
        write: false,
        control: false,
    };

    pub const CONTROL: Self = Self {
        read: true,
        write: true,
        control: true,
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseState {
    Active,
    Revoking,
    Revoked,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceLease {
    pub id: LeaseId,
    pub owner: ElmId,
    pub kind: LeaseKind,
    pub rights: LeaseRights,
    pub generation: Generation,
    pub state: LeaseState,
    pub active_refs: usize,
}

impl ResourceLease {
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
        }
    }

    pub fn begin_revoke(&mut self) -> ElmResult<()> {
        match self.state {
            LeaseState::Active => {
                self.state = LeaseState::Revoking;
                Ok(())
            }
            LeaseState::Revoking | LeaseState::Revoked => Err(ElmError::InvalidLeaseState),
        }
    }

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
pub struct LeaseRegistry {
    leases: BTreeMap<LeaseId, ResourceLease>,
}

impl LeaseRegistry {
    pub const fn new() -> Self {
        Self {
            leases: BTreeMap::new(),
        }
    }

    pub fn insert(&mut self, lease: ResourceLease) -> ElmResult<()> {
        if self.leases.contains_key(&lease.id) {
            return Err(ElmError::DuplicateLease);
        }
        self.leases.insert(lease.id, lease);
        Ok(())
    }

    pub fn get(&self, id: LeaseId) -> Option<&ResourceLease> {
        self.leases.get(&id)
    }

    pub fn get_mut(&mut self, id: LeaseId) -> Option<&mut ResourceLease> {
        self.leases.get_mut(&id)
    }

    pub fn len(&self) -> usize {
        self.leases.len()
    }

    pub fn is_empty(&self) -> bool {
        self.leases.is_empty()
    }

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

    pub fn revoke_and_remove_owned_by(&mut self, owner: ElmId) -> ElmResult<Vec<LeaseId>> {
        let mut revoked = Vec::new();
        for lease in self.leases.values_mut() {
            if lease.owner != owner {
                continue;
            }
            if lease.state == LeaseState::Active {
                lease.begin_revoke()?;
            }
            if lease.state == LeaseState::Revoking {
                lease.finish_revoke()?;
                revoked.push(lease.id);
            }
        }
        for id in &revoked {
            self.leases.remove(id);
        }
        Ok(revoked)
    }
}
