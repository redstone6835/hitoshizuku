#![no_std]
#![warn(missing_docs)]

//! 内核直接符号目录的中立契约。
//!
//! 本 crate 只定义链接期描述符、能力组和导出 attribute，不依赖 ELM 运行时，也不承载
//! 任何子系统实现。常驻内核 crate 把经过审核的函数或静态对象放入
//! `.elm.kernel_symbols` 链接区；装载器在执行模块代码前按名称、契约、版本和 Rust ABI
//! 摘要解析地址。地址写入完成后，调用路径就是普通 Rust 间接调用，不经过 elm-mgr、
//! provider 或命名空间函数表。

use core::any::type_name;
use core::fmt;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

include!(concat!(env!("OUT_DIR"), "/interface_source.rs"));

#[cfg(feature = "macros")]
pub use kernel_symbols_macros::export;

/// 内核 Mixin 站点描述符的固定魔数。
pub const KERNEL_MIXIN_SITE_DESCRIPTOR_MAGIC: u64 = u64::from_le_bytes(*b"KMSIT001");
/// 当前内核 Mixin 站点描述符 ABI 版本。
pub const KERNEL_MIXIN_SITE_DESCRIPTOR_ABI_V1: u16 = 1;
/// 内核 Mixin 调用帧的固定魔数。
pub const KERNEL_MIXIN_FRAME_MAGIC: u64 = u64::from_le_bytes(*b"KMFRM001");
/// 当前内核 Mixin 调用帧 ABI 版本。
pub const KERNEL_MIXIN_FRAME_ABI_V1: u16 = 1;
/// 一个可织入函数允许暴露的最大参数数量。
pub const KERNEL_MIXIN_MAX_ARGUMENTS: usize = 64;

/// 函数入口站点。
pub const KERNEL_MIXIN_SITE_HEAD: u16 = 1;
/// 函数统一返回站点。
pub const KERNEL_MIXIN_SITE_RETURN: u16 = 2;
/// 源码可见调用执行前站点。
pub const KERNEL_MIXIN_SITE_CALL_BEFORE: u16 = 3;
/// 源码可见调用执行后站点。
pub const KERNEL_MIXIN_SITE_CALL_AFTER: u16 = 4;
/// 局部变量生命周期站点。
pub const KERNEL_MIXIN_SITE_LOCAL: u16 = 5;
/// 字段访问站点。
pub const KERNEL_MIXIN_SITE_FIELD: u16 = 6;

/// 该帧已经由原函数或覆盖处理器产生返回值。
pub const KERNEL_MIXIN_FRAME_RESULT_READY: u32 = 1 << 0;
/// 入口注入请求跳过后续处理器和原函数。
pub const KERNEL_MIXIN_FRAME_CANCELLED: u32 = 1 << 1;
/// 当前处理器请求停止同一阶段的后续普通处理器。
pub const KERNEL_MIXIN_FRAME_STOP: u32 = 1 << 2;
/// 当前处理器或 continuation 发生故障，调用侧应回退原逻辑。
pub const KERNEL_MIXIN_FRAME_FAULTED: u32 = 1 << 3;
/// 当前版本认识的全部帧控制位。
pub const KERNEL_MIXIN_FRAME_FLAGS_MASK: u32 = KERNEL_MIXIN_FRAME_RESULT_READY
    | KERNEL_MIXIN_FRAME_CANCELLED
    | KERNEL_MIXIN_FRAME_STOP
    | KERNEL_MIXIN_FRAME_FAULTED;

/// 当前站点没有活动处理链，调用侧应直接执行原逻辑。
pub const KERNEL_MIXIN_DISPATCH_UNHANDLED: i32 = 1;
/// 站点处理链执行成功。
pub const KERNEL_MIXIN_DISPATCH_OK: i32 = 0;
/// 站点处理链或调用帧无效。
pub const KERNEL_MIXIN_DISPATCH_INVALID: i32 = -1;
/// 借用槽只允许共享读取。
pub const KERNEL_MIXIN_VALUE_READ_ONLY: u32 = 1 << 0;
/// 借用槽指向尚未初始化的结果存储，只允许通过写入接口完成初始化。
pub const KERNEL_MIXIN_VALUE_UNINITIALIZED: u32 = 1 << 1;
/// 当前版本认识的全部借用槽标志。
pub const KERNEL_MIXIN_VALUE_FLAGS_MASK: u32 =
    KERNEL_MIXIN_VALUE_READ_ONLY | KERNEL_MIXIN_VALUE_UNINITIALIZED;

/// 普通注入处理器；运行时在处理器成功返回后自动继续后续处理链。
pub const KERNEL_MIXIN_HANDLER_INJECT: u16 = 1;
/// 参数修改处理器；只允许挂接到函数入口或调用前站点。
pub const KERNEL_MIXIN_HANDLER_MODIFY_ARGUMENT: u16 = 2;
/// 返回值修改处理器；只允许挂接到函数返回或调用后站点。
pub const KERNEL_MIXIN_HANDLER_MODIFY_RETURN: u16 = 3;
/// 局部变量修改处理器；只允许挂接到局部变量站点。
pub const KERNEL_MIXIN_HANDLER_MODIFY_LOCAL: u16 = 4;
/// 调用重定向处理器；处理器自行决定是否调用下一处理器或原操作。
pub const KERNEL_MIXIN_HANDLER_REDIRECT: u16 = 5;
/// 操作包装处理器；处理器通过 continuation 包围后续处理链。
pub const KERNEL_MIXIN_HANDLER_WRAP_OPERATION: u16 = 6;
/// 函数覆盖处理器；处理器通过 continuation 组成可回退的覆盖链。
pub const KERNEL_MIXIN_HANDLER_OVERWRITE: u16 = 7;

/// 处理器由运行时自动继续处理链。
pub const KERNEL_MIXIN_HANDLER_FLAG_AUTO_CONTINUE: u16 = 1 << 0;
/// 处理器拥有调用 continuation 的权限。
pub const KERNEL_MIXIN_HANDLER_FLAG_CONTINUATION: u16 = 1 << 1;
/// 当前版本认识的全部处理器标志。
pub const KERNEL_MIXIN_HANDLER_FLAGS_MASK: u16 =
    KERNEL_MIXIN_HANDLER_FLAG_AUTO_CONTINUE | KERNEL_MIXIN_HANDLER_FLAG_CONTINUATION;
/// 所有动态内核 Mixin trampoline 使用的规范 Rust ABI 字符串。
pub const KERNEL_MIXIN_HANDLER_RUST_ABI_V1: &str =
    "unsafeextern\"C\"fn(*mutkernel_symbols::KernelMixinFrameV1)->i32";

/// 一个处理链 continuation 的固定调用约定。
pub type KernelMixinContinuationV1 = unsafe extern "C" fn(*mut (), *mut KernelMixinFrameV1) -> i32;

/// 动态 ELM 内核 Mixin 处理器的固定调用约定。
pub type KernelMixinHandlerV1 = unsafe extern "C" fn(*mut KernelMixinFrameV1) -> i32;

/// 类型擦除但携带完整 Rust 类型名称的借用槽。
///
/// 槽本身只在一次同步调用的栈帧中存活。处理器必须先使用 [`Self::is`] 校验类型，再把
/// `pointer` 转换成相应 Rust 引用；任何借用都不得逃逸本次 Mixin 调用。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct KernelMixinValueV1 {
    /// 被借用值的地址。
    pub pointer: *mut (),
    /// `core::any::type_name::<T>()` 返回字符串的首地址。
    pub type_name_pointer: *const u8,
    /// 类型名称字节长度。
    pub type_name_len: u32,
    /// 当前必须为零。
    pub flags: u32,
}

impl KernelMixinValueV1 {
    /// 构造一个可变借用槽。
    pub fn from_mut<T>(value: &mut T) -> Self {
        let name = type_name::<T>();
        Self {
            pointer: core::ptr::from_mut(value).cast(),
            type_name_pointer: name.as_ptr(),
            type_name_len: name.len() as u32,
            flags: 0,
        }
    }

    /// 构造一个尚未初始化、仅允许处理器写入的结果槽。
    pub fn from_uninit<T>(value: &mut MaybeUninit<T>) -> Self {
        let name = type_name::<T>();
        Self {
            pointer: value.as_mut_ptr().cast(),
            type_name_pointer: name.as_ptr(),
            type_name_len: name.len() as u32,
            flags: KERNEL_MIXIN_VALUE_UNINITIALIZED,
        }
    }

    /// 从已由调用方验证的对象地址构造借用槽。
    ///
    /// # Safety
    ///
    /// `pointer` 必须指向一个在整个同步调用期间存活的 `T`；只读槽不得通过其它方式转换成
    /// 可变借用。
    pub unsafe fn from_raw<T>(pointer: *mut T, read_only: bool) -> Self {
        let name = type_name::<T>();
        Self {
            pointer: pointer.cast(),
            type_name_pointer: name.as_ptr(),
            type_name_len: name.len() as u32,
            flags: if read_only {
                KERNEL_MIXIN_VALUE_READ_ONLY
            } else {
                0
            },
        }
    }

    /// 返回槽中记录的类型是否与 `T` 完全一致。
    pub fn is<T>(&self) -> bool {
        let expected = type_name::<T>().as_bytes();
        if self.pointer.is_null()
            || self.type_name_pointer.is_null()
            || self.type_name_len as usize != expected.len()
            || self.flags & !KERNEL_MIXIN_VALUE_FLAGS_MASK != 0
        {
            return false;
        }
        // Safety: 类型名称来自具有静态存储期的 `type_name` 字符串，并已验证长度。
        let actual = unsafe {
            core::slice::from_raw_parts(self.type_name_pointer, self.type_name_len as usize)
        };
        actual == expected
    }

    /// 在完成类型校验后取得共享借用。
    ///
    /// # Safety
    ///
    /// 调用方必须保证槽仍属于当前活动调用帧，并且没有同时创建违反 Rust 别名规则的可变借用。
    pub unsafe fn cast_ref<T>(&self) -> Option<&T> {
        if self.flags & KERNEL_MIXIN_VALUE_UNINITIALIZED != 0 || !self.is::<T>() {
            return None;
        }
        // Safety: 上面的完整类型名称校验和调用方承担的帧生命周期保证转换有效。
        Some(unsafe { &*self.pointer.cast::<T>() })
    }

    /// 在完成类型校验后取得可变借用。
    ///
    /// # Safety
    ///
    /// 调用方必须独占该槽对应值，并保证借用不逃逸当前处理器调用。
    pub unsafe fn cast_mut<T>(&mut self) -> Option<&mut T> {
        if self.flags & (KERNEL_MIXIN_VALUE_READ_ONLY | KERNEL_MIXIN_VALUE_UNINITIALIZED) != 0
            || !self.is::<T>()
        {
            return None;
        }
        // Safety: 上面的完整类型名称校验和调用方承担的独占性保证转换有效。
        Some(unsafe { &mut *self.pointer.cast::<T>() })
    }

    /// 把值写入尚未初始化且类型完全匹配的结果槽。
    pub fn write<T>(&mut self, value: T) -> Result<(), T> {
        if self.flags & KERNEL_MIXIN_VALUE_READ_ONLY != 0
            || self.flags & KERNEL_MIXIN_VALUE_UNINITIALIZED == 0
            || !self.is::<T>()
        {
            return Err(value);
        }
        // Safety: 槽由 `from_uninit` 从有效 `MaybeUninit<T>` 创建，完整类型名称已经匹配。
        unsafe { self.pointer.cast::<T>().write(value) };
        self.flags &= !KERNEL_MIXIN_VALUE_UNINITIALIZED;
        Ok(())
    }

    /// 把由原逻辑直接写入的结果槽标记为已经初始化。
    ///
    /// # Safety
    ///
    /// 调用方必须已经使用完全匹配的 `T` 初始化 `pointer` 指向的存储。
    pub unsafe fn mark_initialized<T>(&mut self) -> bool {
        if self.flags & KERNEL_MIXIN_VALUE_UNINITIALIZED == 0 || !self.is::<T>() {
            return false;
        }
        self.flags &= !KERNEL_MIXIN_VALUE_UNINITIALIZED;
        true
    }
}

/// 内核函数执行一次 Mixin 处理链时使用的固定栈帧。
#[repr(C)]
pub struct KernelMixinFrameV1 {
    /// 固定魔数。
    pub magic: u64,
    /// ABI 版本。
    pub abi_version: u16,
    /// 当前结构完整长度。
    pub struct_size: u16,
    /// [`KERNEL_MIXIN_SITE_HEAD`] 等站点类别。
    pub site_kind: u16,
    /// 参数槽数量。
    pub argument_count: u16,
    /// `KERNEL_MIXIN_FRAME_*` 控制位。
    pub flags: u32,
    /// 处理器或运行时写入的状态码。
    pub status: i32,
    /// 当前必须为零。
    pub reserved0: u32,
    /// 参数槽数组。
    pub arguments: *mut KernelMixinValueV1,
    /// 返回值槽；无返回槽时为空。
    pub result: *mut KernelMixinValueV1,
    /// 调用下一优先级处理器的入口。
    pub next: Option<KernelMixinContinuationV1>,
    /// `next` 私有上下文。
    pub next_context: *mut (),
    /// 调用最终原逻辑的入口。
    pub original: Option<KernelMixinContinuationV1>,
    /// `original` 私有上下文。
    pub original_context: *mut (),
}

impl KernelMixinFrameV1 {
    /// 构造一个只借用调用方栈上参数和结果槽的帧。
    pub fn new(
        site_kind: u16,
        arguments: &mut [KernelMixinValueV1],
        result: Option<&mut KernelMixinValueV1>,
    ) -> Self {
        Self {
            magic: KERNEL_MIXIN_FRAME_MAGIC,
            abi_version: KERNEL_MIXIN_FRAME_ABI_V1,
            struct_size: core::mem::size_of::<Self>() as u16,
            site_kind,
            argument_count: arguments.len() as u16,
            flags: 0,
            status: 0,
            reserved0: 0,
            arguments: arguments.as_mut_ptr(),
            result: result.map_or(core::ptr::null_mut(), core::ptr::from_mut),
            next: None,
            next_context: core::ptr::null_mut(),
            original: None,
            original_context: core::ptr::null_mut(),
        }
    }

    /// 校验固定头部、槽数量和控制位。
    pub fn valid(&self) -> bool {
        self.magic == KERNEL_MIXIN_FRAME_MAGIC
            && self.abi_version == KERNEL_MIXIN_FRAME_ABI_V1
            && self.struct_size as usize == core::mem::size_of::<Self>()
            && matches!(
                self.site_kind,
                KERNEL_MIXIN_SITE_HEAD
                    | KERNEL_MIXIN_SITE_RETURN
                    | KERNEL_MIXIN_SITE_CALL_BEFORE
                    | KERNEL_MIXIN_SITE_CALL_AFTER
                    | KERNEL_MIXIN_SITE_LOCAL
                    | KERNEL_MIXIN_SITE_FIELD
            )
            && self.argument_count as usize <= KERNEL_MIXIN_MAX_ARGUMENTS
            && (self.argument_count == 0 || !self.arguments.is_null())
            && self.flags & !KERNEL_MIXIN_FRAME_FLAGS_MASK == 0
            && self.reserved0 == 0
    }

    /// 返回参数槽切片。
    ///
    /// # Safety
    ///
    /// 调用方必须保证帧仍处于目标函数建立的同步调用范围内。
    pub unsafe fn arguments_mut(&mut self) -> Option<&mut [KernelMixinValueV1]> {
        if !self.valid() {
            return None;
        }
        // Safety: `valid` 已验证空指针和数量，目标包装器保证数组存活。
        Some(unsafe {
            core::slice::from_raw_parts_mut(self.arguments, self.argument_count as usize)
        })
    }

    /// 按索引和完整类型名称取得一个参数的共享借用。
    ///
    /// # Safety
    ///
    /// 借用不得逃逸当前同步处理器调用，且调用方不得同时创建冲突的可变借用。
    pub unsafe fn argument<T>(&mut self, index: usize) -> Option<&T> {
        let arguments = unsafe { self.arguments_mut()? };
        // Safety: 槽仍属于当前帧，类型和初始化状态由 `cast_ref` 校验。
        unsafe { arguments.get(index)?.cast_ref::<T>() }
    }

    /// 按索引和完整类型名称取得一个参数的可变借用。
    ///
    /// # Safety
    ///
    /// 借用不得逃逸当前同步处理器调用，且调用方必须保证该参数当前可独占修改。
    pub unsafe fn argument_mut<T>(&mut self, index: usize) -> Option<&mut T> {
        let arguments = unsafe { self.arguments_mut()? };
        // Safety: 槽仍属于当前帧，只读、类型和初始化状态由 `cast_mut` 校验。
        unsafe { arguments.get_mut(index)?.cast_mut::<T>() }
    }

    /// 取得已经初始化的返回值共享借用。
    ///
    /// # Safety
    ///
    /// 借用不得逃逸当前同步处理器调用。
    pub unsafe fn result<T>(&mut self) -> Option<&T> {
        if self.flags & KERNEL_MIXIN_FRAME_RESULT_READY == 0 || self.result.is_null() {
            return None;
        }
        // Safety: 结果槽由目标包装器创建并在整个同步调用期间存活。
        unsafe { (*self.result).cast_ref::<T>() }
    }

    /// 取得已经初始化的返回值可变借用。
    ///
    /// # Safety
    ///
    /// 借用不得逃逸当前同步处理器调用，且调用方必须独占返回值。
    pub unsafe fn result_mut<T>(&mut self) -> Option<&mut T> {
        if self.flags & KERNEL_MIXIN_FRAME_RESULT_READY == 0 || self.result.is_null() {
            return None;
        }
        // Safety: 结果槽由目标包装器创建并在整个同步调用期间存活。
        unsafe { (*self.result).cast_mut::<T>() }
    }

    /// 写入提前返回或覆盖处理器产生的返回值。
    pub fn set_result<T>(&mut self, value: T) -> Result<(), T> {
        if self.result.is_null() || self.flags & KERNEL_MIXIN_FRAME_RESULT_READY != 0 {
            return Err(value);
        }
        // Safety: 结果槽指针由目标包装器创建并在当前同步调用期间保持有效。
        let result = unsafe { &mut *self.result };
        result.write(value)?;
        self.flags |= KERNEL_MIXIN_FRAME_RESULT_READY;
        Ok(())
    }

    /// 调用当前 continuation。
    ///
    /// # Safety
    ///
    /// 只能由当前处理器在同步调用期间调用，且同一个 continuation 最多调用一次。
    pub unsafe fn call_next(&mut self) -> i32 {
        let Some(next) = self.next.take() else {
            return KERNEL_MIXIN_DISPATCH_INVALID;
        };
        let context = core::mem::replace(&mut self.next_context, core::ptr::null_mut());
        // Safety: continuation 和上下文由内核运行时成对安装，并只在本次同步调用中有效。
        unsafe { next(context, self) }
    }

    /// 调用目标函数提供的最终原逻辑。
    ///
    /// # Safety
    ///
    /// 只能由处理链 continuation 调用一次；目标包装器保证上下文和结果槽有效。
    pub unsafe fn call_original(&mut self) -> i32 {
        let Some(original) = self.original.take() else {
            return KERNEL_MIXIN_DISPATCH_INVALID;
        };
        let context = core::mem::replace(&mut self.original_context, core::ptr::null_mut());
        // Safety: 原逻辑入口和上下文由目标包装器成对安装。
        unsafe { original(context, self) }
    }
}

/// 编译进内核镜像的一个可注入源码站点。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct KernelMixinSiteDescriptorV1 {
    /// 固定魔数。
    pub magic: u64,
    /// ABI 版本。
    pub abi_version: u16,
    /// 当前结构完整长度。
    pub struct_size: u16,
    /// 站点类别。
    pub kind: u16,
    /// 当前必须为零。
    pub flags: u16,
    /// 站点在所属目标函数中的稳定遍历序号。
    pub ordinal: u32,
    /// 产生该站点的内核接口源码摘要。
    pub source_hash: [u8; 32],
    /// 目标函数体规范 token 摘要。
    pub function_hash: [u8; 32],
    /// 站点身份完整摘要。
    pub site_hash: [u8; 32],
    /// 目标函数规范 Rust ABI 摘要。
    pub frame_abi_hash: [u8; 32],
    /// 稳定 API 路径。
    pub api_path: &'static str,
    /// 人类可读且可由构建工具重新解析的 selector。
    pub selector: &'static str,
    /// 该站点当前安装的不可变处理链；空指针表示完全绕过 Mixin 慢路径。
    pub route: &'static AtomicPtr<()>,
}

// Safety: 描述符只包含静态只读数据。
unsafe impl Sync for KernelMixinSiteDescriptorV1 {}

impl KernelMixinSiteDescriptorV1 {
    /// 构造一个完整站点描述符。
    pub const fn new(
        kind: u16,
        ordinal: u32,
        function_hash: [u8; 32],
        site_hash: [u8; 32],
        frame_abi_hash: [u8; 32],
        api_path: &'static str,
        selector: &'static str,
        route: &'static AtomicPtr<()>,
    ) -> Self {
        Self {
            magic: KERNEL_MIXIN_SITE_DESCRIPTOR_MAGIC,
            abi_version: KERNEL_MIXIN_SITE_DESCRIPTOR_ABI_V1,
            struct_size: core::mem::size_of::<Self>() as u16,
            kind,
            flags: 0,
            ordinal,
            source_hash: KERNEL_INTERFACE_SOURCE_SHA256,
            function_hash,
            site_hash,
            frame_abi_hash,
            api_path,
            selector,
            route,
        }
    }

    /// 校验站点描述符固定字段。
    pub fn valid(&self) -> bool {
        self.magic == KERNEL_MIXIN_SITE_DESCRIPTOR_MAGIC
            && self.abi_version == KERNEL_MIXIN_SITE_DESCRIPTOR_ABI_V1
            && self.struct_size as usize == core::mem::size_of::<Self>()
            && matches!(
                self.kind,
                KERNEL_MIXIN_SITE_HEAD
                    | KERNEL_MIXIN_SITE_RETURN
                    | KERNEL_MIXIN_SITE_CALL_BEFORE
                    | KERNEL_MIXIN_SITE_CALL_AFTER
                    | KERNEL_MIXIN_SITE_LOCAL
                    | KERNEL_MIXIN_SITE_FIELD
            )
            && self.flags == 0
            && self.source_hash != [0; 32]
            && self.function_hash != [0; 32]
            && self.site_hash != [0; 32]
            && self.frame_abi_hash != [0; 32]
            && valid_identifier(self.api_path, KERNEL_SYMBOL_NAME_MAX_LEN)
            && !self.selector.is_empty()
            && self.selector.len() <= KERNEL_SYMBOL_RUST_ABI_MAX_LEN
    }

    /// 返回该站点当前是否安装了处理链。
    ///
    /// 该入口供真正要读取路由的慢路径使用，Acquire 与发布者的 Release 配对。
    #[inline]
    pub fn has_handlers(&self) -> bool {
        !self.route.load(Ordering::Acquire).is_null()
    }

    /// 返回该站点是否可能安装了处理链，只用于决定是否进入慢路径。
    ///
    /// 调用方不得解引用这里观察到的指针。真正分发会再次以 Acquire 读取路由，
    /// 因此空路由快路径不需要在 RISC-V 上为一次提示判断执行内存屏障。
    #[inline(always)]
    pub fn has_handlers_hint(&self) -> bool {
        !self.route.load(Ordering::Relaxed).is_null()
    }
}

/// 常驻内核 Mixin 路由器的零分配分发入口。
pub type KernelMixinDispatchV1 =
    unsafe extern "C" fn(*const KernelMixinSiteDescriptorV1, *mut KernelMixinFrameV1) -> i32;

/// ELM 运行时向所有导出符号包装器安装的 Mixin 钩子表。
#[repr(C)]
pub struct KernelMixinRuntimeHooksV1 {
    /// 当前必须为 1。
    pub abi_version: u16,
    /// 当前结构完整长度。
    pub struct_size: u16,
    /// 当前必须为零。
    pub flags: u32,
    /// 零分配同步分发入口。
    pub dispatch: KernelMixinDispatchV1,
}

impl KernelMixinRuntimeHooksV1 {
    /// 校验钩子表固定字段和入口。
    pub fn valid(&self) -> bool {
        self.abi_version == 1
            && self.struct_size as usize == core::mem::size_of::<Self>()
            && self.flags == 0
            && self.dispatch as usize != 0
    }
}

static MIXIN_RUNTIME_HOOKS: AtomicPtr<KernelMixinRuntimeHooksV1> =
    AtomicPtr::new(core::ptr::null_mut());
static MIXIN_RUNTIME_ACTIVE: AtomicBool = AtomicBool::new(false);

/// 返回当前是否至少有一个内核 Mixin 站点安装了处理器。
///
/// 导出包装器先读取这个全局门控。常态没有处理器时，每次调用只承担一次原子读取；
/// 只有门控打开后才读取函数自己的 head/return 路由。
#[inline]
pub fn mixin_runtime_active() -> bool {
    MIXIN_RUNTIME_ACTIVE.load(Ordering::Acquire)
}

/// 发布内核 Mixin 处理器集合的全局活动状态。
///
/// 路由器先更新所有站点指针，再更新该提示。门控本身不发布、解引用路由；观察到活动
/// 状态后，包装器仍通过站点 route 的 Acquire 读取取得不可变快照。
#[doc(hidden)]
pub fn publish_mixin_runtime_active(active: bool) {
    MIXIN_RUNTIME_ACTIVE.store(active, Ordering::Release);
}

/// 安装一次内核 Mixin 运行时钩子。
pub fn install_mixin_runtime_hooks(hooks: &'static KernelMixinRuntimeHooksV1) -> bool {
    if !hooks.valid() {
        return false;
    }
    let pointer = core::ptr::from_ref(hooks).cast_mut();
    MIXIN_RUNTIME_HOOKS
        .compare_exchange(
            core::ptr::null_mut(),
            pointer,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_ok()
        || MIXIN_RUNTIME_HOOKS.load(Ordering::Acquire) == pointer
}

/// 把栈上调用帧交给已安装的内核 Mixin 路由器。
///
/// # Safety
///
/// `site` 和 `frame` 必须在整个同步调用期间有效，且帧内所有槽满足其 Rust 类型和别名规则。
pub unsafe fn dispatch_kernel_mixin(
    site: &KernelMixinSiteDescriptorV1,
    frame: &mut KernelMixinFrameV1,
) -> i32 {
    if !site.has_handlers() {
        return KERNEL_MIXIN_DISPATCH_UNHANDLED;
    }
    let hooks = MIXIN_RUNTIME_HOOKS.load(Ordering::Acquire);
    if hooks.is_null() {
        return KERNEL_MIXIN_DISPATCH_UNHANDLED;
    }
    // Safety: 指针只由安装函数写入经过校验的静态钩子表。
    let hooks = unsafe { &*hooks };
    // Safety: 调用方承担站点和帧生命周期，钩子表保证同步返回。
    unsafe { (hooks.dispatch)(core::ptr::from_ref(site), core::ptr::from_mut(frame)) }
}

/// 在独立冷路径中执行内核 Mixin 调用帧逻辑。
///
/// 导出宏只在站点存在活动处理链时调用这里。禁止内联可以阻止编译器把慢路径的大型栈帧
/// 合并回常用入口，使没有处理器的直接符号调用只承担路由快查成本。
#[cold]
#[inline(never)]
pub fn invoke_kernel_mixin_slow<F, R>(callback: F) -> R
where
    F: FnOnce() -> R,
{
    callback()
}

/// 目标函数在栈上保存的原逻辑 continuation。
///
/// 该类型把任意 `FnOnce() -> R` 擦除成 [`KernelMixinContinuationV1`]，使覆盖处理器能够
/// 通过帧中的 `original` 入口调用真实函数体，同时保持返回值写入调用方的栈槽。
pub struct KernelMixinOriginal<F, R>
where
    F: FnOnce() -> R,
{
    callback: Option<F>,
    result: *mut MaybeUninit<R>,
}

impl<F, R> KernelMixinOriginal<F, R>
where
    F: FnOnce() -> R,
{
    /// 构造一个尚未执行的原逻辑 continuation。
    pub fn new(callback: F, result: &mut MaybeUninit<R>) -> Self {
        Self {
            callback: Some(callback),
            result: core::ptr::from_mut(result),
        }
    }

    /// 把该 continuation 安装到调用帧。
    pub fn bind(&mut self, frame: &mut KernelMixinFrameV1) {
        frame.original = Some(call_kernel_mixin_original::<F, R>);
        frame.original_context = core::ptr::from_mut(self).cast();
    }
}

unsafe extern "C" fn call_kernel_mixin_original<F, R>(
    context: *mut (),
    frame: *mut KernelMixinFrameV1,
) -> i32
where
    F: FnOnce() -> R,
{
    if context.is_null() || frame.is_null() {
        return KERNEL_MIXIN_DISPATCH_INVALID;
    }
    // Safety: `bind` 只写入同一栈帧中仍然存活的 `KernelMixinOriginal<F, R>` 地址。
    let original = unsafe { &mut *context.cast::<KernelMixinOriginal<F, R>>() };
    let Some(callback) = original.callback.take() else {
        return KERNEL_MIXIN_DISPATCH_INVALID;
    };
    // Safety: 结果指针来自调用方仍存活的 `MaybeUninit<R>` 栈槽。
    unsafe { (*original.result).write(callback()) };
    // Safety: 调用方保证帧在 continuation 同步调用期间存活。
    let frame = unsafe { &mut *frame };
    if frame.result.is_null() {
        return KERNEL_MIXIN_DISPATCH_INVALID;
    }
    // Safety: 上面已经用同一个 `R` 初始化结果存储，槽属于当前活动帧。
    if !unsafe { (*frame.result).mark_initialized::<R>() } {
        return KERNEL_MIXIN_DISPATCH_INVALID;
    }
    frame.flags |= KERNEL_MIXIN_FRAME_RESULT_READY;
    KERNEL_MIXIN_DISPATCH_OK
}

/// 在确认调用帧已经产生返回值后取出结果。
///
/// 目标包装器只在原函数、取消处理器或覆盖链成功写入结果后调用本函数；缺少结果表示
/// Mixin 路由器违反固定协议，因此立即终止当前调用。
pub fn finish_kernel_mixin_result<R>(result: MaybeUninit<R>, frame: &KernelMixinFrameV1) -> R {
    assert!(
        frame.flags & KERNEL_MIXIN_FRAME_RESULT_READY != 0
            && !frame.result.is_null()
            // Safety: 这里只读取调用方栈上仍存活的结果槽标志。
            && unsafe { (*frame.result).flags & KERNEL_MIXIN_VALUE_UNINITIALIZED == 0 },
        "内核 Mixin 调用没有产生返回值"
    );
    // Safety: RESULT_READY 只能由写入同一 `MaybeUninit<R>` 的原逻辑或已验证处理器设置。
    unsafe { result.assume_init() }
}

/// 内核符号描述符的固定魔数。
pub const KERNEL_SYMBOL_DESCRIPTOR_MAGIC: u64 = u64::from_le_bytes(*b"KRSYM001");
/// 当前内核符号描述符 ABI 版本。
pub const KERNEL_SYMBOL_DESCRIPTOR_ABI_V1: u16 = 1;
/// 符号名称允许的最大字节数。
pub const KERNEL_SYMBOL_NAME_MAX_LEN: usize = 192;
/// 符号契约 identifier 允许的最大字节数。
pub const KERNEL_SYMBOL_CONTRACT_MAX_LEN: usize = 192;
/// 规范 Rust ABI 字符串允许的最大字节数。
pub const KERNEL_SYMBOL_RUST_ABI_MAX_LEN: usize = 1024;
/// 工具链链接符号允许的最大字节数。
pub const KERNEL_SYMBOL_LINK_NAME_MAX_LEN: usize = 96;

/// 集成组件描述符的固定魔数。
pub const KERNEL_INTEGRATED_COMPONENT_MAGIC: u64 = u64::from_le_bytes(*b"KINIT001");
/// 集成组件描述符 ABI 版本。
pub const KERNEL_INTEGRATED_COMPONENT_ABI_V1: u16 = 1;

/// 集成组件在设备枚举前执行。
pub const KERNEL_INTEGRATED_PHASE_DEVICE: u16 = 1;
/// 集成组件在调度器基础环境建立后执行。
pub const KERNEL_INTEGRATED_PHASE_RUNTIME: u16 = 2;

/// 直接链接进内核镜像的普通组件初始化入口。
pub type KernelIntegratedInit = fn() -> i32;
/// 直接链接进内核镜像的普通组件终结入口。
pub type KernelIntegratedFinalize = fn() -> i32;

/// 由构建期集成组件放入 `.kernel.integrated_components` 的普通内核 initcall 描述符。
///
/// 该结构不表示 ELM cell，也不包含 EBI、来源、代际或 elm-mgr 元数据。ELM attribute 在
/// `y` 模式只负责生成该普通链接记录，运行时执行后不会保留任何 ELM 管理身份。
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct KernelIntegratedComponentV1 {
    /// 固定魔数。
    pub magic: u64,
    /// 描述符 ABI 版本。
    pub abi_version: u16,
    /// 当前结构完整长度。
    pub struct_size: u16,
    /// 初始化阶段。
    pub phase: u16,
    /// v1 必须为零。
    pub flags: u16,
    /// 组件编译时使用的内核 API Profile 摘要。
    pub interface_hash: [u8; 32],
    /// 普通内核初始化入口。
    pub initialize: KernelIntegratedInit,
    /// 内核有序停机时调用的终结入口。
    pub finalize: KernelIntegratedFinalize,
}

impl KernelIntegratedComponentV1 {
    /// 构造一个完整的集成组件描述符。
    pub const fn new(
        initialize: KernelIntegratedInit,
        finalize: KernelIntegratedFinalize,
        interface_hash: [u8; 32],
        phase: u16,
    ) -> Self {
        Self {
            magic: KERNEL_INTEGRATED_COMPONENT_MAGIC,
            abi_version: KERNEL_INTEGRATED_COMPONENT_ABI_V1,
            struct_size: core::mem::size_of::<Self>() as u16,
            phase,
            flags: 0,
            interface_hash,
            initialize,
            finalize,
        }
    }

    /// 校验链接记录的固定布局和入口。
    pub fn valid(&self, interface_hash: [u8; 32]) -> bool {
        self.magic == KERNEL_INTEGRATED_COMPONENT_MAGIC
            && self.abi_version == KERNEL_INTEGRATED_COMPONENT_ABI_V1
            && self.struct_size as usize == core::mem::size_of::<Self>()
            && self.flags == 0
            && matches!(
                self.phase,
                KERNEL_INTEGRATED_PHASE_DEVICE | KERNEL_INTEGRATED_PHASE_RUNTIME
            )
            && self.interface_hash != [0; 32]
            && self.interface_hash == interface_hash
            && self.initialize as usize != 0
            && self.finalize as usize != 0
    }
}

/// 描述符表示可调用的 Rust 函数。
pub const KERNEL_SYMBOL_KIND_FUNCTION: u8 = 1;
/// 描述符表示具有静态存储期的对象。
pub const KERNEL_SYMBOL_KIND_STATIC: u8 = 2;
/// 描述符表示固有实现中的方法；调用 ABI 仍是普通 Rust 函数 ABI。
pub const KERNEL_SYMBOL_KIND_METHOD: u8 = 3;

/// 符号在装载后通过直接 Rust 调用执行。
pub const KERNEL_SYMBOL_DOMAIN_DIRECT_RUST: u8 = 1;

/// 设备直接符号创建的资源统一归入 ELM `Device` 类别。
pub const KERNEL_SYMBOL_RESOURCE_KIND_DEVICE: u32 = 7;
/// 资源已登记到当前 ELM 单元。
pub const KERNEL_SYMBOL_RESOURCE_STATUS_TRACKED: i32 = 0;
/// 当前不在 ELM 执行上下文中，资源保持普通内建内核生命周期。
pub const KERNEL_SYMBOL_RESOURCE_STATUS_UNMANAGED: i32 = 1;
/// 运行时拒绝登记或解除资源。
pub const KERNEL_SYMBOL_RESOURCE_STATUS_FAILED: i32 = -1;

/// 该入口会修改内核或设备状态。
pub const KERNEL_SYMBOL_FLAG_MUTATES_STATE: u32 = 1 << 0;
/// 该入口的 Rust 签名本身是 `unsafe fn`。
pub const KERNEL_SYMBOL_FLAG_UNSAFE: u32 = 1 << 1;
/// 该入口可能返回需要调用方负责释放或撤销的长期对象。
pub const KERNEL_SYMBOL_FLAG_RETURNS_OWNED: u32 = 1 << 2;
/// 该入口只用于诊断，不应被实现用作权限真值来源。
pub const KERNEL_SYMBOL_FLAG_DIAGNOSTIC: u32 = 1 << 3;
/// 入口会把一个或多个来自 ELM 镜像的对象或函数指针保留到调用返回之后。
pub const KERNEL_SYMBOL_FLAG_RETAINS_MODULE_CODE: u32 = 1 << 4;
/// 入口返回的借用可能指向 ELM 镜像拥有的对象，调用方必须维持镜像固定。
pub const KERNEL_SYMBOL_FLAG_RETURNS_MODULE_BORROW: u32 = 1 << 5;
/// 当前版本认可的全部符号标志位。
pub const KERNEL_SYMBOL_FLAGS_MASK: u32 = KERNEL_SYMBOL_FLAG_MUTATES_STATE
    | KERNEL_SYMBOL_FLAG_UNSAFE
    | KERNEL_SYMBOL_FLAG_RETURNS_OWNED
    | KERNEL_SYMBOL_FLAG_DIAGNOSTIC
    | KERNEL_SYMBOL_FLAG_RETAINS_MODULE_CODE
    | KERNEL_SYMBOL_FLAG_RETURNS_MODULE_BORROW;

/// 最多允许一个直接符号声明 64 个参数的长期保留关系。
pub const KERNEL_SYMBOL_MAX_TRACKED_ARGUMENTS: usize = 64;

/// 常驻子系统用于暂停、恢复和退役直接符号资源的操作。
pub type KernelSymbolOwnedResourceOp =
    fn(owner: u64, generation: u64, handle: u64) -> Result<(), i32>;

/// 直接符号资源的完整生命周期操作表。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct KernelSymbolOwnedResourceOpsV1 {
    /// 暂停资源并阻止回调进入模块镜像。
    pub suspend: KernelSymbolOwnedResourceOp,
    /// 恢复已经暂停的资源。
    pub resume: KernelSymbolOwnedResourceOp,
    /// 停止接纳新工作。
    pub quiesce: KernelSymbolOwnedResourceOp,
    /// 取消尚未开始的工作。
    pub cancel: KernelSymbolOwnedResourceOp,
    /// 等待运行中工作退出。
    pub drain: KernelSymbolOwnedResourceOp,
    /// 注销资源并释放内核持有的最后一个模块对象。
    pub release: KernelSymbolOwnedResourceOp,
}

impl KernelSymbolOwnedResourceOpsV1 {
    /// 构造一个六阶段操作均完整提供的资源操作表。
    pub const fn new(
        suspend: KernelSymbolOwnedResourceOp,
        resume: KernelSymbolOwnedResourceOp,
        quiesce: KernelSymbolOwnedResourceOp,
        cancel: KernelSymbolOwnedResourceOp,
        drain: KernelSymbolOwnedResourceOp,
        release: KernelSymbolOwnedResourceOp,
    ) -> Self {
        Self {
            suspend,
            resume,
            quiesce,
            cancel,
            drain,
            release,
        }
    }
}

/// ELM 内核运行时向常驻子系统提供的资源归属钩子。
#[repr(C)]
pub struct KernelSymbolRuntimeHooksV1 {
    /// 当前必须为 1。
    pub abi_version: u16,
    /// 必须等于本结构大小。
    pub struct_size: u16,
    /// 保留字段，必须为零。
    pub reserved: u32,
    /// 把资源登记到当前 ELM 单元。
    pub register_owned_resource: fn(u32, u64, KernelSymbolOwnedResourceOpsV1) -> i32,
    /// 在模块主动注销资源后解除归属记录。
    pub release_owned_resource: fn(u32, u64) -> i32,
}

impl KernelSymbolRuntimeHooksV1 {
    /// 校验钩子表前缀和版本。
    pub const fn valid(&self) -> bool {
        self.abi_version == 1
            && self.struct_size as usize == core::mem::size_of::<Self>()
            && self.reserved == 0
    }
}

static RUNTIME_HOOKS: AtomicPtr<KernelSymbolRuntimeHooksV1> = AtomicPtr::new(core::ptr::null_mut());

/// 安装一次 ELM 资源归属钩子。
pub fn install_runtime_hooks(hooks: &'static KernelSymbolRuntimeHooksV1) -> bool {
    if !hooks.valid() {
        return false;
    }
    let pointer = core::ptr::from_ref(hooks).cast_mut();
    RUNTIME_HOOKS
        .compare_exchange(
            core::ptr::null_mut(),
            pointer,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_ok()
        || RUNTIME_HOOKS.load(Ordering::Acquire) == pointer
}

/// 返回 ELM 运行时是否已经接管直接符号资源归属。
pub fn runtime_hooks_installed() -> bool {
    !RUNTIME_HOOKS.load(Ordering::Acquire).is_null()
}

/// 把常驻子系统资源登记到当前 ELM；内建调用返回 `UNMANAGED`。
pub fn track_owned_resource(kind: u32, handle: u64, ops: KernelSymbolOwnedResourceOpsV1) -> i32 {
    let hooks = RUNTIME_HOOKS.load(Ordering::Acquire);
    if hooks.is_null() {
        return KERNEL_SYMBOL_RESOURCE_STATUS_UNMANAGED;
    }
    // Safety: 指针只由 install_runtime_hooks 写入静态、通过结构校验的只读对象。
    let hooks = unsafe { &*hooks };
    (hooks.register_owned_resource)(kind, handle, ops)
}

/// 解除当前 ELM 对已经由模块主动注销的资源的归属记录。
pub fn untrack_owned_resource(kind: u32, handle: u64) -> i32 {
    let hooks = RUNTIME_HOOKS.load(Ordering::Acquire);
    if hooks.is_null() {
        return KERNEL_SYMBOL_RESOURCE_STATUS_UNMANAGED;
    }
    // Safety: 指针只由 install_runtime_hooks 写入静态、通过结构校验的只读对象。
    let hooks = unsafe { &*hooks };
    (hooks.release_owned_resource)(kind, handle)
}

/// 内核直接符号的权限能力组。
pub mod capability {
    /// 不携带子系统权限的纯查询或纯计算入口。
    pub const CORE_SAFE: u64 = 1 << 0;

    /// 普通内核堆分配、释放和调整大小。
    pub const ALLOCATOR_MEMORY: u64 = 1 << 1;
    /// allocator 统计、能力查询和只读诊断。
    pub const ALLOCATOR_DIAGNOSTIC: u64 = 1 << 2;
    /// 显式物理页、地址空间和 DMA backing 分配。
    pub const ALLOCATOR_PHYSICAL: u64 = 1 << 3;
    /// managed heap、GC 句柄、根和回收控制。
    pub const ALLOCATOR_MANAGED: u64 = 1 << 4;
    /// allocator 初始化、后端安装和全局策略修改。
    pub const ALLOCATOR_ADMIN: u64 = 1 << 5;

    /// VFS 元数据、路径和只读状态查询。
    pub const VFS_QUERY: u64 = 1 << 6;
    /// 文件、目录、管道和描述符 I/O 操作。
    pub const VFS_IO: u64 = 1 << 7;
    /// 挂载命名空间、全局缓存和 VFS 策略修改。
    pub const VFS_ADMIN: u64 = 1 << 8;
    /// 文件系统驱动及 VFS 扩展对象注册。
    pub const VFS_DRIVER: u64 = 1 << 9;

    /// 调度器、任务和 CPU 拓扑的只读查询。
    pub const SCHED_QUERY: u64 = 1 << 10;
    /// 任务创建、唤醒、等待、信号和生命周期操作。
    pub const SCHED_TASK: u64 = 1 << 11;
    /// 全局调度策略、拓扑和 CPU 状态修改。
    pub const SCHED_ADMIN: u64 = 1 << 12;
    /// 架构、任务扩展和生命周期钩子注册。
    pub const SCHED_HOOK: u64 = 1 << 13;

    /// 地址空间模型和映射状态的只读查询。
    pub const MM_QUERY: u64 = 1 << 14;
    /// VMA、用户地址空间和映射内容修改。
    pub const MM_MEMORY: u64 = 1 << 15;

    /// 设备、总线和函数对象的只读发现与快照。
    pub const DEVICE_DISCOVERY: u64 = 1 << 16;
    /// 设备驱动、工厂、函数和热插拔生命周期注册。
    pub const DEVICE_DRIVER: u64 = 1 << 17;
    /// 设备长期资源的取得、登记、释放和撤销。
    pub const DEVICE_RESOURCE: u64 = 1 << 18;
    /// DMA 映射、同步和 DMA backing 管理。
    pub const DEVICE_DMA: u64 = 1 << 19;
    /// IRQ domain、handler、MSI controller 和向量管理。
    pub const DEVICE_INTERRUPT: u64 = 1 << 20;
    /// PCI、platform、USB、virtio 和 firmware bus 操作。
    pub const DEVICE_BUS: u64 = 1 << 21;
    /// 安装全局总线后端、配置访问器或平台级设备策略。
    pub const DEVICE_ADMIN: u64 = 1 << 22;

    /// 页表后端、全局地址空间策略和内存管理后端修改。
    pub const MM_ADMIN: u64 = 1 << 23;
    /// ELF 等非网络镜像格式的解析和验证。
    pub const IMAGE_PARSE: u64 = 1 << 24;
    /// EFI、ACPI、DTB 等固件信息的只读访问。
    pub const FIRMWARE_QUERY: u64 = 1 << 25;
    /// 固件处理器、事件和平台控制后端安装。
    pub const FIRMWARE_ADMIN: u64 = 1 << 26;
    /// FAT、Ext 等具体文件系统驱动的构造和注册。
    pub const FILESYSTEM_DRIVER: u64 = 1 << 27;
    /// 非网络 IPC 对象的创建、查询和操作。
    pub const IPC: u64 = 1 << 28;
    /// 稳定 HAL 参数、时间和平台能力查询。
    pub const HAL_QUERY: u64 = 1 << 29;
    /// HAL 钩子、硬件控制和用户上下文状态修改。
    pub const HAL_CONTROL: u64 = 1 << 30;
    /// 网络协议栈 generation 的注册、启动材料和数据面调用。
    pub const NETWORK_STACK: u64 = 1 << 31;

    /// 默认不需要额外管理员批准的能力组。
    pub const SAFE_DEFAULT: u64 = CORE_SAFE
        | ALLOCATOR_MEMORY
        | ALLOCATOR_DIAGNOSTIC
        | VFS_QUERY
        | SCHED_QUERY
        | MM_QUERY
        | IMAGE_PARSE
        | FIRMWARE_QUERY
        | HAL_QUERY;
    /// allocator 当前定义的全部能力组。
    pub const ALLOCATOR_ALL: u64 = ALLOCATOR_MEMORY
        | ALLOCATOR_DIAGNOSTIC
        | ALLOCATOR_PHYSICAL
        | ALLOCATOR_MANAGED
        | ALLOCATOR_ADMIN;
    /// 设备抽象当前定义的全部能力组。
    pub const DEVICE_ALL: u64 = DEVICE_DISCOVERY
        | DEVICE_DRIVER
        | DEVICE_RESOURCE
        | DEVICE_DMA
        | DEVICE_INTERRUPT
        | DEVICE_BUS
        | DEVICE_ADMIN;
    /// VFS 当前定义的全部能力组。
    pub const VFS_ALL: u64 = VFS_QUERY | VFS_IO | VFS_ADMIN | VFS_DRIVER;
    /// 调度器当前定义的全部能力组。
    pub const SCHED_ALL: u64 = SCHED_QUERY | SCHED_TASK | SCHED_ADMIN | SCHED_HOOK;
    /// 内存模型和地址空间当前定义的全部能力组。
    pub const MM_ALL: u64 = MM_QUERY | MM_MEMORY | MM_ADMIN;
    /// 固件访问当前定义的全部能力组。
    pub const FIRMWARE_ALL: u64 = FIRMWARE_QUERY | FIRMWARE_ADMIN;
    /// HAL 当前定义的全部能力组。
    pub const HAL_ALL: u64 = HAL_QUERY | HAL_CONTROL;
    /// 当前协议认识的全部能力组。
    pub const ALL: u64 = CORE_SAFE
        | ALLOCATOR_ALL
        | VFS_ALL
        | SCHED_ALL
        | MM_ALL
        | DEVICE_ALL
        | IMAGE_PARSE
        | FIRMWARE_ALL
        | FILESYSTEM_DRIVER
        | IPC
        | HAL_ALL
        | NETWORK_STACK;
}

/// 链接到内核镜像中的直接符号描述符。
///
/// 字符串和地址都必须具有静态存储期。该结构只在同一次内核构建产生的镜像内部遍历，
/// 不属于 EBI、EKI 或用户态 ABI。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct KernelSymbolDescriptorV1 {
    /// 固定魔数，必须为 [`KERNEL_SYMBOL_DESCRIPTOR_MAGIC`]。
    pub magic: u64,
    /// 结构实际字节数。
    pub struct_size: u16,
    /// 描述符 ABI 版本。
    pub abi_version: u16,
    /// `KERNEL_SYMBOL_KIND_*` 中的一种。
    pub kind: u8,
    /// 当前必须为 [`KERNEL_SYMBOL_DOMAIN_DIRECT_RUST`]。
    pub execution_domain: u8,
    /// 保留字段，必须为零。
    pub reserved0: u16,
    /// `KERNEL_SYMBOL_FLAG_*` 位集合。
    pub flags: u32,
    /// 符号契约版本，零无效。
    pub version: u32,
    /// 使用该入口必须取得的能力组位集合。
    pub capabilities: u64,
    /// 位 `n` 表示第 `n` 个显式参数会被内核保留到调用返回之后。
    pub retained_argument_mask: u64,
    /// 当前内核构建采用的规范接口摘要。
    pub interface_hash: [u8; 32],
    /// 稳定、与 Rust 模块层级一致的 API 路径。
    pub api_path: &'static str,
    /// 描述符所指向实现项在内核 crate 中的真实 Rust 路径。
    pub item_path: &'static str,
    /// ELM ELF 与内核镜像共同使用的稳定链接符号。
    pub link_name: &'static str,
    /// 稳定语义契约 identifier。
    pub contract: &'static str,
    /// 由导出宏和导入宏共同生成的规范 Rust 函数签名。
    pub rust_abi: &'static str,
    /// 常驻函数或静态对象的真实地址。
    pub address: *const (),
}

// Safety: 描述符只保存静态只读元数据和常驻地址；并发访问不会修改目标对象。
unsafe impl Sync for KernelSymbolDescriptorV1 {}

impl KernelSymbolDescriptorV1 {
    /// 构造一个函数符号描述符。
    pub const fn function(
        api_path: &'static str,
        contract: &'static str,
        version: u32,
        capabilities: u64,
        flags: u32,
        retained_argument_mask: u64,
        item_path: &'static str,
        link_name: &'static str,
        rust_abi: &'static str,
        address: *const (),
    ) -> Self {
        Self {
            magic: KERNEL_SYMBOL_DESCRIPTOR_MAGIC,
            struct_size: core::mem::size_of::<Self>() as u16,
            abi_version: KERNEL_SYMBOL_DESCRIPTOR_ABI_V1,
            kind: KERNEL_SYMBOL_KIND_FUNCTION,
            execution_domain: KERNEL_SYMBOL_DOMAIN_DIRECT_RUST,
            reserved0: 0,
            flags,
            version,
            capabilities,
            retained_argument_mask,
            interface_hash: KERNEL_INTERFACE_SOURCE_SHA256,
            api_path,
            item_path,
            link_name,
            contract,
            rust_abi,
            address,
        }
    }

    /// 构造一个静态对象符号描述符。
    pub const fn static_object(
        api_path: &'static str,
        contract: &'static str,
        version: u32,
        capabilities: u64,
        flags: u32,
        item_path: &'static str,
        link_name: &'static str,
        rust_abi: &'static str,
        address: *const (),
    ) -> Self {
        let mut descriptor = Self::function(
            api_path,
            contract,
            version,
            capabilities,
            flags,
            0,
            item_path,
            link_name,
            rust_abi,
            address,
        );
        descriptor.kind = KERNEL_SYMBOL_KIND_STATIC;
        descriptor
    }

    /// 构造一个固有方法描述符。
    pub const fn method(
        api_path: &'static str,
        contract: &'static str,
        version: u32,
        capabilities: u64,
        flags: u32,
        retained_argument_mask: u64,
        item_path: &'static str,
        link_name: &'static str,
        rust_abi: &'static str,
        address: *const (),
    ) -> Self {
        let mut descriptor = Self::function(
            api_path,
            contract,
            version,
            capabilities,
            flags,
            retained_argument_mask,
            item_path,
            link_name,
            rust_abi,
            address,
        );
        descriptor.kind = KERNEL_SYMBOL_KIND_METHOD;
        descriptor
    }

    /// 校验描述符的结构不变量和文本字段。
    pub fn validate(&self) -> bool {
        self.magic == KERNEL_SYMBOL_DESCRIPTOR_MAGIC
            && self.struct_size as usize == core::mem::size_of::<Self>()
            && self.abi_version == KERNEL_SYMBOL_DESCRIPTOR_ABI_V1
            && matches!(
                self.kind,
                KERNEL_SYMBOL_KIND_FUNCTION | KERNEL_SYMBOL_KIND_STATIC | KERNEL_SYMBOL_KIND_METHOD
            )
            && self.execution_domain == KERNEL_SYMBOL_DOMAIN_DIRECT_RUST
            && self.reserved0 == 0
            && self.flags & !KERNEL_SYMBOL_FLAGS_MASK == 0
            && (self.retained_argument_mask == 0
                || self.flags & KERNEL_SYMBOL_FLAG_RETAINS_MODULE_CODE != 0)
            && (self.flags & KERNEL_SYMBOL_FLAG_RETAINS_MODULE_CODE == 0
                || self.retained_argument_mask != 0)
            && self.version != 0
            && self.capabilities != 0
            && self.capabilities & !capability::ALL == 0
            && self.interface_hash != [0; 32]
            && valid_identifier(self.api_path, KERNEL_SYMBOL_NAME_MAX_LEN)
            && valid_rust_path(self.item_path)
            && valid_link_name(self.link_name)
            && valid_identifier(self.contract, KERNEL_SYMBOL_CONTRACT_MAX_LEN)
            && !self.rust_abi.is_empty()
            && self.rust_abi.len() <= KERNEL_SYMBOL_RUST_ABI_MAX_LEN
            && !self.address.is_null()
    }
}

impl fmt::Debug for KernelSymbolDescriptorV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("KernelSymbolDescriptorV1")
            .field("kind", &self.kind)
            .field("flags", &self.flags)
            .field("version", &self.version)
            .field("capabilities", &self.capabilities)
            .field("retained_argument_mask", &self.retained_argument_mask)
            .field("interface_hash", &self.interface_hash)
            .field("api_path", &self.api_path)
            .field("item_path", &self.item_path)
            .field("link_name", &self.link_name)
            .field("contract", &self.contract)
            .field("rust_abi", &self.rust_abi)
            .field("address", &self.address)
            .finish()
    }
}

fn valid_identifier(value: &str, max_len: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_len
        && !value.starts_with('.')
        && !value.ends_with('.')
        && !value.contains("..")
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b'@'))
}

fn valid_link_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= KERNEL_SYMBOL_LINK_NAME_MAX_LEN
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn valid_rust_path(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= KERNEL_SYMBOL_NAME_MAX_LEN
        && !value.starts_with("::")
        && !value.ends_with("::")
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'_' | b':' | b'<' | b'>' | b' ' | b',' | b'[' | b']' | b'&' | b'\''
                )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    static MIXIN_HINT_ROUTE: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

    fn example(value: usize) -> usize {
        value
    }

    #[test]
    fn descriptor_validation_rejects_unknown_capabilities() {
        let valid = KernelSymbolDescriptorV1::function(
            "allocator.example",
            "kernel.allocator.example@1",
            1,
            capability::ALLOCATOR_MEMORY,
            0,
            0,
            "kernel_symbols::tests::example",
            "__elm_kernel_api_example",
            "fn(usize)->usize",
            example as *const (),
        );
        assert!(valid.validate());

        let mut invalid = valid;
        invalid.capabilities = 1 << 63;
        assert!(!invalid.validate());
    }

    #[test]
    fn integrated_component_requires_exact_interface_profile() {
        fn initialize() -> i32 {
            0
        }
        fn finalize() -> i32 {
            0
        }

        let profile = [0x5a; 32];
        let component = KernelIntegratedComponentV1::new(
            initialize,
            finalize,
            profile,
            KERNEL_INTEGRATED_PHASE_RUNTIME,
        );

        assert!(component.valid(profile));
        assert!(!component.valid([0xa5; 32]));
        assert!(
            !KernelIntegratedComponentV1::new(
                initialize,
                finalize,
                [0; 32],
                KERNEL_INTEGRATED_PHASE_RUNTIME,
            )
            .valid([0; 32])
        );
    }

    #[test]
    fn mixin_slow_path_invokes_callback_once() {
        let mut calls = 0usize;
        let result = invoke_kernel_mixin_slow(|| {
            calls += 1;
            42usize
        });
        assert_eq!(result, 42);
        assert_eq!(calls, 1);
    }

    #[test]
    fn mixin_runtime_gate_tracks_published_routes() {
        publish_mixin_runtime_active(false);
        assert!(!mixin_runtime_active());
        publish_mixin_runtime_active(true);
        assert!(mixin_runtime_active());
        publish_mixin_runtime_active(false);
    }

    #[test]
    fn mixin_handler_hint_tracks_only_route_presence() {
        let descriptor = KernelMixinSiteDescriptorV1::new(
            KERNEL_MIXIN_SITE_HEAD,
            0,
            [1; 32],
            [2; 32],
            [3; 32],
            "tests.query",
            "head",
            &MIXIN_HINT_ROUTE,
        );
        assert!(!descriptor.has_handlers_hint());

        let marker = core::ptr::from_ref(&MIXIN_HINT_ROUTE).cast_mut().cast();
        MIXIN_HINT_ROUTE.store(marker, Ordering::Relaxed);
        assert!(descriptor.has_handlers_hint());
        MIXIN_HINT_ROUTE.store(core::ptr::null_mut(), Ordering::Relaxed);
    }
}
