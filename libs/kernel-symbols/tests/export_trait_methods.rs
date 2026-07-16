trait CounterOps {
    fn increase(&self, value: usize) -> usize;
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
}
