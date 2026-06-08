//! 设备子系统通用稳定命名工具。
//!
//! 这里处理的是“某一类内核可见对象的稳定短名”，例如块设备的 `vd0`、串口的
//! `uart0` 或网络接口的 `eth0`。命名器只接受类别前缀和硬件稳定 key，不理解
//! devtmpfs、设备号、sysfs 路径或具体驱动协议，因此可以被不同 function 层复用。

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicUsize, Ordering};

use spin::mutex::Mutex;

/// 稳定命名分配器。
///
/// `prefix` 由具体类别声明，`stable_key` 由 PnP identity、固件路径或总线地址提供。
/// 同一个 key 多次分配会复用同一个短名，避免驱动解绑、依赖重试或热插重扫导致
/// 用户可见名称随 probe 顺序漂移。
pub struct StableNameAllocator {
    prefix: &'static str,
    next_index: AtomicUsize,
    reservations: Mutex<Vec<StableNameReservation>>,
}

struct StableNameReservation {
    stable_key: String,
    name: StableName,
}

/// 一次稳定命名分配的结果。
///
/// `name` 是类别内短名，`index` 是同一分配器内的稳定序号。调用方需要建立主设备
/// 别名时应读取 index，而不是解析字符串后缀。
#[derive(Debug, PartialEq, Eq)]
pub struct StableName {
    index: usize,
    name: String,
}

impl StableName {
    pub const fn index(&self) -> usize {
        self.index
    }

    pub fn as_str(&self) -> &str {
        &self.name
    }

    pub fn into_string(self) -> String {
        self.name
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StableNameAllocError {
    /// 记录稳定 key 与短名映射时分配失败，或序号空间已耗尽。
    OutOfMemory,
}

impl StableNameAllocator {
    pub const fn new(prefix: &'static str) -> Self {
        Self {
            prefix,
            next_index: AtomicUsize::new(0),
            reservations: Mutex::new(Vec::new()),
        }
    }

    /// 分配下一个短名，例如 `uart0` 或 `vd1`。
    pub fn try_alloc(&self) -> Result<StableName, StableNameAllocError> {
        loop {
            let index = self.next_index.load(Ordering::Relaxed);
            let next = index
                .checked_add(1)
                .ok_or(StableNameAllocError::OutOfMemory)?;
            let name = self.try_build_name(index)?;
            if self
                .next_index
                .compare_exchange(index, next, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                return Ok(StableName { index, name });
            }
        }
    }

    /// 为稳定设备身份分配或复用短名。
    pub fn try_alloc_stable(&self, stable_key: &str) -> Result<StableName, StableNameAllocError> {
        let mut reservations = self.reservations.lock();
        if let Some(existing) = reservations
            .iter()
            .find(|reservation| reservation.stable_key == stable_key)
        {
            return try_clone_name(&existing.name);
        }

        let stable_key = try_clone_string(stable_key)?;
        reservations
            .try_reserve(1)
            .map_err(|_| StableNameAllocError::OutOfMemory)?;
        let (name, reserved_name) = self.try_alloc_pair()?;
        reservations.push(StableNameReservation {
            stable_key,
            name: reserved_name,
        });
        Ok(name)
    }

    /// 返回该分配器负责的类别前缀，供日志或诊断使用。
    pub const fn prefix(&self) -> &'static str {
        self.prefix
    }

    fn try_build_name(&self, index: usize) -> Result<String, StableNameAllocError> {
        let len = self
            .prefix
            .len()
            .checked_add(decimal_digits(index))
            .ok_or(StableNameAllocError::OutOfMemory)?;
        let mut name = String::new();
        name.try_reserve(len)
            .map_err(|_| StableNameAllocError::OutOfMemory)?;
        name.push_str(self.prefix);
        write!(&mut name, "{}", index).map_err(|_| StableNameAllocError::OutOfMemory)?;
        Ok(name)
    }

    fn try_alloc_pair(&self) -> Result<(StableName, StableName), StableNameAllocError> {
        loop {
            let index = self.next_index.load(Ordering::Relaxed);
            let next = index
                .checked_add(1)
                .ok_or(StableNameAllocError::OutOfMemory)?;
            let public_name = self.try_build_name(index)?;
            let reserved_name = self.try_build_name(index)?;
            if self
                .next_index
                .compare_exchange(index, next, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                return Ok((
                    StableName {
                        index,
                        name: public_name,
                    },
                    StableName {
                        index,
                        name: reserved_name,
                    },
                ));
            }
        }
    }
}

fn decimal_digits(mut value: usize) -> usize {
    let mut digits = 1;
    while value >= 10 {
        value /= 10;
        digits += 1;
    }
    digits
}

fn try_clone_name(name: &StableName) -> Result<StableName, StableNameAllocError> {
    Ok(StableName {
        index: name.index,
        name: try_clone_string(&name.name)?,
    })
}

fn try_clone_string(value: &str) -> Result<String, StableNameAllocError> {
    let mut out = String::new();
    out.try_reserve(value.len())
        .map_err(|_| StableNameAllocError::OutOfMemory)?;
    out.push_str(value);
    Ok(out)
}
