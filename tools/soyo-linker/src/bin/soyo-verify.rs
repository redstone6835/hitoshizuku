use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use soyo::{
    SignatureTrust, SignatureTrustPolicy, SliceSoyoReader, SoyoReadLimits, TrustedPublicKey,
    read_soyo, verify_metadata_signature,
};

const MAX_INPUT_SIZE: u64 = soyo::registry::MAX_FILE_SIZE;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("soyo-verify: {error}");
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<(), String> {
    let mut input = None;
    let mut allow_unsigned = false;
    let mut key_paths = Vec::new();
    let mut revoked = Vec::new();
    let mut rejected = Vec::new();
    let mut arguments = std::env::args_os().skip(1);
    while let Some(argument) = arguments.next() {
        if argument == "--allow-unsigned" {
            allow_unsigned = true;
        } else if argument == "--key" {
            key_paths.push(PathBuf::from(
                arguments.next().ok_or("--key 缺少参数")?,
            ));
        } else if argument == "--revoked-key-id" {
            revoked.push(parse_hex32(
                &arguments
                    .next()
                    .ok_or("--revoked-key-id 缺少参数")?
                    .to_string_lossy(),
            )?);
        } else if argument == "--reject-content-hash" {
            rejected.push(parse_hex32(
                &arguments
                    .next()
                    .ok_or("--reject-content-hash 缺少参数")?
                    .to_string_lossy(),
            )?);
        } else if argument.to_string_lossy().starts_with('-') {
            return Err(format!("未知选项 {}", argument.to_string_lossy()));
        } else if input.replace(PathBuf::from(argument)).is_some() {
            return Err("只能验证一个 SOYO 文件".into());
        }
    }
    let input = input.ok_or("缺少 SOYO 文件")?;
    let bytes = read_bounded(&input, MAX_INPUT_SIZE)?;
    let metadata = read_soyo(&SliceSoyoReader::new(&bytes), SoyoReadLimits::portable())
        .map_err(|error| format!("格式或摘要校验失败: {error:?}"))?;
    let trusted = key_paths
        .iter()
        .map(|path| read_public_key(path).map(TrustedPublicKey::new))
        .collect::<Result<Vec<_>, _>>()?;
    match verify_metadata_signature(
        &metadata,
        SignatureTrustPolicy {
            allow_unsigned,
            trusted_keys: &trusted,
            revoked_key_ids: &revoked,
            rejected_content_hashes: &rejected,
        },
    )
    .map_err(|error| format!("信任策略拒绝: {error:?}"))?
    {
        SignatureTrust::Unsigned => println!("unsigned"),
        SignatureTrust::Trusted { key_id } => println!("trusted {}", encode_hex(&key_id)),
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

fn read_public_key(path: &Path) -> Result<[u8; 32], String> {
    let bytes = read_bounded(path, 128)?;
    if bytes.len() == 32 {
        let mut key = [0; 32];
        key.copy_from_slice(&bytes);
        return Ok(key);
    }
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| format!("{} 不是 32 字节公钥或 64 位 hex", path.display()))?;
    parse_hex32(text.trim())
}

fn parse_hex32(text: &str) -> Result<[u8; 32], String> {
    if text.len() != 64 {
        return Err("需要 64 位 hex".into());
    }
    let mut bytes = [0; 32];
    for (index, byte) in bytes.iter_mut().enumerate() {
        let offset = index * 2;
        *byte = u8::from_str_radix(&text[offset..offset + 2], 16)
            .map_err(|_| "包含非 hex 字符".to_string())?;
    }
    Ok(bytes)
}

fn encode_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;

    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(output, "{byte:02x}").unwrap();
    }
    output
}
