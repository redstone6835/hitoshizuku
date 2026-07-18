//! ELM 事件队列辅助函数。

use elm_model::ElmEventRecord;
use errno::Errno;

use super::core::ElmCore;

pub(crate) fn read_next_event(core: &ElmCore) -> Result<ElmEventRecord, Errno> {
    core.read_next_event().ok_or(Errno::EAGAIN)
}

pub(crate) fn ack_event(core: &mut ElmCore, sequence: u64) {
    core.ack_event(sequence);
}
