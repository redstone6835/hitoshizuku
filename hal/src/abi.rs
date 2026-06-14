//! 平台 ABI 导出。

use general::vfs::stat::DevId;

pub fn decode_dev_t(dev: u64) -> DevId {
    arch::abi::decode_dev_t(dev)
}

pub fn encode_dev_t(dev: DevId) -> u64 {
    arch::abi::encode_dev_t(dev)
}
