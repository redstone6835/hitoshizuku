//! UTS 命名空间：hostname 与 domainname。

use core::sync::atomic::{AtomicU64, Ordering};

use spin::Mutex;

use crate::{Namespace, NsType, allocate_ns_inum};

/// `utsname` 字段长度（Linux `__NEW_UTS_LEN + 1`）。
pub const UTS_FIELD_LEN: usize = 65;

/// UTS 命名空间。
pub struct UtsNamespace {
    inum: u64,
    hostname: Mutex<[u8; UTS_FIELD_LEN]>,
    domainname: Mutex<[u8; UTS_FIELD_LEN]>,
}

impl UtsNamespace {
    pub fn new(hostname: &[u8], domainname: &[u8]) -> Arc<Self> {
        Arc::new(Self {
            inum: allocate_ns_inum(),
            hostname: Mutex::new(fill_field(hostname)),
            domainname: Mutex::new(fill_field(domainname)),
        })
    }

    pub fn hostname(&self) -> [u8; UTS_FIELD_LEN] {
        *self.hostname.lock()
    }

    pub fn domainname(&self) -> [u8; UTS_FIELD_LEN] {
        *self.domainname.lock()
    }

    /// 设置 hostname（校验：含 NUL 截断、不含 `/`、长度上限）。
    pub fn set_hostname(&self, name: &[u8]) -> Result<(), errno::Errno> {
        set_field(&self.hostname, name)
    }

    pub fn set_domainname(&self, name: &[u8]) -> Result<(), errno::Errno> {
        set_field(&self.domainname, name)
    }
}

fn fill_field(value: &[u8]) -> [u8; UTS_FIELD_LEN] {
    let mut field = [0u8; UTS_FIELD_LEN];
    let len = value.len().min(UTS_FIELD_LEN - 1);
    field[..len].copy_from_slice(&value[..len]);
    field
}

fn set_field(field: &Mutex<[u8; UTS_FIELD_LEN]>, name: &[u8]) -> Result<(), errno::Errno> {
    if name.iter().any(|byte| *byte == 0) {
        return Err(errno::Errno::EINVAL);
    }
    if name.iter().any(|byte| *byte == b'/') {
        return Err(errno::Errno::EINVAL);
    }
    if name.len() > UTS_FIELD_LEN - 1 {
        return Err(errno::Errno::EINVAL);
    }
    *field.lock() = fill_field(name);
    Ok(())
}

impl Namespace for UtsNamespace {
    fn ns_type(&self) -> NsType {
        NsType::Uts
    }

    fn inum(&self) -> u64 {
        self.inum
    }
}

use alloc::sync::Arc;
