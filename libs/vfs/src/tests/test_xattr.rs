//! xattr 语义层与 POSIX ACL 的宿主单测。

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use crate::acl::{self, AclCheckKind, PosixAcl};
use crate::inode::{Inode, InodeId, InodeMeta, InodeOps};
use crate::stat::{FileMode, FileType, Timespec};
use crate::vfs::cred::{CapSet, Capability, Credentials, Gid, Uid};
use crate::vfs::error::{VfsError, VfsResult};
use crate::vfs::sync::Spinlock;
use crate::xattr;

/// 内存 xattr 后端（模拟 tmpfs）。
struct MemXattrOps {
    xattrs: Spinlock<alloc::collections::BTreeMap<Vec<u8>, Vec<u8>>>,
    mode: Spinlock<FileMode>,
}

impl MemXattrOps {
    fn new(mode: FileMode) -> Self {
        Self {
            xattrs: Spinlock::new(Default::default()),
            mode: Spinlock::new(mode),
        }
    }
}

impl InodeOps for MemXattrOps {
    fn lookup(&self, _inode: &Inode, _name: &str) -> VfsResult<Arc<Inode>> {
        Err(VfsError::NotFound)
    }
    fn open(
        &self,
        _inode: &Inode,
        _opts: &crate::file::OpenOptions,
        _cred: &Credentials,
    ) -> VfsResult<Box<dyn crate::file::FileOps + Send + Sync>> {
        Err(VfsError::NotSupported)
    }
    fn getxattr(&self, name: &[u8]) -> VfsResult<Option<Vec<u8>>> {
        Ok(self.xattrs.lock().get(name).cloned())
    }
    fn setxattr(&self, name: &[u8], value: &[u8], flags: u32) -> VfsResult<()> {
        let mut map = self.xattrs.lock();
        let exists = map.contains_key(name);
        if flags & xattr::XATTR_CREATE != 0 && exists {
            return Err(VfsError::AlreadyExists);
        }
        if flags & xattr::XATTR_REPLACE != 0 && !exists {
            return Err(VfsError::NoData);
        }
        map.insert(name.to_vec(), value.to_vec());
        Ok(())
    }
    fn listxattr(&self) -> VfsResult<Vec<Vec<u8>>> {
        Ok(self.xattrs.lock().keys().cloned().collect())
    }
    fn removexattr(&self, name: &[u8]) -> VfsResult<()> {
        if self.xattrs.lock().remove(name).is_none() {
            return Err(VfsError::NoData);
        }
        Ok(())
    }
    fn chmod(&self, _inode: &Inode, mode: FileMode) -> VfsResult<()> {
        *self.mode.lock() = mode;
        Ok(())
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

fn make_inode(mode: u16, uid: u32, gid: u32, ops: Arc<MemXattrOps>) -> Arc<Inode> {
    Inode::new(
        InodeId {
            fs_id: crate::stat::FsId::new(0x1234),
            ino: 7,
        },
        FileType::Regular,
        crate::vfs::stat::DevId::new(0, 0),
        4096,
        None,
        InodeMeta {
            size: 0,
            nlink: 1,
            mode: FileMode::new(mode),
            uid: Uid(uid),
            gid: Gid(gid),
            atime: Timespec::ZERO,
            mtime: Timespec::ZERO,
            ctime: Timespec::ZERO,
            blocks: 0,
        },
        ops as Arc<dyn InodeOps + Send + Sync>,
        alloc::sync::Weak::new(),
    )
}

fn cred(uid: u32, gid: u32, caps: &[Capability]) -> Credentials {
    let mut set = CapSet::EMPTY;
    for c in caps {
        set = set.with(*c);
    }
    let mut c = Credentials::unprivileged(Uid(uid), Gid(gid));
    c.caps = set;
    c
}

fn acl6(
    user_perm: u16,
    named_user: Option<(u32, u16)>,
    group_perm: u16,
    named_group: Option<(u32, u16)>,
    mask: u16,
    other_perm: u16,
) -> Vec<u8> {
    let mut entries = vec![acl::AclEntry {
        tag: acl::ACL_USER_OBJ,
        perm: user_perm,
        id: 0,
    }];
    if let Some((id, perm)) = named_user {
        entries.push(acl::AclEntry {
            tag: acl::ACL_USER,
            perm,
            id,
        });
    }
    entries.push(acl::AclEntry {
        tag: acl::ACL_GROUP_OBJ,
        perm: group_perm,
        id: 0,
    });
    if let Some((id, perm)) = named_group {
        entries.push(acl::AclEntry {
            tag: acl::ACL_GROUP,
            perm,
            id,
        });
    }
    entries.push(acl::AclEntry {
        tag: acl::ACL_MASK,
        perm: mask,
        id: 0,
    });
    entries.push(acl::AclEntry {
        tag: acl::ACL_OTHER,
        perm: other_perm,
        id: 0,
    });
    acl::encode(&PosixAcl { entries })
}

// ── 名称解析 ──────────────────────────────────────────────────────────────

#[test]
fn parse_name_namespaces() {
    use xattr::XattrNamespace::*;
    assert_eq!(xattr::parse_name(b"user.foo").unwrap(), User);
    assert_eq!(xattr::parse_name(b"trusted.x").unwrap(), Trusted);
    assert_eq!(xattr::parse_name(b"security.s").unwrap(), Security);
    assert_eq!(
        xattr::parse_name(b"system.posix_acl_access").unwrap(),
        PosixAclAccess
    );
    assert_eq!(
        xattr::parse_name(b"system.posix_acl_default").unwrap(),
        PosixAclDefault
    );
    assert_eq!(xattr::parse_name(b"system.other").unwrap(), OtherSystem);
    // 非法名
    assert_eq!(xattr::parse_name(b""), Err(VfsError::InvalidArgument));
    assert_eq!(xattr::parse_name(b"user."), Err(VfsError::InvalidArgument));
    assert_eq!(
        xattr::parse_name(b"user..x"),
        Err(VfsError::InvalidArgument)
    );
    assert_eq!(
        xattr::parse_name(b"user.a/b"),
        Err(VfsError::InvalidArgument)
    );
    // 未知前缀
    assert_eq!(xattr::parse_name(b"foo.bar"), Err(VfsError::NotSupported));
}

// ── ACL 编解码与校验 ───────────────────────────────────────────────────────

#[test]
fn acl_parse_encode_roundtrip() {
    let bytes = acl6(7, Some((1000, 5)), 4, Some((2000, 2)), 3, 0);
    let acl = acl::parse(&bytes).unwrap();
    assert_eq!(acl.entries.len(), 6);
    assert_eq!(acl.entries[0].tag, acl::ACL_USER_OBJ);
    assert_eq!(
        acl.entries[1],
        acl::AclEntry {
            tag: 0x02,
            perm: 5,
            id: 1000
        }
    );
    assert_eq!(acl.entries[4].tag, acl::ACL_MASK);
    assert_eq!(acl::encode(&acl), bytes);
}

#[test]
fn acl_validation_rules() {
    // 缺基本条目 → 非法
    let bad = acl::encode(&PosixAcl {
        entries: vec![acl::AclEntry {
            tag: acl::ACL_USER_OBJ,
            perm: 7,
            id: 0,
        }],
    });
    assert_eq!(acl::parse(&bad), Err(VfsError::InvalidArgument));
    // 乱序 → 非法
    let bad = acl::encode(&PosixAcl {
        entries: vec![
            acl::AclEntry {
                tag: acl::ACL_USER_OBJ,
                perm: 7,
                id: 0,
            },
            acl::AclEntry {
                tag: acl::ACL_OTHER,
                perm: 0,
                id: 0,
            },
            acl::AclEntry {
                tag: acl::ACL_GROUP_OBJ,
                perm: 0,
                id: 0,
            },
        ],
    });
    assert_eq!(acl::parse(&bad), Err(VfsError::InvalidArgument));
    // 命名条目无 mask → 非法
    let bad = acl::encode(&PosixAcl {
        entries: vec![
            acl::AclEntry {
                tag: acl::ACL_USER_OBJ,
                perm: 7,
                id: 0,
            },
            acl::AclEntry {
                tag: acl::ACL_USER,
                perm: 4,
                id: 1000,
            },
            acl::AclEntry {
                tag: acl::ACL_GROUP_OBJ,
                perm: 0,
                id: 0,
            },
            acl::AclEntry {
                tag: acl::ACL_OTHER,
                perm: 0,
                id: 0,
            },
        ],
    });
    assert_eq!(acl::parse(&bad), Err(VfsError::InvalidArgument));
    // perm 超界 → 非法
    let bad = acl::encode(&PosixAcl {
        entries: vec![acl::AclEntry {
            tag: acl::ACL_USER_OBJ,
            perm: 8,
            id: 0,
        }],
    });
    assert!(acl::parse(&bad).is_err());
}

// ── ACL 强制 ───────────────────────────────────────────────────────────────

#[test]
fn acl_enforcement_owner_named_group_other() {
    // mode 0644 的文件 + ACL：owner rwx、命名 user(1000)=r-x、group r--、
    // 命名 group(2000)=--x、mask r--、other ---
    let ops = Arc::new(MemXattrOps::new(FileMode::new(0o644)));
    let inode = make_inode(0o644, 0, 0, Arc::clone(&ops));
    inode.mark_has_xattrs();
    ops.setxattr(
        acl::ACL_ACCESS_XATTR,
        &acl6(7, Some((1000, 5)), 4, Some((2000, 1)), 4, 0),
        0,
    )
    .unwrap();

    // owner：ACL USER_OBJ rwx → 全放行
    assert!(acl::check_access(
        &cred(0, 0, &[]),
        &inode,
        AclCheckKind::Read
    ));
    assert!(acl::check_access(
        &cred(0, 0, &[]),
        &inode,
        AclCheckKind::Write
    ));
    // 命名 user 1000：r-x（读放行、写拒绝）
    assert!(acl::check_access(
        &cred(1000, 0, &[]),
        &inode,
        AclCheckKind::Read
    ));
    assert!(!acl::check_access(
        &cred(1000, 0, &[]),
        &inode,
        AclCheckKind::Write
    ));
    // 命名 group 2000：--x 且 mask r-- → 有效权限 = --x & r-- = 0 → 读拒绝
    assert!(!acl::check_access(
        &cred(3000, 2000, &[]),
        &inode,
        AclCheckKind::Read
    ));
    // 补充组命中命名组（fsgid=3000、groups=[2000]）同样参与检查
    let mut g = cred(3000, 3000, &[]);
    g.groups = vec![Gid(2000)];
    assert!(!acl::check_access(&g, &inode, AclCheckKind::Read));
    // 命名组条目带读权限且 mask 允许时放行
    let ops2 = Arc::new(MemXattrOps::new(FileMode::new(0o644)));
    let inode2 = make_inode(0o644, 0, 0, Arc::clone(&ops2));
    inode2.mark_has_xattrs();
    ops2.setxattr(
        acl::ACL_ACCESS_XATTR,
        &acl6(7, None, 0, Some((2000, 4)), 4, 0),
        0,
    )
    .unwrap();
    let mut g2 = cred(3000, 3000, &[]);
    g2.groups = vec![Gid(2000)];
    assert!(acl::check_access(&g2, &inode2, AclCheckKind::Read));
    // 组 3000（非成员）：other --- → 拒绝
    assert!(!acl::check_access(
        &cred(3000, 3000, &[]),
        &inode,
        AclCheckKind::Read
    ));
    // DAC_OVERRIDE 能力仍可绕过
    assert!(acl::check_access(
        &cred(3000, 3000, &[Capability::DacOverride]),
        &inode,
        AclCheckKind::Read
    ));
}

#[test]
fn acl_fast_path_without_xattrs() {
    let ops = Arc::new(MemXattrOps::new(FileMode::new(0o600)));
    let inode = make_inode(0o600, 0, 0, ops);
    // 无 xattr：owner 可读写、他人拒绝（纯 mode 路径）
    assert!(acl::check_access(
        &cred(0, 0, &[]),
        &inode,
        AclCheckKind::Read
    ));
    assert!(!acl::check_access(
        &cred(1, 1, &[]),
        &inode,
        AclCheckKind::Read
    ));
}

// ── default ACL 派生（posix_acl_create）───────────────────────────────────

#[test]
fn default_acl_derives_child_acl_and_mode() {
    let default = acl6(7, Some((1000, 5)), 5, None, 7, 0);
    let parsed = acl::parse(&default).unwrap();
    // 创建 mode 0640 → USER_OBJ=6、OTHER=0、MASK=4（组位）
    let (child, mode) = acl::create(&parsed, FileMode::new(0o640)).unwrap();
    let mask = acl::mask_to_mode_group(&child).unwrap();
    assert_eq!(mask, 4);
    assert_eq!(mode.0 & 0o070, 0o040); // 组位 = mask
    assert_eq!(child.entries[0].perm, 6); // USER_OBJ = owner 位
    let other = child
        .entries
        .iter()
        .find(|e| e.tag == acl::ACL_OTHER)
        .unwrap();
    assert_eq!(other.perm, 0); // OTHER = other 位
}

// ── 语义层权限模型 ────────────────────────────────────────────────────────

#[test]
fn xattr_permission_model() {
    let ops = Arc::new(MemXattrOps::new(FileMode::new(0o644)));
    let inode = make_inode(0o644, 0, 0, Arc::clone(&ops));

    // user.*：写需要写权限；他人无写权限 → EACCES
    assert_eq!(
        xattr::setxattr(&inode, b"user.k", b"v", 0, &cred(1, 1, &[])),
        Err(VfsError::PermissionDenied)
    );
    // owner 可写
    assert!(xattr::setxattr(&inode, b"user.k", b"v", 0, &cred(0, 0, &[])).is_ok());
    // 读需要读权限；0600 文件对他人无读权限 → EACCES
    let ops_ro = Arc::new(MemXattrOps::new(FileMode::new(0o600)));
    let inode_ro = make_inode(0o600, 0, 0, Arc::clone(&ops_ro));
    inode_ro.mark_has_xattrs();
    ops_ro.setxattr(b"user.k", b"v", 0).unwrap();
    assert_eq!(
        xattr::getxattr(&inode_ro, b"user.k", &cred(2, 2, &[])),
        Err(VfsError::PermissionDenied)
    );
    assert_eq!(
        xattr::getxattr(&inode_ro, b"user.k", &cred(0, 0, &[])).unwrap(),
        Some(b"v".to_vec())
    );
    // trusted.* 需要 CAP_SYS_ADMIN
    assert_eq!(
        xattr::setxattr(&inode, b"trusted.t", b"v", 0, &cred(0, 0, &[])),
        Err(VfsError::OperationNotPermitted)
    );
    assert!(
        xattr::setxattr(
            &inode,
            b"trusted.t",
            b"v",
            0,
            &cred(0, 0, &[Capability::SysAdmin])
        )
        .is_ok()
    );
    // XATTR_CREATE/REPLACE
    assert_eq!(
        xattr::setxattr(
            &inode,
            b"user.k",
            b"v2",
            xattr::XATTR_CREATE,
            &cred(0, 0, &[])
        ),
        Err(VfsError::AlreadyExists)
    );
    assert_eq!(
        xattr::setxattr(
            &inode,
            b"user.missing",
            b"v",
            xattr::XATTR_REPLACE,
            &cred(0, 0, &[])
        ),
        Err(VfsError::NoData)
    );
    // listxattr 包含已设置的属性
    let names = xattr::listxattr(&inode).unwrap();
    assert!(names.contains(&b"user.k".to_vec()));
    assert!(names.contains(&b"trusted.t".to_vec()));
    // removexattr
    assert!(xattr::removexattr(&inode, b"user.k", &cred(0, 0, &[])).is_ok());
    assert_eq!(
        xattr::getxattr(&inode, b"user.k", &cred(0, 0, &[])).unwrap(),
        None
    );
    // 值超限 → E2BIG（FileTooLarge 映射）
    let big = vec![0u8; xattr::XATTR_SIZE_MAX + 1];
    assert_eq!(
        xattr::setxattr(&inode, b"user.big", &big, 0, &cred(0, 0, &[])),
        Err(VfsError::FileTooLarge)
    );
}
