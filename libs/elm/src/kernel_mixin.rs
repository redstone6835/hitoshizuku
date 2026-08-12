//! 内核符号级 Mixin 的 Rust 开发接口。
//!
//! 本模块只包装目标函数在栈上建立的短生命周期调用帧。参数、返回值和 continuation
//! 都不能逃逸当前处理器；构建工具和装载器会在处理器可见前校验完整内核 Profile、
//! 源码站点摘要和 Rust ABI 摘要。
//!
//! # 稳定范围
//!
//! v1 已稳定支持导出函数的 `head` 和 `return` 站点，以及 `inject`、`modify_arg`、
//! `modify_return`、`overwrite` 四类处理器。内部调用、局部变量和字段站点需要 MIR 级
//! 类型与控制流信息；对应 `redirect`、`wrap_operation`、`modify_local` attribute 当前会以
//! `TODO(ELM-MIR)` 在编译期拒绝，不会生成占位实现。
//!
//! # 示例
//!
//! 下例把四种稳定处理器附着到真实的 `allocator.GlobalAlloc.alloc`。`method` 使用内核接口
//! 清单中的稳定 API 路径；开发者不声明 `extern`、导出符号或调用帧 ABI。
//!
//! ```ignore
//! use core::alloc::Layout;
//! use elm::{ElmModule, HookResult, KernelMixinContext};
//!
//! struct AllocObserver;
//!
//! #[elm::mixin(target = "allocator")]
//! impl AllocObserver {
//!     #[elm::inject(method = "GlobalAlloc.alloc", at = "head", priority = 300)]
//!     fn before_alloc(&self, context: &mut KernelMixinContext<'_>) -> HookResult {
//!         let _layout = context.argument::<Layout>(1).ok_or(elm::HookError::new(-1))?;
//!         Ok(())
//!     }
//!
//!     #[elm::modify_arg(method = "GlobalAlloc.alloc", priority = 200)]
//!     fn inspect_layout(&self, context: &mut KernelMixinContext<'_>) -> HookResult {
//!         let layout = context
//!             .argument_mut::<Layout>(1)
//!             .ok_or(elm::HookError::new(-1))?;
//!         *layout = *layout;
//!         Ok(())
//!     }
//!
//!     #[elm::overwrite(method = "GlobalAlloc.alloc", priority = 100)]
//!     fn wrap_alloc(&self, context: &mut KernelMixinContext<'_>) -> HookResult {
//!         context.proceed()
//!     }
//!
//!     #[elm::modify_return(method = "GlobalAlloc.alloc", priority = 100)]
//!     fn after_alloc(&self, context: &mut KernelMixinContext<'_>) -> HookResult {
//!         let _pointer = context
//!             .result::<*mut u8>()
//!             .ok_or(elm::HookError::new(-1))?;
//!         Ok(())
//!     }
//! }
//! ```

use core::marker::PhantomData;

use crate::{ElmModule, HookError, HookResult, ModuleSlot};

pub use kernel_symbols::{
    KERNEL_MIXIN_DISPATCH_INVALID, KERNEL_MIXIN_DISPATCH_OK, KERNEL_MIXIN_FRAME_CANCELLED,
    KERNEL_MIXIN_FRAME_FAULTED, KERNEL_MIXIN_FRAME_RESULT_READY, KERNEL_MIXIN_FRAME_STOP,
    KERNEL_MIXIN_SITE_CALL_AFTER, KERNEL_MIXIN_SITE_CALL_BEFORE, KERNEL_MIXIN_SITE_FIELD,
    KERNEL_MIXIN_SITE_HEAD, KERNEL_MIXIN_SITE_LOCAL, KERNEL_MIXIN_SITE_RETURN, KernelMixinFrameV1,
};

/// 一个处理器对当前内核源码站点的受限同步视图。
///
/// `KernelMixinContext` 不能被构造、复制或保存。泛型访问器只有在镜像绑定的完整站点 ABI
/// 与运行中内核一致时才会被调用，因此类型名称匹配同时受到 Profile 摘要和装载器验证保护。
pub struct KernelMixinContext<'frame> {
    frame: &'frame mut KernelMixinFrameV1,
    _not_send: PhantomData<*mut ()>,
}

impl<'frame> KernelMixinContext<'frame> {
    /// 从运行时已经验证的调用帧建立处理器视图。
    ///
    /// # Safety
    ///
    /// `frame` 必须由当前内核站点创建，并且在 `KernelMixinContext` 销毁前保持独占有效。
    pub unsafe fn from_frame(frame: &'frame mut KernelMixinFrameV1) -> Option<Self> {
        frame.valid().then_some(Self {
            frame,
            _not_send: PhantomData,
        })
    }

    /// 返回当前站点类别。
    pub const fn site_kind(&self) -> u16 {
        self.frame.site_kind
    }

    /// 返回目标函数或操作暴露的参数数量。
    pub const fn argument_count(&self) -> usize {
        self.frame.argument_count as usize
    }

    /// 取得一个参数的共享借用。
    ///
    /// `index` 按规范 Rust 调用帧计数，方法接收者占第 0 项。类型、大小、对齐和初始化状态
    /// 任一不匹配都会返回 `None`，不会执行未经验证的类型转换。
    pub fn argument<T>(&mut self, index: usize) -> Option<&T> {
        // Safety: 上下文独占调用帧，返回借用受 `&mut self` 生命周期约束，不能逃逸处理器。
        unsafe { self.frame.argument::<T>(index) }
    }

    /// 取得一个允许修改的参数借用。
    ///
    /// 只读接收者和运行时标记为只读的槽不会返回可变借用。修改只影响尚未执行的原函数或
    /// continuation；借用不能越过当前处理器返回。
    pub fn argument_mut<T>(&mut self, index: usize) -> Option<&mut T> {
        // Safety: 上下文独占调用帧；槽的只读位、类型名称和初始化状态由底层校验。
        unsafe { self.frame.argument_mut::<T>(index) }
    }

    /// 取得已经产生的返回值共享借用。
    ///
    /// 该方法只在原函数、前一个 continuation 或 `set_result` 已经初始化返回槽后成功。
    pub fn result<T>(&mut self) -> Option<&T> {
        // Safety: 上下文独占调用帧，底层只在 RESULT_READY 后返回已初始化值。
        unsafe { self.frame.result::<T>() }
    }

    /// 取得已经产生的返回值可变借用。
    ///
    /// 典型用途是 `modify_return` 在 `return` 站点原地修改结果；类型不匹配时返回 `None`。
    pub fn result_mut<T>(&mut self) -> Option<&mut T> {
        // Safety: 上下文独占调用帧，底层只在 RESULT_READY 后返回已初始化值。
        unsafe { self.frame.result_mut::<T>() }
    }

    /// 写入尚未产生的返回值。
    ///
    /// 返回槽已经初始化或 `T` 与目标返回类型不一致时，原值通过 `Err` 返还给调用者。
    pub fn set_result<T>(&mut self, value: T) -> Result<(), T> {
        self.frame.set_result(value)
    }

    /// 写入提前返回值并阻止原函数或原操作执行。
    ///
    /// 该操作适用于 `head` 或调用前站点。返回类型不匹配时不会设置取消标志。
    pub fn cancel<T>(&mut self, value: T) -> Result<(), T> {
        self.frame.set_result(value)?;
        self.frame.flags |= KERNEL_MIXIN_FRAME_CANCELLED;
        Ok(())
    }

    /// 停止当前阶段剩余的普通自动继续处理器。
    ///
    /// `stop` 不会丢弃已经写入的参数或返回值，也不会回滚已经完成的处理器；运行时直接
    /// 转到该阶段末端的原逻辑或既有结果。
    pub fn stop(&mut self) {
        self.frame.flags |= KERNEL_MIXIN_FRAME_STOP;
    }

    /// 调用下一优先级处理器；处理链末端会进入原函数或原操作。
    ///
    /// 该方法只允许由 continuation 类处理器调用一次。当前稳定范围中只有 `overwrite` 会
    /// 生成此类处理器；`redirect` 和 `wrap_operation` 等待 `TODO(ELM-MIR)`。
    pub fn proceed(&mut self) -> HookResult {
        // Safety: continuation 由内核运行时为当前同步处理器安装，底层保证只能消费一次。
        let status = unsafe { self.frame.call_next() };
        self.frame.status = status;
        if status == KERNEL_MIXIN_DISPATCH_OK {
            Ok(())
        } else {
            Err(HookError::new(status))
        }
    }

    /// 返回处理链是否已经产生结果。
    pub const fn result_ready(&self) -> bool {
        self.frame.flags & KERNEL_MIXIN_FRAME_RESULT_READY != 0
    }

    /// 返回当前处理器之前是否已经记录故障。
    pub const fn faulted(&self) -> bool {
        self.frame.flags & KERNEL_MIXIN_FRAME_FAULTED != 0
    }
}

#[doc(hidden)]
/// 执行 attribute 生成的内核 Mixin trampoline。
///
/// # Safety
///
/// `frame` 必须来自内核 Mixin 路由器，`handler` 必须属于 `T` 的当前镜像 generation。
pub unsafe fn kernel_mixin_trampoline<T: ElmModule>(
    slot: &ModuleSlot<T>,
    frame: *mut KernelMixinFrameV1,
    handler: fn(&T, &mut KernelMixinContext<'_>) -> HookResult,
) -> i32 {
    if frame.is_null() {
        return KERNEL_MIXIN_DISPATCH_INVALID;
    }
    // Safety: 调用约束要求该指针来自当前同步路由器并保持独占有效。
    let frame = unsafe { &mut *frame };
    // Safety: 上面恢复了独占帧借用，固定头部和槽指针由构造器校验。
    let Some(mut context) = (unsafe { KernelMixinContext::from_frame(frame) }) else {
        return KERNEL_MIXIN_DISPATCH_INVALID;
    };
    match slot.with_active(|module| handler(module, &mut context)) {
        Ok(Ok(())) => KERNEL_MIXIN_DISPATCH_OK,
        Ok(Err(error)) | Err(error) => {
            context.frame.status = error.status();
            error.status()
        }
    }
}
