//! V1 使用的强类型 ID、owner 和 opaque handle。

macro_rules! runtime_id {
    ($name:ident, $doc:literal) => {
        #[repr(transparent)]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
        #[doc = $doc]
        pub struct $name(pub u64);

        impl $name {
            /// 保留的无效值。
            pub const INVALID: Self = Self(0);

            /// 从非零原始编号构造 ID。
            pub const fn new(raw: u64) -> Option<Self> {
                if raw == 0 { None } else { Some(Self(raw)) }
            }

            /// 从协议整数构造 ID，不做有效性检查。
            pub const fn from_raw(raw: u64) -> Self {
                Self(raw)
            }

            /// 返回协议中的原始整数。
            pub const fn raw(self) -> u64 {
                self.0
            }

            /// 判断该 ID 是否不是保留零值。
            pub const fn is_valid(self) -> bool {
                self.0 != 0
            }
        }
    };
}

runtime_id!(
    LanguageId,
    "语言实现的稳定运行时编号。该值不是地址，也不保证跨启动稳定。"
);
runtime_id!(BackendId, "语言后端的稳定运行时编号。编号零表示没有后端。");
runtime_id!(
    InstanceId,
    "语言后端实例的稳定运行时编号。编号零表示没有实例。"
);
runtime_id!(RequestId, "语言运行时请求的关联编号。编号零保留为无请求。");

/// 一次语言运行时调用的 owner cell 和 generation 快照。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LanguageOwnerV1 {
    /// 拥有对象或请求的 ELM cell 编号，零无效。
    pub cell_id: u64,
    /// 拥有对象或请求的 ELM generation，零无效。
    pub generation: u64,
}

impl LanguageOwnerV1 {
    /// 构造一个 owner 快照。
    pub const fn new(cell_id: u64, generation: u64) -> Self {
        Self {
            cell_id,
            generation,
        }
    }

    /// 判断两个字段都满足 V1 的非零约束。
    pub const fn is_valid(self) -> bool {
        self.cell_id != 0 && self.generation != 0
    }
}

/// 语言运行时的 opaque 句柄。
///
/// 低 32 位是槽位，高 32 位是槽位 generation；任一字段为零都表示无效。运行时必须在
/// 句柄表中另行保存 owner cell/generation，并在每次使用时比较 owner。该值不是地址。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct LanguageHandle {
    /// 句柄槽位，零保留为无效值。
    pub slot: u32,
    /// 句柄槽位 generation，零保留为无效值。
    pub generation: u32,
}

impl LanguageHandle {
    /// 无效句柄。
    pub const INVALID: Self = Self {
        slot: 0,
        generation: 0,
    };

    /// 从槽位和 generation 构造句柄；任一字段为零都会失败。
    pub const fn new(slot: u32, generation: u32) -> Option<Self> {
        if slot == 0 || generation == 0 {
            None
        } else {
            Some(Self { slot, generation })
        }
    }

    /// 从稳定的 64 位编码解码句柄。
    pub const fn from_raw(raw: u64) -> Self {
        Self {
            slot: raw as u32,
            generation: (raw >> 32) as u32,
        }
    }

    /// 把句柄编码为稳定的 64 位值。
    pub const fn raw(self) -> u64 {
        self.slot as u64 | ((self.generation as u64) << 32)
    }

    /// 判断槽位和 generation 是否都有效。
    pub const fn is_valid(self) -> bool {
        self.slot != 0 && self.generation != 0
    }
}

/// 在 wire 中把句柄和 owner 一起传递时使用的固定记录。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LanguageOwnedHandleV1 {
    /// opaque 句柄。
    pub handle: LanguageHandle,
    /// 句柄所属的 cell。
    pub owner_cell_id: u64,
    /// 句柄所属的 cell generation。
    pub owner_generation: u64,
}

impl LanguageOwnedHandleV1 {
    /// 构造一个带 owner 绑定的句柄记录。
    pub const fn new(handle: LanguageHandle, owner: LanguageOwnerV1) -> Self {
        Self {
            handle,
            owner_cell_id: owner.cell_id,
            owner_generation: owner.generation,
        }
    }

    /// 判断句柄和 owner 均有效。
    pub const fn is_valid(self) -> bool {
        self.handle.is_valid() && self.owner_cell_id != 0 && self.owner_generation != 0
    }

    /// 返回记录中的 owner 快照。
    pub const fn owner(self) -> LanguageOwnerV1 {
        LanguageOwnerV1::new(self.owner_cell_id, self.owner_generation)
    }

    /// 验证句柄有效且 owner 与受信任调用上下文一致。
    pub const fn validate_for(self, expected: LanguageOwnerV1) -> crate::ValidationResult {
        if !self.handle.is_valid() {
            return Err(crate::LanguageValidationError::Handle);
        }
        crate::validation::validate_expected_owner(
            self.owner_cell_id,
            self.owner_generation,
            expected.cell_id,
            expected.generation,
        )
    }
}
