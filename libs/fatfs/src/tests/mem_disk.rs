//! fatfs::BlockBackend 适配器。

extern crate std;

use crate::BlockBackend;
use crate::BlockBackendError;

impl BlockBackend for ktest_mock::MemDisk {
    fn sector_size(&self) -> u32 {
        self.sector_size()
    }

    fn sector_count(&self) -> u64 {
        self.sector_count()
    }

    fn read_sectors(&self, lba: u64, count: u32, buf: &mut [u8]) -> Result<(), BlockBackendError> {
        if self.read_sectors(lba, count, buf) {
            Ok(())
        } else {
            Err(BlockBackendError::OutOfRange)
        }
    }

    fn write_sectors(&self, lba: u64, count: u32, buf: &[u8]) -> Result<(), BlockBackendError> {
        if self.write_sectors(lba, count, buf) {
            Ok(())
        } else {
            Err(BlockBackendError::OutOfRange)
        }
    }
}
