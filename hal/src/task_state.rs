//! 架构扩展任务状态的薄胶水。

use alloc::sync::Arc;
use core::any::Any;

use sched::{Task, TaskExtKey};

/// fork 时让架构后端深拷或重建自己拥有的任务扩展。
pub fn clone_extension(
    key: TaskExtKey,
    source: &Arc<dyn Any + Send + Sync>,
) -> Option<Arc<dyn Any + Send + Sync>> {
    arch::clone_user_task_extension(key, source)
}

/// exec/exit 时清理架构后端拥有的用户任务状态。
pub fn reset(task: &Task) {
    arch::reset_user_task_state(task);
}

/// 信号投递前保存基础 trap frame 之外的架构用户状态。
pub fn push_signal_state(task: &Arc<Task>, context: usize) -> Result<(), ()> {
    arch::push_user_signal_state(task, context)
}

/// `rt_sigreturn` 后恢复基础 trap frame 之外的架构用户状态。
pub fn pop_signal_state(task: &Arc<Task>, context: usize) {
    arch::pop_user_signal_state(task, context);
}
