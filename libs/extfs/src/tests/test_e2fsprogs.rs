//! 与 e2fsprogs 的交叉验证集成测试。
//!
//! 流程:mke2fs 建真镜像 → 手工注入 jbd2 事务(格式由 logdump 认可) →
//! 本驱动挂载恢复 → e2fsck -fn 验证干净;另有 e2fsck -fy 修复的对照组,
//! 比较两边恢复语义一致。找不到 e2fsprogs 工具时测试自动跳过。

extern crate std;

use std::format;
use std::sync::Arc;
use std::vec;
use std::vec::Vec;
use std::{fs, io, process::Command};

use ktest::ktest;
use vfs::cred::Credentials;
use vfs::file::OpenOptions;
use vfs::superblock::{FsDriver, Superblock as VfsSuperblock};

use super::extimg::*;
use crate::state::{BlockBackend, BlockBackendError, ExtFsDriver};

// ── 工具与文件 backend ─────────────────────────────────────────────────

fn have_tools() -> bool {
    ["mke2fs", "e2fsck", "debugfs"]
        .iter()
        .all(|t| Command::new(t).arg("-V").output().is_ok())
}

fn run(cmd: &str, args: &[&str]) -> std::string::String {
    let out = Command::new(cmd)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("无法执行 {cmd}: {e}"));
    std::string::String::from_utf8_lossy(&out.stdout).into_owned()
}

struct FileBackend {
    f: std::sync::Mutex<fs::File>,
    len: u64,
}

impl FileBackend {
    fn open(path: &std::path::Path) -> Self {
        let f = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .expect("打开镜像");
        let len = f.metadata().expect("metadata").len();
        Self {
            f: std::sync::Mutex::new(f),
            len,
        }
    }
}

impl BlockBackend for FileBackend {
    fn sector_size(&self) -> u32 {
        512
    }
    fn sector_count(&self) -> u64 {
        self.len / 512
    }
    fn read_sectors(&self, lba: u64, count: u32, buf: &mut [u8]) -> Result<(), BlockBackendError> {
        use io::{Read, Seek, SeekFrom};
        let need = count as usize * 512;
        if buf.len() < need
            || lba
                .checked_add(count as u64)
                .is_none_or(|e| e > self.sector_count())
        {
            return Err(BlockBackendError::OutOfRange);
        }
        let mut f = self.f.lock().expect("lock");
        f.seek(SeekFrom::Start(lba * 512))
            .map_err(|_| BlockBackendError::Io)?;
        f.read_exact(&mut buf[..need])
            .map_err(|_| BlockBackendError::Io)
    }
    fn write_sectors(&self, lba: u64, count: u32, buf: &[u8]) -> Result<(), BlockBackendError> {
        use io::{Seek, SeekFrom, Write};
        let need = count as usize * 512;
        if buf.len() < need
            || lba
                .checked_add(count as u64)
                .is_none_or(|e| e > self.sector_count())
        {
            return Err(BlockBackendError::OutOfRange);
        }
        let mut f = self.f.lock().expect("lock");
        f.seek(SeekFrom::Start(lba * 512))
            .map_err(|_| BlockBackendError::Io)?;
        f.write_all(&buf[..need]).map_err(|_| BlockBackendError::Io)
    }
}

// ── 镜像字节级读写 ─────────────────────────────────────────────────────

fn read_range(path: &std::path::Path, off: u64, len: usize) -> Vec<u8> {
    use io::{Read, Seek, SeekFrom};
    let mut f = fs::File::open(path).expect("打开镜像");
    f.seek(SeekFrom::Start(off)).expect("seek");
    let mut buf = vec![0u8; len];
    f.read_exact(&mut buf).expect("read");
    buf
}

fn write_range(path: &std::path::Path, off: u64, data: &[u8]) {
    use io::{Seek, SeekFrom, Write};
    let mut f = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .expect("打开镜像");
    f.seek(SeekFrom::Start(off)).expect("seek");
    f.write_all(data).expect("write");
}

fn read_block(path: &std::path::Path, blk: u64) -> Vec<u8> {
    read_range(path, blk * BS as u64, BS)
}

fn fs_uuid(img: &std::path::Path) -> [u8; 16] {
    let sb = read_range(img, 1024, 1024);
    let mut u = [0u8; 16];
    u.copy_from_slice(&sb[0x68..0x78]);
    u
}

fn fs_csum_seed(img: &std::path::Path) -> u32 {
    let sb = read_range(img, 1024, 1024);
    let incompat = rd32(&sb, 0x60);
    if incompat & 0x2000 != 0 {
        // INCOMPAT_CSUM_SEED:s_checksum_seed 在 0x270。
        rd32(&sb, 0x270)
    } else {
        crate::crc::crc32c(&fs_uuid(img))
    }
}

fn debugfs_bmap(img: &std::path::Path, file: &str, lb: u32) -> u64 {
    let spec = if lb == 0 {
        std::string::String::from(file)
    } else {
        std::string::String::from(file)
    };
    let _ = spec;
    let out = run(
        "debugfs",
        &[
            "-R",
            &std::string::String::from(format!("bmap {file} {lb}")),
            img.to_str().expect("path"),
        ],
    );
    out.lines()
        .last()
        .and_then(|l| l.trim().parse::<u64>().ok())
        .unwrap_or_else(|| panic!("解析 bmap 输出失败: {out}"))
}

fn debugfs_alloc_inode(img: &std::path::Path) -> u32 {
    // 扫 inode 位图找第一个空闲 inode。
    let gdt = read_range(img, GDT_BLOCK as u64 * BS as u64, 64);
    let inode_bitmap = rd32(&gdt, 4) as u64;
    let bm = read_block(img, inode_bitmap);
    for bit in 0..(INODES_PER_GROUP as usize).min(bm.len() * 8) {
        if bm[bit / 8] & (1u8 << (bit % 8)) == 0 {
            return bit as u32 + 1;
        }
    }
    panic!("没有空闲 inode");
}

// ── 主测试 ─────────────────────────────────────────────────────────────

/// 完整交叉验证:两个事务(改写 + 新建文件)的恢复,与 e2fsck 修复结果一致。
#[ktest]
fn e2fsprogs_cross_validate_journal_replay() {
    if !have_tools() {
        return;
    }
    let dir = std::env::temp_dir().join(format!("extfs-it-{}", std::process::id()));
    let _ = fs::create_dir_all(&dir);
    let img = dir.join("dirty.img");
    let img_ref = dir.join("ref.img");
    let payload = dir.join("payload.txt");

    // 1) 建镜像 + 写一个 hello.txt。
    fs::write(&payload, b"OLD!").expect("写 payload");
    run(
        "dd",
        &[
            "if=/dev/zero",
            &format!("of={}", img.display()),
            "bs=1M",
            "count=64",
            "status=none",
        ],
    );
    run(
        "mke2fs",
        &[
            "-t",
            "ext4",
            "-F",
            "-b",
            "4096",
            "-q",
            img.to_str().expect("p"),
        ],
    );
    run(
        "debugfs",
        &[
            "-w",
            "-R",
            &format!("write {} hello.txt", payload.display()),
            img.to_str().expect("p"),
        ],
    );

    // 2) 收集布局信息。
    let uuid = fs_uuid(&img);
    let seed = fs_csum_seed(&img);
    let helper = ExtImg::for_uuid(uuid);
    let hello_blk = debugfs_bmap(&img, "hello.txt", 0);
    let journal_blk = |lb: u32| debugfs_bmap(&img, "<8>", lb);

    // 3) 事务 1(seq=1):hello.txt 数据块 → "NEW!"。
    let d1 = {
        let mut d = vec![0u8; BS];
        d[..4].copy_from_slice(b"NEW!");
        d
    };
    let descr1 = helper.journal_descriptor(1, &[(hello_blk, 0, d1.clone())]);
    let commit1 = helper.journal_commit(1, 1_700_000_001, false);

    // 4) 事务 2(seq=2):创建 created.txt(inode 表 + 根目录叶块 + 位图 +
    //    gdt + 超级块 + 数据块)。
    let new_ino = debugfs_alloc_inode(&img);
    let created_blk = {
        // 找一个空闲数据块:扫块位图。
        let gdt = read_range(&img, GDT_BLOCK as u64 * BS as u64, 64);
        let block_bitmap = rd32(&gdt, 0) as u64;
        let bm = read_block(&img, block_bitmap);
        let mut found = None;
        for bit in 0..bm.len() * 8 {
            if bm[bit / 8] & (1u8 << (bit % 8)) == 0 {
                found = Some(bit as u64);
                break;
            }
        }
        found.expect("空闲数据块")
    };

    let gdt_blk = GDT_BLOCK as u64;
    let gdt = read_range(&img, gdt_blk * BS as u64, 64);
    let inode_table = rd32(&gdt, 8) as u64;
    let inode_bitmap_blk = rd32(&gdt, 4) as u64;
    let block_bitmap_blk = rd32(&gdt, 0) as u64;

    // 4a) inode 表块:写入 created.txt 的 inode 表项。
    let it_blk = inode_table + (new_ino as u64 - 1) * INODE_SIZE as u64 / BS as u64;
    let mut it_data = read_block(&img, it_blk);
    {
        let ib = extent_root(&[(0, 1, created_blk)]);
        let mut raw = ExtImg::make_inode(S_IFREG | 0o644, 1, 8, &ib, EXT4_EXTENTS_FL);
        le32(&mut raw, 28, 8);
        inode_csum(seed, new_ino, 0, &mut raw);
        let off = ((new_ino as usize - 1) * INODE_SIZE) % BS;
        it_data[off..off + INODE_SIZE].copy_from_slice(&raw);
    }

    // 4b) 根目录块(逻辑块 0,小目录无索引):分裂最后一个 entry 插入 created.txt。
    let leaf_phys = debugfs_bmap(&img, "/", 0);
    let mut leaf = read_block(&img, leaf_phys);
    {
        let mut off = 0usize;
        let data_end = BS - 12; // dir tail
        let mut last: Option<(usize, usize, usize)> = None; // (off, rec_len, name_len)
        loop {
            if off + 8 > data_end {
                break;
            }
            let rec_len = u16::from_le_bytes([leaf[off + 4], leaf[off + 5]]) as usize;
            if rec_len < 8 || off + rec_len > data_end {
                break;
            }
            let ino = rd32(&leaf, off);
            let name_len = leaf[off + 6] as usize;
            if ino != 0 && name_len != 0 {
                last = Some((off, rec_len, name_len));
            }
            off += rec_len;
        }
        let (loff, lrec, lnl) = last.expect("根目录块没有可用 entry");
        let needed = ((8 + lnl + 3) & !3) as u16;
        let new_off = loff + needed as usize;
        let entry_rec = (lrec - needed as usize) as u16;
        le16(&mut leaf, loff + 4, needed);
        le32(&mut leaf, new_off, new_ino);
        le16(&mut leaf, new_off + 4, entry_rec);
        leaf[new_off + 6] = 11;
        leaf[new_off + 7] = 1;
        leaf[new_off + 8..new_off + 19].copy_from_slice(b"created.txt");
        // 根目录 inode 的 generation 固定为 0(mke2fs)。
        dir_tail_csum(seed, 2, 0, &mut leaf);
    }

    // 4c) inode 位图块 + 块位图块。
    let mut ibm = read_block(&img, inode_bitmap_blk);
    ibm[((new_ino - 1) / 8) as usize] |= 1u8 << ((new_ino - 1) % 8);
    let mut bbm = read_block(&img, block_bitmap_blk);
    bbm[(created_blk / 8) as usize] |= 1u8 << (created_blk % 8);

    // 4d) gdt 块:新增一个常规文件会消耗一个数据块和一个 inode，
    // 同时 inode table 的未使用尾部缩短一项。三项计数都必须随事务回放，
    // 否则 e2fsck 会把新 inode 判为落在 unused-inodes 区域。
    let mut gdt_data = read_block(&img, gdt_blk);
    {
        let desc = &mut gdt_data[..64];
        let fb_lo = u16::from_le_bytes([desc[12], desc[13]]) as u32;
        let fb_hi = u16::from_le_bytes([desc[44], desc[45]]) as u32;
        let fi_lo = u16::from_le_bytes([desc[14], desc[15]]) as u32;
        let fi_hi = u16::from_le_bytes([desc[46], desc[47]]) as u32;
        let iu_lo = u16::from_le_bytes([desc[28], desc[29]]) as u32;
        let iu_hi = u16::from_le_bytes([desc[50], desc[51]]) as u32;
        let fb = ((fb_hi << 16) | fb_lo) - 1;
        let fi = ((fi_hi << 16) | fi_lo) - 1;
        let iu = ((iu_hi << 16) | iu_lo) - 1;
        desc[12..14].copy_from_slice(&(fb as u16).to_le_bytes());
        desc[44..46].copy_from_slice(&((fb >> 16) as u16).to_le_bytes());
        desc[14..16].copy_from_slice(&(fi as u16).to_le_bytes());
        desc[46..48].copy_from_slice(&((fi >> 16) as u16).to_le_bytes());
        desc[28..30].copy_from_slice(&(iu as u16).to_le_bytes());
        desc[50..52].copy_from_slice(&((iu >> 16) as u16).to_le_bytes());
        let sb = read_range(&img, 1024, 1024);
        let bpg = rd32(&sb, 0x20) as usize;
        let ipg = rd32(&sb, 0x28) as usize;
        let bb_csum = bitmap_csum(seed, &bbm[..bpg / 8]);
        let ib_csum = bitmap_csum(seed, &ibm[..ipg / 8]);
        desc[0x18..0x1a].copy_from_slice(&(bb_csum as u16).to_le_bytes());
        desc[0x1a..0x1c].copy_from_slice(&(ib_csum as u16).to_le_bytes());
        desc[0x38..0x3a].copy_from_slice(&((bb_csum >> 16) as u16).to_le_bytes());
        desc[0x3a..0x3c].copy_from_slice(&((ib_csum >> 16) as u16).to_le_bytes());
        let mut d64 = [0u8; 64];
        d64.copy_from_slice(desc);
        gdt_csum(seed, 0, &mut d64);
        desc.copy_from_slice(&d64);
    }

    // 4e) 超级块所在块(块 0):空闲计数 -1/-1,重算 sb csum。
    let mut blk0 = read_block(&img, 0);
    {
        let sb = &mut blk0[1024..2048];
        let free_b = rd32(sb, 0x0c) as u64 | ((rd32(sb, 0x158) as u64) << 32);
        let free_i = rd32(sb, 0x10);
        let free_b = free_b - 1;
        let free_i = free_i - 1;
        le32(sb, 0x0c, free_b as u32);
        le32(sb, 0x158, (free_b >> 32) as u32);
        le32(sb, 0x10, free_i);
        let sum = crate::crc::crc32c(&sb[..0x3fc]);
        le32(sb, 0x3fc, sum);
    }

    // 4f) created.txt 数据块。
    let d2 = {
        let mut d = vec![0u8; BS];
        d[..8].copy_from_slice(b"CREATED!");
        d
    };

    let descr2 = helper.journal_descriptor(
        2,
        &[
            (it_blk, 0, it_data.clone()),
            (leaf_phys, 0, leaf.clone()),
            (inode_bitmap_blk, 0, ibm.clone()),
            (block_bitmap_blk, 0, bbm.clone()),
            (gdt_blk, 0, gdt_data.clone()),
            (0, 0, blk0.clone()),
            (created_blk, 0, d2.clone()),
        ],
    );
    let commit2 = helper.journal_commit(2, 1_700_000_002, false);

    // 5) 注入日志物理块(日志逻辑块 1..N,从 jsb start=1 开始)。
    let journal_blocks: Vec<(u32, Vec<u8>)> = {
        let mut v = vec![(1u32, descr1), (2, d1), (3, commit1), (4, descr2)];
        let mut lb = 5u32;
        for data in [&it_data, &leaf, &ibm, &bbm, &gdt_data, &blk0, &d2] {
            v.push((lb, data.clone()));
            lb += 1;
        }
        v.push((lb, commit2));
        v
    };
    for (lb, content) in &journal_blocks {
        write_range(&img, journal_blk(*lb) * BS as u64, content);
    }
    // jsb:start=1, sequence=1,并开启 REVOKE+CSUM_V3(与 v3 构造匹配)。
    {
        let mut jsb = read_block(&img, journal_blk(0));
        be32(&mut jsb, 0x18, 1);
        be32(&mut jsb, 0x1c, 1);
        be32(&mut jsb, 0x28, 0x11); // feature_incompat = REVOKE | CSUM_V3
        jsb[0x50] = 4; // checksum type = crc32c
        let mut tmp = jsb[..1024].to_vec();
        tmp[0xfc..0x100].fill(0);
        let sum = crate::crc::crc32c(&tmp);
        be32(&mut jsb, 0xfc, sum);
        write_range(&img, journal_blk(0) * BS as u64, &jsb);
    }
    // 设置 needs_recovery。
    run(
        "debugfs",
        &[
            "-w",
            "-R",
            "features needs_recovery",
            img.to_str().expect("p"),
        ],
    );

    // 6) logdump 必须认可我们构造的日志(e2fsprogs 侧的格式验证)。
    let dump = run("debugfs", &["-R", "logdump", img.to_str().expect("p")]);
    assert!(
        dump.contains("Found expected sequence 1") || dump.contains("sequence 1"),
        "logdump 未识别事务 1: {dump}"
    );
    assert!(
        dump.contains("transaction 2") || dump.contains("sequence 2"),
        "logdump 未识别事务 2: {dump}"
    );

    // 7) 对照组:e2fsck 修复。
    fs::copy(&img, &img_ref).expect("复制镜像");
    run("e2fsck", &["-fy", img_ref.to_str().expect("p")]);

    // 8) 本驱动挂载恢复。
    let backend: Arc<dyn BlockBackend> = Arc::new(FileBackend::open(&img));
    let driver = ExtFsDriver::new();
    driver.bind_backend(backend);
    let sb: Arc<VfsSuperblock> = driver.mount(None, "").expect("挂载脏镜像");

    let read_file = |sb: &VfsSuperblock, name: &str| -> Vec<u8> {
        let inode = sb.root_inode.lookup(name).expect("lookup");
        let f = inode
            .open_ops(&OpenOptions::default(), &Credentials::root())
            .expect("open");
        let mut buf = vec![0u8; inode.size() as usize];
        let n = f.read_at(&mut buf, 0).expect("read");
        buf.truncate(n);
        buf
    };

    assert_eq!(read_file(&sb, "hello.txt"), b"NEW!", "事务 1 必须回放");
    assert_eq!(
        read_file(&sb, "created.txt"),
        b"CREATED!",
        "事务 2 必须回放"
    );
    driver.kill_sb(Arc::clone(&sb));

    // 9) 本驱动恢复后的镜像必须被 e2fsck 认为干净。
    let check = Command::new("e2fsck")
        .args(["-fn", img.to_str().expect("p")])
        .output()
        .expect("e2fsck");
    let report = std::string::String::from_utf8_lossy(&check.stdout).into_owned()
        + &std::string::String::from_utf8_lossy(&check.stderr);
    assert!(
        !report.contains("Block bitmap differences")
            && !report.contains("Free blocks count wrong")
            && !report.contains("Free inodes count wrong")
            && !report.contains("Recovery flag not set")
            && !report.contains("journal")
            && !report.contains("Journal"),
        "恢复后 e2fsck 仍报错:\n{report}"
    );

    // 10) 对照组语义一致:e2fsck 修复后 created.txt/hello.txt 内容相同。
    let ls = run("debugfs", &["-R", "ls -l /", img_ref.to_str().expect("p")]);
    assert!(ls.contains("created.txt"), "对照组缺少 created.txt: {ls}");
    let ref_hello = read_block(&img_ref, hello_blk);
    assert_eq!(&ref_hello[..4], b"NEW!", "对照组事务 1 回放结果不同");
    let ref_created = read_block(&img_ref, created_blk);
    assert_eq!(&ref_created[..8], b"CREATED!", "对照组事务 2 回放结果不同");
    // 两边 created.txt 的数据块内容必须一致。
    let our_created = read_block(&img, created_blk);
    assert_eq!(our_created, ref_created);

    let _ = fs::remove_dir_all(&dir);
}

/// fast commit 区域回放:CREAT 新建文件 + ADD_RANGE 扩展已有文件,
/// 与 e2fsck 的 fc 恢复结果对照。
#[ktest]
fn e2fsprogs_cross_validate_fast_commit() {
    if !have_tools() {
        return;
    }
    let dir = std::env::temp_dir().join(format!("extfs-it3-{}", std::process::id()));
    let _ = fs::create_dir_all(&dir);
    let img = dir.join("dirty.img");
    let img_ref = dir.join("ref.img");
    let payload = dir.join("payload.txt");

    // 1) 带 fast_commit 特性的镜像 + 已有 hello.txt。
    fs::write(&payload, b"OLD!").expect("写 payload");
    run(
        "dd",
        &[
            "if=/dev/zero",
            &format!("of={}", img.display()),
            "bs=1M",
            "count=64",
            "status=none",
        ],
    );
    run(
        "mke2fs",
        &[
            "-t",
            "ext4",
            "-F",
            "-b",
            "4096",
            "-O",
            "fast_commit",
            "-q",
            img.to_str().expect("p"),
        ],
    );
    run(
        "debugfs",
        &[
            "-w",
            "-R",
            &format!("write {} hello.txt", payload.display()),
            img.to_str().expect("p"),
        ],
    );

    let uuid = fs_uuid(&img);
    let helper = ExtImg::for_uuid(uuid);
    let hello_blk = debugfs_bmap(&img, "hello.txt", 0);
    let journal_blk = |lb: u32| debugfs_bmap(&img, "<8>", lb);
    let seed = fs_csum_seed(&img);

    // hello.txt 的 inode 号(debugfs stat 第一行 "Inode: N")。
    let stat = run(
        "debugfs",
        &["-R", "stat hello.txt", img.to_str().expect("p")],
    );
    let hello_ino: u32 = stat
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .expect("解析 hello inode 号");

    // 2) fc 区域:CREAT 新文件 + ADD_RANGE 扩展 hello.txt。
    let new_ino = debugfs_alloc_inode(&img);
    let added_blk = {
        let gdt = read_range(&img, GDT_BLOCK as u64 * BS as u64, 64);
        let block_bitmap = rd32(&gdt, 0) as u64;
        let bm = read_block(&img, block_bitmap);
        let mut found = None;
        for bit in 0..bm.len() * 8 {
            if bm[bit / 8] & (1u8 << (bit % 8)) == 0 {
                found = Some(bit as u64);
                break;
            }
        }
        found.expect("空闲块")
    };

    // hello.txt 新快照:size 扩到 2 块。
    let gdt = read_range(&img, GDT_BLOCK as u64 * BS as u64, 64);
    let inode_table = rd32(&gdt, 8) as u64;
    let it_blk = inode_table + (hello_ino as u64 - 1) * INODE_SIZE as u64 / BS as u64;
    let it_data = read_block(&img, it_blk);
    let off = ((hello_ino as usize - 1) * INODE_SIZE) % BS;
    let mut hello_raw = [0u8; INODE_SIZE];
    hello_raw.copy_from_slice(&it_data[off..off + INODE_SIZE]);
    le32(&mut hello_raw, 4, (2 * BS) as u32); // i_size = 8192
    le32(&mut hello_raw, 108, 0);
    // 新文件 inode(CREAT 目标)。
    let ib = extent_root(&[]);
    let new_raw = ExtImg::make_inode(S_IFREG | 0o644, 1, 0, &ib, EXT4_EXTENTS_FL);

    let fc1 = helper.fc_block(
        1,
        &[
            ExtImg::fc_add_range(hello_ino, 1, 1, added_blk),
            ExtImg::fc_inode(hello_ino, &hello_raw),
            ExtImg::fc_inode(new_ino, &new_raw),
            ExtImg::fc_dentry(3, 2, new_ino, b"fcfile.txt"),
        ],
    );

    // 3) 注入:普通日志区空(start=1 指向空块),fc 区在末尾 4 块。
    {
        // jsb:start=1, sequence=1, REVOKE|CSUM_V3|FAST_COMMIT, num_fc=4。
        let mut jsb = read_block(&img, journal_blk(0));
        be32(&mut jsb, 0x18, 1);
        be32(&mut jsb, 0x1c, 1);
        be32(&mut jsb, 0x28, 0x31); // 0x1|0x10|0x20
        jsb[0x50] = 4;
        be32(&mut jsb, 0x54, 4);
        let mut tmp = jsb[..1024].to_vec();
        tmp[0xfc..0x100].fill(0);
        let sum = crate::crc::crc32c(&tmp);
        be32(&mut jsb, 0xfc, sum);
        write_range(&img, journal_blk(0) * BS as u64, &jsb);
        // fc 区第一块(maxlen-3)。
        let maxlen = rdb32(&jsb, 0x10);
        write_range(&img, journal_blk(maxlen - 3) * BS as u64, &fc1);
    }
    run(
        "debugfs",
        &[
            "-w",
            "-R",
            "features needs_recovery",
            img.to_str().expect("p"),
        ],
    );

    // 4) 对照组:e2fsck 修复(1.47 支持 fc 回放)。
    fs::copy(&img, &img_ref).expect("复制镜像");
    run("e2fsck", &["-fy", img_ref.to_str().expect("p")]);

    // 5) 本驱动挂载恢复。
    let backend: Arc<dyn BlockBackend> = Arc::new(FileBackend::open(&img));
    let driver = ExtFsDriver::new();
    driver.bind_backend(backend);
    let sb: Arc<VfsSuperblock> = driver.mount(None, "").expect("挂载 fc 脏镜像");

    // fcfile.txt 必须被 CREAT 出来。
    let fc_inode = sb
        .root_inode
        .lookup("fcfile.txt")
        .expect("fc 回放应创建 fcfile.txt");
    assert_eq!(fc_inode.kind(), vfs::stat::FileType::Regular);
    // hello.txt 必须被 ADD_RANGE + INODE 扩展到 8192。
    let hello_inode = sb.root_inode.lookup("hello.txt").expect("lookup hello.txt");
    assert_eq!(
        hello_inode.size(),
        (2 * BS) as u64,
        "ADD_RANGE+INODE 必须扩展 size"
    );
    driver.kill_sb(Arc::clone(&sb));

    // 6) 块位图:added_blk 必须被置位(两个镜像一致)。
    let gdt = read_range(&img, GDT_BLOCK as u64 * BS as u64, 64);
    let block_bitmap = rd32(&gdt, 0) as u64;
    let bm = read_block(&img, block_bitmap);
    assert!(
        bm[(added_blk / 8) as usize] & (1u8 << (added_blk % 8)) != 0,
        "ADD_RANGE 的物理块必须被标记为已用"
    );
    // 对照组 fcfile.txt 也必须存在。
    let ls = run("debugfs", &["-R", "ls -l /", img_ref.to_str().expect("p")]);
    assert!(ls.contains("fcfile.txt"), "对照组缺少 fcfile.txt: {ls}");

    // 7) 本驱动恢复后 e2fsck 必须认为干净。
    let check = Command::new("e2fsck")
        .args(["-fn", img.to_str().expect("p")])
        .output()
        .expect("e2fsck");
    let report = std::string::String::from_utf8_lossy(&check.stdout).into_owned()
        + &std::string::String::from_utf8_lossy(&check.stderr);
    assert!(
        !report.contains("journal") && !report.contains("Journal") && !report.contains("wrong"),
        "fc 恢复后 e2fsck 仍报错:\n{report}"
    );

    let _ = fs::remove_dir_all(&dir);
    let _ = seed;
    let _ = hello_blk;
}

/// 撕裂提交 + revoke 在真实镜像上的行为。
#[ktest]
fn e2fsprogs_cross_validate_torn_and_revoke() {
    if !have_tools() {
        return;
    }
    let dir = std::env::temp_dir().join(format!("extfs-it2-{}", std::process::id()));
    let _ = fs::create_dir_all(&dir);
    let img = dir.join("dirty.img");
    let payload = dir.join("payload.txt");

    fs::write(&payload, b"OLD!").expect("写 payload");
    run(
        "dd",
        &[
            "if=/dev/zero",
            &format!("of={}", img.display()),
            "bs=1M",
            "count=64",
            "status=none",
        ],
    );
    run(
        "mke2fs",
        &[
            "-t",
            "ext4",
            "-F",
            "-b",
            "4096",
            "-q",
            img.to_str().expect("p"),
        ],
    );
    run(
        "debugfs",
        &[
            "-w",
            "-R",
            &format!("write {} hello.txt", payload.display()),
            img.to_str().expect("p"),
        ],
    );

    let uuid = fs_uuid(&img);
    let helper = ExtImg::for_uuid(uuid);
    let hello_blk = debugfs_bmap(&img, "hello.txt", 0);
    let journal_blk = |lb: u32| debugfs_bmap(&img, "<8>", lb);

    // 事务 1(seq=1):改写 + revoke(应被取消);事务 2(seq=2):撕裂提交(应被截断)。
    let d1 = {
        let mut d = vec![0u8; BS];
        d[..4].copy_from_slice(b"REV!");
        d
    };
    let descr1 = helper.journal_descriptor(1, &[(hello_blk, 0, d1.clone())]);
    let revoke1 = helper.journal_revoke(1, &[hello_blk]);
    let commit1 = helper.journal_commit(1, 1_700_000_001, false);
    let d2 = {
        let mut d = vec![0u8; BS];
        d[..4].copy_from_slice(b"BAD!");
        d
    };
    let descr2 = helper.journal_descriptor(2, &[(hello_blk, 0, d2.clone())]);
    let commit2 = helper.journal_commit(2, 1_700_000_002, true);

    let blocks: Vec<(u32, Vec<u8>)> = vec![
        (1, descr1),
        (2, d1),
        (3, revoke1),
        (4, commit1),
        (5, descr2),
        (6, d2),
        (7, commit2),
    ];
    for (lb, content) in &blocks {
        write_range(&img, journal_blk(*lb) * BS as u64, content);
    }
    {
        let mut jsb = read_block(&img, journal_blk(0));
        be32(&mut jsb, 0x18, 1);
        be32(&mut jsb, 0x1c, 1);
        be32(&mut jsb, 0x28, 0x11); // REVOKE | CSUM_V3
        jsb[0x50] = 4;
        let mut tmp = jsb[..1024].to_vec();
        tmp[0xfc..0x100].fill(0);
        let sum = crate::crc::crc32c(&tmp);
        be32(&mut jsb, 0xfc, sum);
        write_range(&img, journal_blk(0) * BS as u64, &jsb);
    }
    run(
        "debugfs",
        &[
            "-w",
            "-R",
            "features needs_recovery",
            img.to_str().expect("p"),
        ],
    );

    let backend: Arc<dyn BlockBackend> = Arc::new(FileBackend::open(&img));
    let driver = ExtFsDriver::new();
    driver.bind_backend(backend);
    let sb = driver.mount(None, "").expect("挂载脏镜像");
    let inode = sb.root_inode.lookup("hello.txt").expect("lookup");
    let f = inode
        .open_ops(&OpenOptions::default(), &Credentials::root())
        .expect("open");
    let mut buf = vec![0u8; inode.size() as usize];
    let n = f.read_at(&mut buf, 0).expect("read");
    buf.truncate(n);
    assert_eq!(buf, b"OLD!", "revoke + 撕裂提交都必须阻止回放");
    driver.kill_sb(Arc::clone(&sb));

    let _ = fs::remove_dir_all(&dir);
}
