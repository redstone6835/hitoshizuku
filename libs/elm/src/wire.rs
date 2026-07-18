//! 不依赖堆分配的 ELM 公共判别类型。
//!
//! 这些枚举使用显式 `repr(u32)`，可直接嵌入固定 ABI 结构。解析不可信输入时必须先调用
//! 对应 `from_raw`，不能对任意整数执行 `transmute`。

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
/// 一个补缀点允许的处理器组合模式。
pub enum ElmMixinMode {
    /// 多个处理器按优先级和稳定次序形成可修改控制链。
    Chain = 1,
    /// 处理器只能观察结果，不应改变主控制流。
    Observer = 2,
    /// 同一时间只允许一个有效处理器附着到该点。
    Exclusive = 3,
}

impl ElmMixinMode {
    /// 从稳定判别值解析模式；未知值返回 `None`。
    pub const fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            1 => Some(Self::Chain),
            2 => Some(Self::Observer),
            3 => Some(Self::Exclusive),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
/// provider 或 Nexus 端口的可见范围。
pub enum ElmPortAccessPolicy {
    /// 仅端口所有者内部使用，不能由其他 cell 建立普通绑定。
    Internal = 1,
    /// 可被满足契约、策略和作用域约束的其他 cell 绑定。
    Public = 2,
    /// 只允许已建立 extension 关系的 cell 使用。
    ExtensionOnly = 3,
}

impl ElmPortAccessPolicy {
    /// 从稳定判别值解析访问策略；未知值返回 `None`。
    pub const fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            1 => Some(Self::Internal),
            2 => Some(Self::Public),
            3 => Some(Self::ExtensionOnly),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
/// 端口在契约数据流中的方向。
pub enum FlowDirection {
    /// 产生数据或事件，通常与 sink 端绑定。
    Source = 1,
    /// 消费数据或事件，通常与 source 端绑定。
    Sink = 2,
    /// 双向交换请求和数据。
    Duplex = 3,
    /// 以调用/回复为主的控制型端口。
    Control = 4,
}

impl FlowDirection {
    /// 从稳定判别值解析方向；未知值返回 `None`。
    pub const fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            1 => Some(Self::Source),
            2 => Some(Self::Sink),
            3 => Some(Self::Duplex),
            4 => Some(Self::Control),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
/// 一个端口接受绑定和分发调用的方式。
pub enum FlowMode {
    /// 端口只能存在一个有效对端或独占调用者。
    Exclusive = 1,
    /// 多个绑定可以共享端口，调用顺序不提供额外保证。
    Shared = 2,
    /// 运行时必须保持该端口调用的规定顺序。
    Ordered = 3,
    /// 调用沿多个阶段依次处理，适合可组合处理链。
    Pipeline = 4,
    /// 一次发布分发给全部符合条件的绑定。
    Broadcast = 5,
}

impl FlowMode {
    /// 从稳定判别值解析模式；未知值返回 `None`。
    pub const fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            1 => Some(Self::Exclusive),
            2 => Some(Self::Shared),
            3 => Some(Self::Ordered),
            4 => Some(Self::Pipeline),
            5 => Some(Self::Broadcast),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
/// 发起策略检查或管理操作的主体种类。
pub enum ElmPrincipalKind {
    /// 内核内部受信任控制路径。
    Kernel = 1,
    /// 通过系统调用进入、已通过管理员授权检查的用户态主体。
    UserAdmin = 2,
    /// 由 cell id 与 generation 共同标识的 ELM 主体。
    ElmCell = 3,
}
