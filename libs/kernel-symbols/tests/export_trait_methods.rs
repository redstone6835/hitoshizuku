trait CounterOps {
    fn increase(&self, value: usize) -> usize;
}

static DROPS: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

struct Droppable;

#[kernel_symbols::export]
impl Drop for Droppable {
    #[kernel_symbols::export(
        name = "test.Droppable.drop",
        contract = "test.drop@1",
        version = 1,
        capabilities = kernel_symbols::capability::CORE_SAFE
    )]
    fn drop(&mut self) {
        DROPS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    }
}

struct Counter;

#[kernel_symbols::export]
impl CounterOps for Counter {
    #[kernel_symbols::export(
        name = "test.CounterOps.increase",
        contract = "test.counter@1",
        version = 1,
        capabilities = kernel_symbols::capability::CORE_SAFE
    )]
    fn increase(&self, value: usize) -> usize {
        value + 1
    }
}

#[test]
fn exported_trait_methods_remain_callable() {
    assert_eq!(Counter.increase(41), 42);
    drop(Droppable);
    assert_eq!(DROPS.load(core::sync::atomic::Ordering::Relaxed), 1);
}
