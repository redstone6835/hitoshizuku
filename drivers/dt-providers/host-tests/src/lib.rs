//! DT fixed provider 驱动的宿主侧契约测试入口。

extern crate alloc;

#[cfg(test)]
pub(crate) use general::dev;

// The harness imports the complete driver module to run its host-only contract tests.
#[cfg(test)]
#[allow(dead_code)]
#[path = "../../src/driver.rs"]
mod driver;
