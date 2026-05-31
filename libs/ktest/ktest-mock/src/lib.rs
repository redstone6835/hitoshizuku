//! ktest-mock —— 内核测试 mock 辅助类型。

#![no_std]

extern crate alloc;

pub mod mem_disk;

pub use mem_disk::MemDisk;

#[cfg(test)]
mod tests;
