use std::env;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    verify_sha256_implementation();
    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let (digest, file_count) = repository_interface_identity(&manifest_dir)
        .unwrap_or_else(|| bundled_interface_identity(&manifest_dir));
    let generated = format!(
        "/// 当前内核构建的 allocator/general/log/sched 规范源码 SHA-256。\n\
         pub const KERNEL_INTERFACE_SOURCE_SHA256: [u8; 32] = {digest:?};\n\
         /// 参与规范源码摘要的源码和 manifest 文件数量。\n\
         pub const KERNEL_INTERFACE_SOURCE_FILE_COUNT: usize = {file_count};\n"
    );
    let output = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("interface_source.rs");
    fs::write(&output, generated)
        .unwrap_or_else(|error| panic!("写入 {} 失败: {error}", output.display()));
}

fn repository_interface_identity(manifest_dir: &Path) -> Option<([u8; 32], usize)> {
    let repository = manifest_dir.join("../..");
    let allocator = repository.join("libs/allocator");
    let general = repository.join("general");
    let log = repository.join("libs/log");
    let sched = repository.join("libs/sched");
    if !allocator.join("src/lib.rs").is_file()
        || !general.join("src/dev/mod.rs").is_file()
        || !log.join("src/lib.rs").is_file()
        || !sched.join("src/lib.rs").is_file()
    {
        return None;
    }
    let mut files = Vec::new();
    collect_rust_sources(&allocator.join("src"), "allocator/src", &mut files);
    collect_rust_sources(&general.join("src/dev"), "general/src/dev", &mut files);
    collect_rust_sources(&log.join("src"), "log/src", &mut files);
    collect_rust_sources(&sched.join("src"), "sched/src", &mut files);
    files.push((
        "allocator/Cargo.toml".to_string(),
        allocator.join("Cargo.toml"),
    ));
    files.push(("general/Cargo.toml".to_string(), general.join("Cargo.toml")));
    files.push(("log/Cargo.toml".to_string(), log.join("Cargo.toml")));
    files.push(("sched/Cargo.toml".to_string(), sched.join("Cargo.toml")));
    files.sort_by(|left, right| left.0.cmp(&right.0));
    if files.is_empty() {
        panic!("没有找到 allocator/general/log/sched 规范接口源码");
    }

    let mut hash = Sha256::new();
    hash.update(b"ELM-KERNEL-EXACT-INTERFACE-V1\0RUST-MONOMORPHIZATION=LOCAL\0");
    for (logical_path, physical_path) in &files {
        println!("cargo:rerun-if-changed={}", physical_path.display());
        let contents = fs::read(physical_path)
            .unwrap_or_else(|error| panic!("读取 {} 失败: {error}", physical_path.display()));
        hash.update(&(logical_path.len() as u64).to_le_bytes());
        hash.update(logical_path.as_bytes());
        hash.update(&(contents.len() as u64).to_le_bytes());
        hash.update(&contents);
    }
    Some((hash.finish(), files.len()))
}

fn bundled_interface_identity(manifest_dir: &Path) -> ([u8; 32], usize) {
    let target = env::var("TARGET").unwrap_or_default();
    let target_path = manifest_dir.join(format!("interface.identity.{target}"));
    let path = if target_path.is_file() {
        target_path
    } else {
        manifest_dir.join("interface.identity")
    };
    println!("cargo:rerun-if-changed={}", path.display());
    let input = fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "外部 kernel-symbols 缺少精确接口身份文件 {}: {error}",
            path.display()
        )
    });
    let mut digest = None;
    let mut files = None;
    for line in input.lines() {
        if let Some(value) = line.strip_prefix("sha256=") {
            digest = Some(parse_sha256(value));
        } else if let Some(value) = line.strip_prefix("files=") {
            files = Some(
                value
                    .parse::<usize>()
                    .unwrap_or_else(|_| panic!("{} 的 files 字段无效", path.display())),
            );
        }
    }
    (
        digest.unwrap_or_else(|| panic!("{} 缺少 sha256 字段", path.display())),
        files.unwrap_or_else(|| panic!("{} 缺少 files 字段", path.display())),
    )
}

fn parse_sha256(value: &str) -> [u8; 32] {
    if value.len() != 64 {
        panic!("接口 sha256 必须包含 64 个十六进制字符");
    }
    let mut output = [0u8; 32];
    for (index, byte) in output.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
            .unwrap_or_else(|_| panic!("接口 sha256 包含非十六进制字符"));
    }
    output
}

fn collect_rust_sources(directory: &Path, prefix: &str, output: &mut Vec<(String, PathBuf)>) {
    let mut entries = fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("读取 {} 失败: {error}", directory.display()))
        .map(|entry| entry.unwrap())
        .collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let logical = format!("{prefix}/{name}");
        if path.is_dir() {
            collect_rust_sources(&path, &logical, output);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            output.push((logical, path));
        }
    }
}

fn verify_sha256_implementation() {
    let empty = Sha256::new().finish();
    assert_eq!(
        empty,
        [
            0xe3, 0xb0, 0xc4, 0x42, 0x98, 0xfc, 0x1c, 0x14, 0x9a, 0xfb, 0xf4, 0xc8, 0x99, 0x6f,
            0xb9, 0x24, 0x27, 0xae, 0x41, 0xe4, 0x64, 0x9b, 0x93, 0x4c, 0xa4, 0x95, 0x99, 0x1b,
            0x78, 0x52, 0xb8, 0x55,
        ],
        "内核接口摘要 SHA-256 空输入向量失败"
    );

    let mut split = Sha256::new();
    split.update(b"a");
    split.update(b"b");
    split.update(b"c");
    assert_eq!(
        split.finish(),
        [
            0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae,
            0x22, 0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61,
            0xf2, 0x00, 0x15, 0xad,
        ],
        "内核接口摘要 SHA-256 分段输入向量失败"
    );
}

struct Sha256 {
    state: [u32; 8],
    block: [u8; 64],
    block_len: usize,
    total_len: u64,
}

impl Sha256 {
    const fn new() -> Self {
        Self {
            state: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
                0x5be0cd19,
            ],
            block: [0; 64],
            block_len: 0,
            total_len: 0,
        }
    }

    fn update(&mut self, mut input: &[u8]) {
        self.total_len = self.total_len.checked_add(input.len() as u64).unwrap();
        if self.block_len != 0 {
            let take = (64 - self.block_len).min(input.len());
            self.block[self.block_len..self.block_len + take].copy_from_slice(&input[..take]);
            self.block_len += take;
            input = &input[take..];
            if self.block_len == 64 {
                let block = self.block;
                self.compress(&block);
                self.block_len = 0;
            } else {
                return;
            }
        }
        while input.len() >= 64 {
            let block: &[u8; 64] = input[..64].try_into().unwrap();
            self.compress(block);
            input = &input[64..];
        }
        self.block[..input.len()].copy_from_slice(input);
        self.block_len = input.len();
    }

    fn finish(mut self) -> [u8; 32] {
        let bit_len = self.total_len.checked_mul(8).unwrap();
        self.block[self.block_len] = 0x80;
        self.block_len += 1;
        if self.block_len > 56 {
            self.block[self.block_len..].fill(0);
            let block = self.block;
            self.compress(&block);
            self.block = [0; 64];
        } else {
            self.block[self.block_len..56].fill(0);
        }
        self.block[56..64].copy_from_slice(&bit_len.to_be_bytes());
        let block = self.block;
        self.compress(&block);
        let mut output = [0u8; 32];
        for (chunk, word) in output.chunks_exact_mut(4).zip(self.state) {
            chunk.copy_from_slice(&word.to_be_bytes());
        }
        output
    }

    fn compress(&mut self, block: &[u8; 64]) {
        const K: [u32; 64] = [
            0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
            0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
            0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
            0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
            0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
            0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
            0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
            0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
            0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
            0xc67178f2,
        ];
        let mut words = [0u32; 64];
        for (index, chunk) in block.chunks_exact(4).enumerate() {
            words[index] = u32::from_be_bytes(chunk.try_into().unwrap());
        }
        for index in 16..64 {
            let s0 = words[index - 15].rotate_right(7)
                ^ words[index - 15].rotate_right(18)
                ^ (words[index - 15] >> 3);
            let s1 = words[index - 2].rotate_right(17)
                ^ words[index - 2].rotate_right(19)
                ^ (words[index - 2] >> 10);
            words[index] = words[index - 16]
                .wrapping_add(s0)
                .wrapping_add(words[index - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
        for index in 0..64 {
            let sum1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choice = (e & f) ^ (!e & g);
            let temporary1 = h
                .wrapping_add(sum1)
                .wrapping_add(choice)
                .wrapping_add(K[index])
                .wrapping_add(words[index]);
            let sum0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let temporary2 = sum0.wrapping_add(majority);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temporary1);
            d = c;
            c = b;
            b = a;
            a = temporary1.wrapping_add(temporary2);
        }
        for (state, value) in self.state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *state = state.wrapping_add(value);
        }
    }
}
