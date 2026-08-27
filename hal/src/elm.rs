//! ELM 原生执行调用门的架构无关入口。

/// 在隔离栈上调用已验证的 ELM 原生入口。
///
/// # Safety
///
/// `entry` 必须指向当前架构的有效 ELM 原生入口，`context` 必须满足该入口
/// 的 ABI，`stack_top` 必须描述调用方独占且足够大的栈。
pub unsafe fn call_native(entry: usize, context: *mut u8, stack_top: usize) -> i32 {
    // Safety: 调用方承担通用调用门契约，arch 后端落实具体寄存器 ABI。
    unsafe { arch::call_elm_native(entry, context, stack_top) }
}

/// 在当前内核栈上调用已验证的 ELM 原生入口。
///
/// # Safety
///
/// `entry` 与 `context` 必须满足 [`call_native`] 的入口契约，当前栈还必须有
/// 足够余量。
pub unsafe fn call_native_current_stack(entry: usize, context: *mut u8) -> i32 {
    // Safety: 调用方承担通用调用门契约，arch 后端落实具体寄存器 ABI。
    unsafe { arch::call_elm_native_current_stack(entry, context) }
}

/// 从当前 ELM 原生执行域恢复到先前发布的内核边界帧。
///
/// # Safety
///
/// 三个值必须来自当前任务仍有效的 ELM fault guard 恢复记录。
pub unsafe fn resume_panic(return_pc: usize, return_sp: usize, return_value: usize) -> ! {
    // Safety: 调用方保证恢复帧仍属于当前执行域。
    unsafe { arch::resume_elm_panic(return_pc, return_sp, return_value) }
}
