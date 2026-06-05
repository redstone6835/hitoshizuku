#![no_std]

pub mod runner;

/// 测试入口描述。内核端由 linker section 自动收集。
#[repr(C)]
pub struct KtestEntry {
    pub name: &'static str,
    pub file: &'static str,
    pub line: u32,
    pub func: fn(),
}

/// 重新导出宏，使使用者只需 `use ktest::ktest;`
pub use ktest_macro::ktest;
