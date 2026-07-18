//! 平台 ABI 导出。

use general::vfs::stat::DevId;

#[kernel_symbols::export(name = "hal.abi.decode_dev_t", contract = "kernel.hal.abi@1", version = 1, capabilities = kernel_symbols::capability::HAL_QUERY)]
pub fn decode_dev_t(dev: u64) -> DevId {
    arch::abi::decode_dev_t(dev)
}

#[kernel_symbols::export(name = "hal.abi.encode_dev_t", contract = "kernel.hal.abi@1", version = 1, capabilities = kernel_symbols::capability::HAL_QUERY)]
pub fn encode_dev_t(dev: DevId) -> u64 {
    arch::abi::encode_dev_t(dev)
}
