//! 设备子系统通用稳定命名工具。
//!
//! 这里处理的是“某一类内核可见对象的稳定短名”，例如块设备的 `vd0`、串口的
//! `uart0` 或网络接口的 `eth0`。命名器只接受类别前缀和硬件稳定 key，不理解
//! devtmpfs、设备号、sysfs 路径或具体驱动协议，因此可以被不同 function 层复用。

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

use spin::mutex::Mutex;

/// 稳定命名分配器。
///
/// `prefix` 由具体类别声明，`stable_key` 由 PnP identity、固件路径或总线地址提供。
/// 同一个 key 多次分配会复用同一个短名，避免驱动解绑、依赖重试或热插重扫导致
/// 用户可见名称随 probe 顺序漂移。
///
/// 分配状态按前缀全局共享，而不是按驱动实例保存。多个驱动如果都声明同一个
/// `prefix`，会自然消费同一组编号，避免 `uart0`/`uart0` 或 `vd0`/`vd0` 这类
/// 兼容层节点名冲突。`StableNameAllocator` 本身只是一份轻量的前缀声明。
pub struct StableNameAllocator {
    prefix: &'static str,
}

struct StableNameReservation {
    stable_key: String,
    name: StableName,
}

struct StableNamePrefixState {
    prefix: &'static str,
    next_index: usize,
    reservations: Vec<StableNameReservation>,
}

impl StableNamePrefixState {
    const fn new(prefix: &'static str) -> Self {
        Self {
            prefix,
            next_index: 0,
            reservations: Vec::new(),
        }
    }
}

static STABLE_NAME_PREFIXES: Mutex<Vec<StableNamePrefixState>> = Mutex::new(Vec::new());

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
        Self { prefix }
    }

    /// 分配下一个短名，例如 `uart0` 或 `vd1`。
    pub fn try_alloc(&self) -> Result<StableName, StableNameAllocError> {
        let mut prefixes = STABLE_NAME_PREFIXES.lock();
        let state = prefix_state_mut(&mut prefixes, self.prefix)?;
        let index = state.next_index;
        let next = index
            .checked_add(1)
            .ok_or(StableNameAllocError::OutOfMemory)?;
        let name = try_build_name(self.prefix, index)?;
        state.next_index = next;
        Ok(StableName { index, name })
    }

    /// 为稳定设备身份分配或复用短名。
    pub fn try_alloc_stable(&self, stable_key: &str) -> Result<StableName, StableNameAllocError> {
        let mut prefixes = STABLE_NAME_PREFIXES.lock();
        let state = prefix_state_mut(&mut prefixes, self.prefix)?;
        if let Some(existing) = state
            .reservations
            .iter()
            .find(|reservation| reservation.stable_key == stable_key)
        {
            return try_clone_name(&existing.name);
        }

        let stable_key = try_clone_string(stable_key)?;
        state
            .reservations
            .try_reserve(1)
            .map_err(|_| StableNameAllocError::OutOfMemory)?;
        let (name, reserved_name) = try_alloc_pair(state, self.prefix)?;
        state.reservations.push(StableNameReservation {
            stable_key,
            name: reserved_name,
        });
        Ok(name)
    }

    /// 返回该分配器负责的类别前缀，供日志或诊断使用。
    pub const fn prefix(&self) -> &'static str {
        self.prefix
    }
}

fn prefix_state_mut<'a>(
    prefixes: &'a mut Vec<StableNamePrefixState>,
    prefix: &'static str,
) -> Result<&'a mut StableNamePrefixState, StableNameAllocError> {
    if let Some(index) = prefixes.iter().position(|state| state.prefix == prefix) {
        return Ok(&mut prefixes[index]);
    }

    prefixes
        .try_reserve(1)
        .map_err(|_| StableNameAllocError::OutOfMemory)?;
    prefixes.push(StableNamePrefixState::new(prefix));
    prefixes.last_mut().ok_or(StableNameAllocError::OutOfMemory)
}

fn try_build_name(prefix: &str, index: usize) -> Result<String, StableNameAllocError> {
    let len = prefix
        .len()
        .checked_add(decimal_digits(index))
        .ok_or(StableNameAllocError::OutOfMemory)?;
    let mut name = String::new();
    name.try_reserve(len)
        .map_err(|_| StableNameAllocError::OutOfMemory)?;
    name.push_str(prefix);
    write!(&mut name, "{}", index).map_err(|_| StableNameAllocError::OutOfMemory)?;
    Ok(name)
}

fn try_alloc_pair(
    state: &mut StableNamePrefixState,
    prefix: &str,
) -> Result<(StableName, StableName), StableNameAllocError> {
    let index = state.next_index;
    let next = index
        .checked_add(1)
        .ok_or(StableNameAllocError::OutOfMemory)?;
    let public_name = try_build_name(prefix, index)?;
    let reserved_name = try_build_name(prefix, index)?;
    state.next_index = next;
    Ok((
        StableName {
            index,
            name: public_name,
        },
        StableName {
            index,
            name: reserved_name,
        },
    ))
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
