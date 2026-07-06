//! 能力织网端口运行时描述。

use elm_model::PortDescriptor;

#[derive(Debug, Clone, Copy)]
pub(crate) struct PortRuntime {
    pub desc: PortDescriptor,
}

impl PortRuntime {
    pub const fn new(desc: PortDescriptor) -> Self {
        Self { desc }
    }
}
