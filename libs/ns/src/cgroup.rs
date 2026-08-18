//! cgroup 命名空间。本内核不实现 cgroupfs 层级，所有命名空间都共享"根"
//! 视图；对象仍然存在，保证 `unshare(CLONE_NEWCGROUP)`/`setns` 与
//! `/proc/self/cgroup` 语义成立。

use crate::{Namespace, NsType, allocate_ns_inum};

/// cgroup 命名空间（恒为根层级）。
pub struct CgroupNamespace {
    inum: u64,
}

impl CgroupNamespace {
    pub fn new() -> alloc::sync::Arc<Self> {
        alloc::sync::Arc::new(Self {
            inum: allocate_ns_inum(),
        })
    }

    /// 本内核无 cgroup 层级：`/proc/self/cgroup` 固定输出 `0::/`。
    pub fn cgroup_path(&self) -> &'static str {
        "/"
    }
}

impl Namespace for CgroupNamespace {
    fn ns_type(&self) -> NsType {
        NsType::Cgroup
    }

    fn inum(&self) -> u64 {
        self.inum
    }
}
