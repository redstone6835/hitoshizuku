//! RISC-V 外部中断线的跨 hart 期望状态。

use core::sync::atomic::{AtomicBool, Ordering};

pub(crate) struct ExternalIrqState {
    enabled: AtomicBool,
}

impl ExternalIrqState {
    pub(crate) const fn new() -> Self {
        Self {
            enabled: AtomicBool::new(false),
        }
    }

    /// 更新期望状态，并返回状态是否真正发生变化。
    pub(crate) fn set_enabled(&self, enabled: bool) -> bool {
        self.enabled.swap(enabled, Ordering::AcqRel) != enabled
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }
}

static EXTERNAL_IRQ_STATE: ExternalIrqState = ExternalIrqState::new();

pub(crate) fn set_enabled(enabled: bool) -> bool {
    EXTERNAL_IRQ_STATE.set_enabled(enabled)
}

pub(crate) fn is_enabled() -> bool {
    EXTERNAL_IRQ_STATE.is_enabled()
}

#[cfg(test)]
mod tests {
    use super::ExternalIrqState;

    #[test]
    fn desired_state_reports_only_real_transitions() {
        let state = ExternalIrqState::new();

        assert!(!state.is_enabled());
        assert!(state.set_enabled(true));
        assert!(state.is_enabled());
        assert!(!state.set_enabled(true));
        assert!(state.set_enabled(false));
        assert!(!state.is_enabled());
        assert!(!state.set_enabled(false));
    }
}
