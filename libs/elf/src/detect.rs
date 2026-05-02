//! magic 嗅探 + 顶层 [`parse`] 分派。
//!
//! 本模块是"新格式接入点"。加一种格式流程：
//!
//! 1. 在 `libs/elf/src/<new_fmt>/` 建子树，暴露 `parse(bytes) -> Result<X, ElfError>`
//!    与 `impl Image for X`。
//! 2. 在 [`Kind`] 加变体，在 [`detect`] 加 magic arm。
//! 3. 在 [`parse`] 的 match 加对应分支。
//!
//! 调用方的类型契约（`Box<dyn Image>`）不动，零兼容破坏。

use alloc::boxed::Box;

use crate::error::ElfError;
use crate::image::Image;

/// ELF 的 `\x7fELF` magic。
const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];

// 未来格式的保留位
// const MYGO_MAGIC: [u8; 4] = *b"MYGO";

/// 已识别的格式种类。解析器内部使用。
enum Kind {
    Elf,
    // Mygo,
}

/// 读前 4 字节决定走哪个解析器。
fn detect(bytes: &[u8]) -> Result<Kind, ElfError> {
    if bytes.len() < 4 {
        return Err(ElfError::TooShort);
    }
    if bytes[..4] == ELF_MAGIC {
        return Ok(Kind::Elf);
    }
    // if bytes[..4] == MYGO_MAGIC { return Ok(Kind::Mygo); }
    Err(ElfError::BadMagic)
}

/// 从字节流构造一个格式无关的 [`Image`] trait 对象。
///
/// 返回的 [`Box<dyn Image>`] 借用 `bytes`——调用方需确保 image 字节在
/// trait object 活跃期间不释放。
pub fn parse<'a>(bytes: &'a [u8]) -> Result<Box<dyn Image<'a> + 'a>, ElfError> {
    match detect(bytes)? {
        Kind::Elf => {
            let img = crate::linux::LinuxElfImage::parse(bytes)?;
            Ok(Box::new(img))
        } // Kind::Mygo => { let img = crate::mygo::parse(bytes)?; Ok(Box::new(img)) }
    }
}
