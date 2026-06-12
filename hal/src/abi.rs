//! 平台 ABI 导出。

use general::vfs::stat::DevId;

pub fn decode_dev_t(dev: u64) -> DevId {
    #[cfg(target_arch = "loongarch64")]
    {
        arch::abi::decode_dev_t(dev)
    }

    #[cfg(target_arch = "riscv64")]
    {
        todo!("riscv64 decode_dev_t is not implemented")
    }
}

pub fn encode_dev_t(dev: DevId) -> u64 {
    #[cfg(target_arch = "loongarch64")]
    {
        arch::abi::encode_dev_t(dev)
    }

    #[cfg(target_arch = "riscv64")]
    {
        todo!("riscv64 encode_dev_t is not implemented")
    }
}
