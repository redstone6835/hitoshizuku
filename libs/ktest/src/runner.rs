#[cfg(feature = "kernel")]
use crate::KtestEntry;

// ── 主机端空壳 ──────────────────────────────────────────────────

#[cfg(not(feature = "kernel"))]
pub struct KtestReport {
    pub total: usize,
    pub passed: usize,
}

#[cfg(not(feature = "kernel"))]
pub fn run_all() -> KtestReport {
    KtestReport { total: 0, passed: 0 }
}

#[cfg(not(feature = "kernel"))]
pub fn set_writer(_w: fn(&[u8])) {}

// ── 内核端实现 ──────────────────────────────────────────────────

#[cfg(feature = "kernel")]
extern crate alloc;

#[cfg(feature = "kernel")]
pub struct KtestReport {
    pub total: usize,
    pub passed: usize,
}

#[cfg(feature = "kernel")]
static WRITER: spin::Mutex<Option<fn(&[u8])>> = spin::Mutex::new(None);

#[cfg(feature = "kernel")]
pub fn set_writer(w: fn(&[u8])) {
    *WRITER.lock() = Some(w);
}

#[cfg(feature = "kernel")]
fn tap_write(bytes: &[u8]) {
    if let Some(w) = *WRITER.lock() {
        w(bytes);
    }
}

#[cfg(feature = "kernel")]
pub fn run_all() -> KtestReport {
    unsafe extern "C" {
        static __start_ktest: KtestEntry;
        static __stop_ktest: KtestEntry;
    }

    let start = &raw const __start_ktest as *const KtestEntry;
    let end = &raw const __stop_ktest as *const KtestEntry;
    let count = unsafe { end.offset_from(start) } as usize;

    let header = alloc::format!("TAP version 14\n1..{}\n", count);
    tap_write(header.as_bytes());

    let mut passed = 0;

    for i in 0..count {
        let entry = unsafe { &*start.add(i) };
        // 先输出测试名，panic 时最后一行可定位失败测试
        let diag = alloc::format!("# {}\n", entry.name);
        tap_write(diag.as_bytes());

        (entry.func)();
        passed += 1;

        let ok_line = alloc::format!("ok {} - {}\n", i + 1, entry.name);
        tap_write(ok_line.as_bytes());
    }

    KtestReport { total: count, passed }
}
