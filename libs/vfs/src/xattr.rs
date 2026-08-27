//! 扩展属性（xattr）的 VFS 层语义：命名空间解析、权限模型与 POSIX ACL。
//!
//! 命名空间（Linux 语义）：
//! - `user.*`：任意文件，set 需文件写权限（目录另受 sticky 位规则约束），
//!   get 需文件读权限；
//! - `trusted.*`：需 `CAP_SYS_ADMIN`；
//! - `security.*`：需 `CAP_SYS_ADMIN`（无 LSM 时保守策略）；
//! - `system.posix_acl_access` / `system.posix_acl_default`：POSIX ACL，
//!   set 需文件属主或 `CAP_FOWNER`，get 无额外限制；
//! - 其他 `system.*`：`EOPNOTSUPP`；未知前缀：`EOPNOTSUPP`。
//!
//! 存储后端由 `InodeOps` 的 xattr 方法提供（extfs 用 ext4 兼容 xattr 块、
//! tmpfs 用内存表）；本模块负责语义层，避免各文件系统重复实现。

use alloc::vec;
use alloc::vec::Vec;

use crate::vfs::cred::{Capability, Credentials};
use crate::vfs::error::{VfsError, VfsResult};
use crate::vfs::inode::Inode;
use crate::vfs::stat::FileMode;
use crate::vfs::stat::FileType;

/// xattr 名称最大长度（Linux `XATTR_NAME_MAX`）。
pub const XATTR_NAME_MAX: usize = 255;
/// xattr 值最大长度（Linux `XATTR_SIZE_MAX`）。
pub const XATTR_SIZE_MAX: usize = 65536;
/// `setxattr` 的 `XATTR_CREATE`（仅新建）。
pub const XATTR_CREATE: u32 = 0x1;
/// `setxattr` 的 `XATTR_REPLACE`（仅替换）。
pub const XATTR_REPLACE: u32 = 0x2;

const PREFIX_USER: &[u8] = b"user.";
const PREFIX_TRUSTED: &[u8] = b"trusted.";
const PREFIX_SECURITY: &[u8] = b"security.";
const PREFIX_SYSTEM: &[u8] = b"system.";
const ACL_ACCESS_NAME: &[u8] = b"system.posix_acl_access";
const ACL_DEFAULT_NAME: &[u8] = b"system.posix_acl_default";

/// 解析后的 xattr 命名空间。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum XattrNamespace {
    User,
    Trusted,
    Security,
    PosixAclAccess,
    PosixAclDefault,
    /// `system.*` 下未识别的名称。
    OtherSystem,
}

/// 解析并校验 xattr 名称。
pub fn parse_name(name: &[u8]) -> VfsResult<XattrNamespace> {
    if name.is_empty() || name.len() > XATTR_NAME_MAX {
        return Err(VfsError::InvalidArgument);
    }
    if name.contains(&b'/') {
        return Err(VfsError::InvalidArgument);
    }
    if let Some(rest) = name.strip_prefix(PREFIX_USER) {
        check_suffix(rest)?;
        return Ok(XattrNamespace::User);
    }
    if let Some(rest) = name.strip_prefix(PREFIX_TRUSTED) {
        check_suffix(rest)?;
        return Ok(XattrNamespace::Trusted);
    }
    if let Some(rest) = name.strip_prefix(PREFIX_SECURITY) {
        check_suffix(rest)?;
        return Ok(XattrNamespace::Security);
    }
    if let Some(rest) = name.strip_prefix(PREFIX_SYSTEM) {
        if rest.is_empty() {
            return Err(VfsError::InvalidArgument);
        }
        if name == ACL_ACCESS_NAME {
            return Ok(XattrNamespace::PosixAclAccess);
        }
        if name == ACL_DEFAULT_NAME {
            return Ok(XattrNamespace::PosixAclDefault);
        }
        return Ok(XattrNamespace::OtherSystem);
    }
    // 无已知前缀：Linux 返回 EOPNOTSUPP（未知命名空间）。
    Err(VfsError::NotSupported)
}

fn check_suffix(rest: &[u8]) -> VfsResult<()> {
    if rest.is_empty() || rest.starts_with(b".") {
        return Err(VfsError::InvalidArgument);
    }
    Ok(())
}

/// 校验 xattr 值长度（`E2BIG` 语义）。
fn check_value_len(value: &[u8]) -> VfsResult<()> {
    if value.len() > XATTR_SIZE_MAX {
        return Err(VfsError::FileTooLarge);
    }
    Ok(())
}

/// 目录 sticky 位规则：非属主（文件或目录）且无 `CAP_FOWNER` 时拒绝
/// 在目录内修改他人文件（Linux `xattr_permission` 语义）。
fn check_sticky(inode: &Inode, cred: &Credentials) -> VfsResult<()> {
    if !inode.meta_snapshot().mode.has(FileMode::ISVTX) {
        return Ok(());
    }
    let meta = inode.meta_snapshot();
    let is_owner = cred.is_owner(meta.uid);
    if is_owner || cred.has_cap(Capability::FOwner) {
        return Ok(());
    }
    Err(VfsError::OperationNotPermitted)
}

/// `getxattr`：读取命名属性。不存在返回 `None`（`ENODATA` 由调用方映射）。
pub fn getxattr(inode: &Inode, name: &[u8], cred: &Credentials) -> VfsResult<Option<Vec<u8>>> {
    let ns = parse_name(name)?;
    match ns {
        XattrNamespace::User => {
            let meta = inode.meta_snapshot();
            if !cred.can_read(meta.uid, meta.gid, meta.mode) {
                return Err(VfsError::PermissionDenied);
            }
        }
        XattrNamespace::Trusted | XattrNamespace::Security => {
            if !cred.has_cap(Capability::SysAdmin) {
                return Err(VfsError::OperationNotPermitted);
            }
        }
        XattrNamespace::PosixAclAccess | XattrNamespace::PosixAclDefault => {}
        XattrNamespace::OtherSystem => return Err(VfsError::NotSupported),
    }
    inode.ops.getxattr(name)
}

/// `setxattr`：设置命名属性（`XATTR_CREATE`/`XATTR_REPLACE` 由后端处理，
/// 本层做权限与长度校验）。
pub fn setxattr(
    inode: &Inode,
    name: &[u8],
    value: &[u8],
    flags: u32,
    cred: &Credentials,
) -> VfsResult<()> {
    if flags & !(XATTR_CREATE | XATTR_REPLACE) != 0 {
        return Err(VfsError::InvalidArgument);
    }
    let ns = parse_name(name)?;
    check_value_len(value)?;
    let parsed_acl = match ns {
        XattrNamespace::User => {
            let meta = inode.meta_snapshot();
            if !cred.can_write(meta.uid, meta.gid, meta.mode) {
                return Err(VfsError::PermissionDenied);
            }
            if inode.kind == FileType::Directory {
                check_sticky(inode, cred)?;
            }
            None
        }
        XattrNamespace::Trusted | XattrNamespace::Security => {
            if !cred.has_cap(Capability::SysAdmin) {
                return Err(VfsError::OperationNotPermitted);
            }
            None
        }
        XattrNamespace::PosixAclAccess | XattrNamespace::PosixAclDefault => {
            let meta = inode.meta_snapshot();
            if !cred.is_owner(meta.uid) && !cred.has_cap(Capability::FOwner) {
                return Err(VfsError::OperationNotPermitted);
            }
            // 后端只负责持久化。必须在进入后端前解析并完整校验 ACL，避免
            // tmpfs/extfs 保存一个之后会改变 DAC 决策的畸形属性。
            Some(crate::acl::parse(value)?)
        }
        XattrNamespace::OtherSystem => return Err(VfsError::NotSupported),
    };
    inode.ops.setxattr(name, value, flags)?;
    // 有 xattr 后置位快速路径标记（ACL 强制依赖）。
    inode.mark_has_xattrs();
    if ns == XattrNamespace::PosixAclAccess {
        // mode 组位 ↔ ACL mask 双向同步（Linux：setfacl 后 mode 组位 = mask）。
        if let Some(mask_perm) = parsed_acl.as_ref().and_then(crate::acl::mask_to_mode_group) {
            let meta = inode.meta_snapshot();
            let new_mode = crate::vfs::stat::FileMode((meta.mode.0 & !0o070) | (mask_perm << 3));
            let _ = inode.ops.chmod(inode, new_mode);
        }
    }
    Ok(())
}

/// `listxattr`：列出全部属性名。
pub fn listxattr(inode: &Inode) -> VfsResult<Vec<Vec<u8>>> {
    inode.ops.listxattr()
}

/// `removexattr`：删除命名属性。
pub fn removexattr(inode: &Inode, name: &[u8], cred: &Credentials) -> VfsResult<()> {
    let ns = parse_name(name)?;
    match ns {
        XattrNamespace::User => {
            let meta = inode.meta_snapshot();
            if !cred.can_write(meta.uid, meta.gid, meta.mode) {
                return Err(VfsError::PermissionDenied);
            }
        }
        XattrNamespace::Trusted | XattrNamespace::Security => {
            if !cred.has_cap(Capability::SysAdmin) {
                return Err(VfsError::OperationNotPermitted);
            }
        }
        XattrNamespace::PosixAclAccess | XattrNamespace::PosixAclDefault => {
            let meta = inode.meta_snapshot();
            if !cred.is_owner(meta.uid) && !cred.has_cap(Capability::FOwner) {
                return Err(VfsError::OperationNotPermitted);
            }
        }
        XattrNamespace::OtherSystem => return Err(VfsError::NotSupported),
    }
    inode.ops.removexattr(name)?;
    // 属性列表为空时复位快速路径标记。
    if inode.ops.listxattr().map(|l| l.is_empty()).unwrap_or(false) {
        inode.clear_has_xattrs();
    }
    Ok(())
}

/// 把 xattr 名编码为 Linux 用户态可见的 `name\0` 列表（listxattr 输出）。
pub fn encode_list(names: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for name in names {
        out.extend_from_slice(name);
        out.push(0);
    }
    out
}

/// 空属性名列表（供后端默认实现返回）。
pub(crate) fn empty_list() -> Vec<Vec<u8>> {
    vec![]
}
