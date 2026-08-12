use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use soyo::{SliceSoyoReader, SoyoReadLimits, read_soyo};
use soyo_linker::inspect::SoyoInspection;

const MAX_INPUT_SIZE: u64 = soyo::registry::MAX_FILE_SIZE;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("soyo-inspect: {error}");
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<(), String> {
    let mut input = None;
    let mut tsv = false;
    for argument in std::env::args_os().skip(1) {
        if argument == "--tsv" {
            tsv = true;
        } else if argument.to_string_lossy().starts_with('-') {
            return Err(format!("未知选项 {}", argument.to_string_lossy()));
        } else if input.replace(PathBuf::from(argument)).is_some() {
            return Err("只能检查一个 SOYO 文件".into());
        }
    }
    let input = input.ok_or("缺少 SOYO 文件")?;
    let bytes = read_bounded(&input, MAX_INPUT_SIZE)?;
    let metadata = read_soyo(&SliceSoyoReader::new(&bytes), SoyoReadLimits::portable())
        .map_err(|error| format!("格式或摘要校验失败: {error:?}"))?;
    let inspection = SoyoInspection::from_metadata(&metadata);
    if tsv {
        print!("{}", inspection.to_tsv());
    } else {
        print!("{}", inspection.to_text());
    }
    Ok(())
}

fn read_bounded(path: &Path, maximum: u64) -> Result<Vec<u8>, String> {
    let metadata = fs::metadata(path)
        .map_err(|error| format!("读取 {} 元数据失败: {error}", path.display()))?;
    if metadata.len() > maximum {
        return Err(format!("{} 超过 {maximum} 字节上限", path.display()));
    }
    let bytes = fs::read(path).map_err(|error| format!("读取 {} 失败: {error}", path.display()))?;
    if bytes.len() as u64 > maximum {
        return Err(format!("{} 在读取期间超过大小上限", path.display()));
    }
    Ok(bytes)
}
