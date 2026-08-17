//! POSIX ACL：Linux 二进制格式的解析/编码/校验与 DAC 强制。
//!
//! 磁盘格式（`system.posix_acl_access` / `system.posix_acl_default`）：
//!
//! ```text
//! u32 version = 0x0002
//! 条目 × N：{ u16 tag, u16 perm, u32 id }
//! ```
//!
//! tag：`ACL_USER_OBJ`(0x01) / `ACL_USER`(0x02) / `ACL_GROUP_OBJ`(0x04) /
//! `ACL_GROUP`(0x08) / `ACL_MASK`(0x10) / `ACL_OTHER`(0x20)。perm 为
//! rwx 三比特。校验规则与 Linux `posix_acl_valid` 一致：三类基本条目各一、
//! 命名条目必须排序且 id 唯一、出现命名条目时必须有 mask。
//!
//! 强制：`check_access` 在 inode 无 xattr（`has_xattrs` 快速路径）时直接走
//! mode 检查；有 ACL 时按 Linux `posix_acl_permission` 语义（owner → 命名
//! user → 组（mask 掩蔽）→ other）。

use alloc::vec;
use alloc::vec::Vec;

use crate::vfs::cred::{Capability, Credentials};
use crate::vfs::error::{VfsError, VfsResult};
use crate::vfs::inode::Inode;
use crate::vfs::stat::FileMode;

/// ACL 版本号（Linux `POSIX_ACL_XATTR_VERSION`）。
pub const ACL_VERSION: u32 = 0x0002;

pub const ACL_USER_OBJ: u16 = 0x01;
pub const ACL_USER: u16 = 0x02;
pub const ACL_GROUP_OBJ: u16 = 0x04;
pub const ACL_GROUP: u16 = 0x08;
pub const ACL_MASK: u16 = 0x10;
pub const ACL_OTHER: u16 = 0x20;

/// `system.posix_acl_access` 属性名。
pub const ACL_ACCESS_XATTR: &[u8] = b"system.posix_acl_access";
/// `system.posix_acl_default` 属性名。
pub const ACL_DEFAULT_XATTR: &[u8] = b"system.posix_acl_default";

/// 一条 ACL 条目（与磁盘布局同构）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AclEntry {
    pub tag: u16,
    pub perm: u16,
    pub id: u32,
}

/// 解析后的 POSIX ACL。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PosixAcl {
    pub entries: Vec<AclEntry>,
}

/// 需要检查的访问类型（与 mode 位对应）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AclCheckKind {
    Read,
    Write,
    /// 执行/搜索。`is_dir` 决定 `DAC_OVERRIDE` 能力是否可直接放行
    /// （与 `Credentials::check_permission` 的 `Exec { is_dir }` 一致）。
    Exec { is_dir: bool },
}

impl AclCheckKind {
    fn mode_bit(self, mode: FileMode) -> bool {
        match self {
            AclCheckKind::Read => mode.has(FileMode::IRUSR),
            AclCheckKind::Write => mode.has(FileMode::IWUSR),
            AclCheckKind::Exec { .. } => mode.has(FileMode::IXUSR),
        }
    }

    fn perm_bit(self) -> u16 {
        match self {
            AclCheckKind::Read => 4,
            AclCheckKind::Write => 2,
            AclCheckKind::Exec { .. } => 1,
        }
    }
}

/// 解析并校验 ACL 字节（版本、条目结构、排序与唯一性）。
pub fn parse(bytes: &[u8]) -> VfsResult<PosixAcl> {
    if bytes.len() < 4 || bytes.len() % 8 != 4 {
        return Err(VfsError::InvalidArgument);
    }
    let version = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
    if version != ACL_VERSION {
        return Err(VfsError::InvalidArgument);
    }
    let mut entries = Vec::new();
    for chunk in bytes[4..].chunks_exact(8) {
        let tag = u16::from_le_bytes(chunk[0..2].try_into().unwrap());
        let perm = u16::from_le_bytes(chunk[2..4].try_into().unwrap());
        let id = u32::from_le_bytes(chunk[4..8].try_into().unwrap());
        entries.push(AclEntry { tag, perm, id });
    }
    validate(&entries)?;
    Ok(PosixAcl { entries })
}

/// 校验条目集合（Linux `posix_acl_valid` 语义）。
pub fn validate(entries: &[AclEntry]) -> VfsResult<()> {
    let mut user_obj = false;
    let mut group_obj = false;
    let mut other = false;
    let mut mask = false;
    let mut named_user = false;
    let mut named_group = false;
    let mut prev_tag = 0u16;
    let mut prev_id = 0u32;
    for e in entries {
        if e.perm > 7 {
            return Err(VfsError::InvalidArgument);
        }
        match e.tag {
            ACL_USER_OBJ => {
                if user_obj {
                    return Err(VfsError::InvalidArgument);
                }
                user_obj = true;
            }
            ACL_USER => {
                named_user = true;
                if e.id == 0 {
                    return Err(VfsError::InvalidArgument);
                }
            }
            ACL_GROUP_OBJ => {
                if group_obj {
                    return Err(VfsError::InvalidArgument);
                }
                group_obj = true;
            }
            ACL_GROUP => {
                named_group = true;
                if e.id == 0 {
                    return Err(VfsError::InvalidArgument);
                }
            }
            ACL_MASK => {
                if mask {
                    return Err(VfsError::InvalidArgument);
                }
                mask = true;
            }
            ACL_OTHER => {
                if other {
                    return Err(VfsError::InvalidArgument);
                }
                other = true;
            }
            _ => return Err(VfsError::InvalidArgument),
        }
        // 严格升序（Linux 要求排序；同 tag 按 id 升序）。
        if e.tag < prev_tag || (e.tag == prev_tag && e.id <= prev_id) {
            return Err(VfsError::InvalidArgument);
        }
        prev_tag = e.tag;
        prev_id = e.id;
    }
    if !user_obj || !group_obj || !other {
        return Err(VfsError::InvalidArgument);
    }
    if (named_user || named_group) && !mask {
        return Err(VfsError::InvalidArgument);
    }
    Ok(())
}

/// 编码为磁盘字节。
pub fn encode(acl: &PosixAcl) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + acl.entries.len() * 8);
    out.extend_from_slice(&ACL_VERSION.to_le_bytes());
    for e in &acl.entries {
        out.extend_from_slice(&e.tag.to_le_bytes());
        out.extend_from_slice(&e.perm.to_le_bytes());
        out.extend_from_slice(&e.id.to_le_bytes());
    }
    out
}

/// 结合 ACL 的访问检查：能力绕过优先（Linux `generic_permission` 顺序），
/// inode 无 xattr 时走 mode 快速路径。
pub fn check_access(cred: &Credentials, inode: &Inode, kind: AclCheckKind) -> bool {
    let meta = inode.meta_snapshot();
    // 能力检查（与 `Credentials::check_permission` 的语义一致）。
    match kind {
        AclCheckKind::Read => {
            if cred.has_cap(Capability::DacReadSearch) || cred.has_cap(Capability::DacOverride) {
                return true;
            }
        }
        AclCheckKind::Write => {
            if cred.has_cap(Capability::DacOverride) {
                return true;
            }
        }
        AclCheckKind::Exec { is_dir } => {
            if cred.has_cap(Capability::DacOverride)
                && (is_dir || meta.mode.has_any(FileMode::ANY_EXEC))
            {
                return true;
            }
        }
    }
    if !inode.has_xattrs() {
        return mode_check(cred, &meta.uid, &meta.gid, meta.mode, kind);
    }
    match inode.ops.getxattr(ACL_ACCESS_XATTR) {
        Ok(Some(bytes)) => match parse(&bytes) {
            Ok(acl) => acl_check(cred, &acl, &meta.uid, &meta.gid, kind),
            // 存储的 ACL 在 setxattr 时已校验；损坏时保守回退 mode 检查。
            Err(_) => mode_check(cred, &meta.uid, &meta.gid, meta.mode, kind),
        },
        _ => mode_check(cred, &meta.uid, &meta.gid, meta.mode, kind),
    }
}

/// 纯 mode 检查（等价于 `Credentials::can_read/write/exec`，供 ACL 层回退）。
fn mode_check(
    cred: &Credentials,
    uid: &crate::vfs::cred::Uid,
    gid: &crate::vfs::cred::Gid,
    mode: FileMode,
    kind: AclCheckKind,
) -> bool {
    match kind {
        AclCheckKind::Read => cred.can_read(*uid, *gid, mode),
        AclCheckKind::Write => cred.can_write(*uid, *gid, mode),
        AclCheckKind::Exec { is_dir } => cred.can_exec(*uid, *gid, mode, is_dir),
    }
}

/// ACL 语义检查（Linux `posix_acl_permission`）。
fn acl_check(
    cred: &Credentials,
    acl: &PosixAcl,
    inode_uid: &crate::vfs::cred::Uid,
    inode_gid: &crate::vfs::cred::Gid,
    kind: AclCheckKind,
) -> bool {
    let bit = kind.perm_bit();
    let mut mask: Option<u16> = None;
    for e in &acl.entries {
        if e.tag == ACL_MASK {
            mask = Some(e.perm);
        }
    }
    let allow = |perm: u16| perm & bit != 0;

    // owner
    if cred.fsuid == *inode_uid {
        for e in &acl.entries {
            if e.tag == ACL_USER_OBJ {
                return allow(e.perm);
            }
        }
        return false;
    }
    // 命名 user
    for e in &acl.entries {
        if e.tag == ACL_USER && e.id == cred.fsuid.0 {
            return allow(e.perm);
        }
    }
    // 组：fsgid 或补充组命中文件组（GROUP_OBJ）或命名组条目（GROUP）时，
    // 按 mask 掩蔽逐条检查（Linux `posix_acl_permission` 语义）。
    let mut group_hit = false;
    for e in &acl.entries {
        let matches = match e.tag {
            ACL_GROUP_OBJ => {
                cred.fsgid == *inode_gid || cred.groups.contains(inode_gid)
            }
            ACL_GROUP => {
                cred.fsgid == crate::vfs::cred::Gid(e.id)
                    || cred.groups.contains(&crate::vfs::cred::Gid(e.id))
            }
            _ => false,
        };
        if !matches {
            continue;
        }
        group_hit = true;
        let effective = match mask {
            Some(m) => e.perm & m,
            None => e.perm,
        };
        if allow(effective) {
            return true;
        }
    }
    if group_hit {
        return false;
    }
    // other
    for e in &acl.entries {
        if e.tag == ACL_OTHER {
            return allow(e.perm);
        }
    }
    false
}

/// `posix_acl_create`：由父目录 default ACL 与创建 mode 派生子文件 ACL 与
/// 调整后的 mode（Linux 语义：USER_OBJ/OTHER 取 mode 对应位；MASK 取 mode
/// 组位；返回值 `(child_acl, adjusted_mode)`）。
pub fn create(default: &PosixAcl, mode: FileMode) -> VfsResult<(PosixAcl, FileMode)> {
    let mut entries = default.entries.clone();
    // 重写基本条目：USER_OBJ 与 OTHER 按 mode；MASK（若存在）按 mode 组位。
    let owner_perm = ((mode.0 >> 6) & 7) as u16;
    let group_perm = ((mode.0 >> 3) & 7) as u16;
    let other_perm = (mode.0 & 7) as u16;
    let mut has_mask = false;
    for e in entries.iter_mut() {
        match e.tag {
            ACL_USER_OBJ => e.perm = owner_perm,
            ACL_OTHER => e.perm = other_perm,
            ACL_MASK => {
                e.perm = group_perm;
                has_mask = true;
            }
            _ => {}
        }
    }
    // 调整后的 mode：组位 = mask（有 mask 时），否则保持。
    let adjusted = if has_mask {
        FileMode((mode.0 & !0o070) | (group_perm << 3))
    } else {
        mode
    };
    validate(&entries)?;
    Ok((PosixAcl { entries }, adjusted))
}

/// 由 ACL 的 MASK 条目推导 mode 组位（chmod 与 setfacl 双向同步用）。
pub fn mask_to_mode_group(acl: &PosixAcl) -> Option<u16> {
    for e in &acl.entries {
        if e.tag == ACL_MASK {
            return Some(e.perm);
        }
    }
    None
}

/// 判断凭据是否允许设置 ACL（属主或 CAP_FOWNER）。
pub fn can_set_acl(cred: &Credentials, inode_uid: &crate::vfs::cred::Uid) -> bool {
    cred.is_owner(*inode_uid) || cred.has_cap(Capability::FOwner)
}
