//! VMA 权限与属性位图。
//!
//! 用一个裸 `u32` 自定义的 bitfield，避免拉 `bitflags` 依赖。位定义与 Linux
//! `PROT_*` / `MAP_*` 对齐，便于 syscall 层一对一翻译。常量单独暴露，`has` /
//! `with` / `without` / `contains_all` 四个方法覆盖常见操作；按位或 / 与 /
//! 非这些"库级语法糖"不走 trait，避免 debug 打印时歧义。

/// VMA 属性位。保留低 16 位做权限，其余给区域类型标志。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VmFlags(u32);

impl VmFlags {
    /// 空集：既不可读、不可写、不可执行。`contains_all` 检查时总为假。
    pub const EMPTY: Self = Self(0);

    // ── 权限位（Linux PROT_* 对齐） ─────────────────────────────────────────
    pub const READ: u32 = 1 << 0;
    pub const WRITE: u32 = 1 << 1;
    pub const EXEC: u32 = 1 << 2;
    pub const PERM_MASK: u32 = Self::READ | Self::WRITE | Self::EXEC;

    // ── 区域类型标志 ────────────────────────────────────────────────────────
    /// 用户态可访问。内核栈 / 内核映射不置。
    pub const USER: u32 = 1 << 8;
    /// 共享：`fork` 时共享物理页（对应 Linux `MAP_SHARED`）。否则 fork 时
    /// 深拷贝物理页。
    pub const SHARED: u32 = 1 << 9;
    /// 栈区域：向低地址生长，触栈底时 `handle_fault` 需扩 VMA。
    pub const GROWS_DOWN: u32 = 1 << 10;
    /// 匿名映射：无 file backing。对应 Linux `MAP_ANONYMOUS`。
    pub const ANON: u32 = 1 << 11;
    /// 已被 mlock/munlockall 状态标记为常驻。当前无换出回收时仅作为策略位。
    pub const LOCKED: u32 = 1 << 12;

    /// 从裸位构造。不做语义校验；校验留给 VmSpace 层。
    pub const fn from_bits(bits: u32) -> Self {
        Self(bits)
    }

    pub const fn bits(self) -> u32 {
        self.0
    }

    pub const fn permissions(self) -> Self {
        Self(self.0 & Self::PERM_MASK)
    }

    pub const fn with_permissions(self, permissions: Self) -> Self {
        Self((self.0 & !Self::PERM_MASK) | (permissions.0 & Self::PERM_MASK))
    }

    /// 是否含某一位（接受单 flag 或组合）。
    pub const fn has(self, flag: u32) -> bool {
        (self.0 & flag) == flag && flag != 0
    }

    pub const fn with(self, flag: u32) -> Self {
        Self(self.0 | flag)
    }

    pub const fn without(self, flag: u32) -> Self {
        Self(self.0 & !flag)
    }

    /// 是否包含所有给定位。空 mask 返回 false，避免把“未要求任何位”误判为匹配。
    pub const fn contains_all(self, flags: u32) -> bool {
        flags != 0 && (self.0 & flags) == flags
    }
}

impl Default for VmFlags {
    fn default() -> Self {
        Self::EMPTY
    }
}
