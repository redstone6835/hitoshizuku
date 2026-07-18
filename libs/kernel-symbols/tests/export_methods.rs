use kernel_symbols::{KernelSymbolDescriptorV1, capability};
use std::sync::Arc;

struct Counter(u64);

#[kernel_symbols::export]
impl Counter {
    #[kernel_symbols::export(
        name = "tests.Counter.read",
        contract = "kernel.tests.counter@1",
        version = 1,
        capabilities = capability::CORE_SAFE
    )]
    pub fn read(&self) -> u64 {
        self.0
    }

    #[kernel_symbols::export(
        name = "tests.Counter.replace",
        contract = "kernel.tests.counter@1",
        version = 1,
        capabilities = capability::CORE_SAFE,
        flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
    )]
    pub fn replace(&mut self, value: u64) -> u64 {
        core::mem::replace(&mut self.0, value)
    }

    #[kernel_symbols::export(
        name = "tests.Counter.read_arc",
        contract = "kernel.tests.counter@1",
        version = 1,
        capabilities = capability::CORE_SAFE
    )]
    pub fn read_arc(self: &Arc<Self>) -> u64 {
        self.0
    }

    #[kernel_symbols::export(
        name = "tests.Counter.local_semantics",
        contract = "kernel.tests.counter@1",
        version = 1,
        capabilities = capability::CORE_SAFE
    )]
    pub fn local_semantics(&self, value: u64) -> u64 {
        let add = || self.0;
        let borrowed = &value;
        add() + *borrowed
    }
}

#[test]
fn exported_inherent_methods_remain_callable() {
    let mut counter = Counter(3);
    assert_eq!(counter.read(), 3);
    assert_eq!(counter.replace(7), 3);
    assert_eq!(counter.read(), 7);
    let counter = Arc::new(counter);
    assert_eq!(counter.read_arc(), 7);
    assert_eq!(counter.local_semantics(5), 12);
    assert!(core::mem::size_of::<KernelSymbolDescriptorV1>() >= 96);
}
