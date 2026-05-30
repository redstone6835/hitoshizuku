/// 测试报告
pub struct KtestReport {
    pub total: usize,
    pub passed: usize,
}

pub struct KtestFailure {
    pub name: &'static str,
    pub file: &'static str,
    pub line: u32,
    pub msg: &'static str,
}

/// 运行所有注册在 .ktest section 中的测试。
/// 主机端：函数为空（测试由 cargo test 驱动）。
/// 内核端：遍历 linker section 执行。
pub fn run_all() -> KtestReport {
    KtestReport { total: 0, passed: 0 }
}
