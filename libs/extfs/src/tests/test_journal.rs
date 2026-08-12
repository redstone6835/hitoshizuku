//! JBD2 日志恢复、fast commit 回放与孤儿清理的镜像级测试。
//!
//! 所有镜像由 [`super::extimg`] 在内存中构造,不依赖外部工具。

extern crate std;

use std::sync::Arc;
use std::vec;
use std::vec::Vec;

use ktest::ktest;
use ktest_mock::MemDisk;
use vfs::cred::Credentials;
use vfs::file::OpenOptions;
use vfs::stat::FileType;
use vfs::superblock::{FsDriver, Superblock as VfsSuperblock};

use super::extimg::*;
use crate::state::{BlockBackend, ExtFsDriver};

struct Mounted {
    driver: ExtFsDriver,
    sb: Arc<VfsSuperblock>,
    disk: Arc<MemDisk>,
}

fn mount(img: ExtImg) -> Mounted {
    let disk = Arc::new(MemDisk::from_bytes(img.data, 512));
    let driver = ExtFsDriver::new();
    let backend: Arc<dyn BlockBackend> = disk.clone();
    driver.bind_backend(backend);
    let sb = driver.mount(None, "").expect("挂载带日志的 ext4 镜像");
    Mounted { driver, sb, disk }
}

fn unmount(m: Mounted) -> Vec<u8> {
    m.driver.kill_sb(Arc::clone(&m.sb));
    m.disk.dump()
}

fn read_file(sb: &VfsSuperblock, name: &str) -> Vec<u8> {
    let inode = sb.root_inode.lookup(name).expect("lookup 文件");
    let f = inode
        .open_ops(&OpenOptions::default(), &Credentials::root())
        .expect("打开文件");
    let mut buf = vec![0u8; inode.size() as usize];
    let n = f.read_at(&mut buf, 0).expect("读文件");
    buf.truncate(n);
    buf
}

/// 日志超级块在镜像中的字节区间。
fn journal_sb_range() -> (usize, usize) {
    let start = ExtImg::journal_phys(0) as usize * BS;
    (start, start + BS)
}

/// 造一个 extent 文件(单块),加入根目录并返回其 inode 号。
fn add_file(img: &mut ExtImg, ino: u32, name: &[u8], data_block: u32, content: &[u8]) {
    let ib = extent_root(&[(0, 1, data_block as u64)]);
    let mut raw = ExtImg::make_inode(
        S_IFREG | 0o644,
        1,
        content.len() as u64,
        &ib,
        EXT4_EXTENTS_FL,
    );
    le32(&mut raw, 28, 8);
    img.write_inode(ino, &raw);
    img.set_inode_used(ino, true);
    img.set_block_used(data_block, true);
    let blk = img.block_mut(data_block);
    blk[..content.len()].copy_from_slice(content);
    img.add_root_entry(ino, 1, name);
}

/// 日志事务注入:v3 descriptor + data + commit,seq 从 1 开始。
fn inject_simple_txn(
    img: &mut ExtImg,
    seq: u32,
    start: u32,
    tags: &[(u64, u16, Vec<u8>)],
    corrupt_commit: bool,
) {
    let descr = img.journal_descriptor(seq, tags);
    let mut blocks: Vec<(u32, Vec<u8>)> = vec![(start, descr)];
    let mut lb = start + 1;
    for (_, _, data) in tags {
        blocks.push((lb, data.clone()));
        lb += 1;
    }
    blocks.push((lb, img.journal_commit(seq, 1_700_000_000, corrupt_commit)));
    img.inject_journal(&blocks);
    img.set_jsb_start(seq, start);
    img.set_recover(true);
}

/// 回放一个已提交事务:数据块内容被改写,日志头复位,RECOVER 清除。
#[ktest]
fn recover_replays_committed_transaction() {
    let mut img = ExtImg::new();
    add_file(&mut img, 12, b"hello", FILE_DATA_BLOCK, b"OLD!");
    // 事务 1:把 FILE_DATA_BLOCK 改写为 "NEW!"。
    let data = {
        let mut d = vec![0u8; BS];
        d[..4].copy_from_slice(b"NEW!");
        d
    };
    inject_simple_txn(&mut img, 1, 1, &[(FILE_DATA_BLOCK as u64, 0, data)], false);

    let m = mount(img);
    assert_eq!(read_file(&m.sb, "hello"), b"NEW!");

    let img = unmount(m);
    // 日志头复位:s_start = 0,序列号推进到 2。
    let sb_off = 1024usize;
    let (jsb_off, _) = journal_sb_range();
    assert_eq!(rdb32(&img[jsb_off..], 0x1c), 0, "s_start 必须清零");
    // j_transaction_sequence = end_transaction + 1(使残留旧事务记录失效)。
    assert_eq!(
        rdb32(&img[jsb_off..], 0x18),
        3,
        "s_sequence 必须推进到 end+1"
    );
    // INCOMPAT_RECOVER 已清除。
    assert_eq!(rd32(&img[sb_off..sb_off + 1024], 0x60) & 0x0004, 0);
    // 数据块内容是 "NEW!"。
    let blk = &img[FILE_DATA_BLOCK as usize * BS..(FILE_DATA_BLOCK + 1) as usize * BS];
    assert_eq!(&blk[..4], b"NEW!");
    // kill_sb 之后 VALID_FS 置位(干净卸载)。
    assert_ne!(
        u16::from_le_bytes([img[sb_off + 0x3a], img[sb_off + 0x3b]]) & 1,
        0
    );
}

/// revoke 记录会取消同事务内的 tag 回放。
#[ktest]
fn recover_skips_revoked_block() {
    let mut img = ExtImg::new();
    add_file(&mut img, 12, b"hello", FILE_DATA_BLOCK, b"OLD!");
    let data = {
        let mut d = vec![0u8; BS];
        d[..4].copy_from_slice(b"REV!");
        d
    };
    let descr = img.journal_descriptor(1, &[(FILE_DATA_BLOCK as u64, 0, data.clone())]);
    let revoke = img.journal_revoke(1, &[FILE_DATA_BLOCK as u64]);
    let commit = img.journal_commit(1, 1_700_000_000, false);
    img.inject_journal(&[(1, descr), (2, data), (3, revoke), (4, commit)]);
    img.set_jsb_start(1, 1);
    img.set_recover(true);

    let m = mount(img);
    assert_eq!(read_file(&m.sb, "hello"), b"OLD!", "revoked 块不得回放");
    let img = unmount(m);
    let blk = &img[FILE_DATA_BLOCK as usize * BS..(FILE_DATA_BLOCK + 1) as usize * BS];
    assert_eq!(&blk[..4], b"OLD!");
}

/// 坏校验和的提交(撕裂提交)截断后续事务;之前的完整事务仍回放。
#[ktest]
fn recover_stops_at_torn_commit() {
    let mut img = ExtImg::new();
    add_file(&mut img, 12, b"hello", FILE_DATA_BLOCK, b"OLD!");
    let d1 = {
        let mut d = vec![0u8; BS];
        d[..4].copy_from_slice(b"OK1!");
        d
    };
    let d2 = {
        let mut d = vec![0u8; BS];
        d[..4].copy_from_slice(b"BAD!");
        d
    };
    // 事务 1 完整;事务 2 的 commit 校验和故意写错。
    let descr1 = img.journal_descriptor(1, &[(FILE_DATA_BLOCK as u64, 0, d1.clone())]);
    let commit1 = img.journal_commit(1, 1_700_000_000, false);
    let descr2 = img.journal_descriptor(2, &[(FILE_DATA_BLOCK as u64, 0, d2.clone())]);
    let commit2 = img.journal_commit(2, 1_700_000_100, true);
    img.inject_journal(&[
        (1, descr1),
        (2, d1),
        (3, commit1),
        (4, descr2),
        (5, d2),
        (6, commit2),
    ]);
    img.set_jsb_start(1, 1);
    img.set_recover(true);

    let m = mount(img);
    assert_eq!(
        read_file(&m.sb, "hello"),
        b"OK1!",
        "撕裂提交之后的事务不得回放"
    );
    let _ = unmount(m);
}

/// ESCAPE 标志:回放后把日志中被转义清零的块首 4 字节恢复为 JBD2 magic。
#[ktest]
fn recover_escape_restores_magic() {
    let mut img = ExtImg::new();
    add_file(&mut img, 12, b"hello", FILE_DATA_BLOCK, b"OLD!");
    // 目标块真实内容以 JBD2 magic 开头;日志里存放转义(前 4 字节清零)版本。
    let mut escaped = vec![0u8; BS];
    escaped[4..8].copy_from_slice(b"REST");
    let descr = img.journal_descriptor(
        1,
        &[(FILE_DATA_BLOCK as u64, JBD2_FLAG_ESCAPE, escaped.clone())],
    );
    let commit = img.journal_commit(1, 1_700_000_000, false);
    img.inject_journal(&[(1, descr), (2, escaped), (3, commit)]);
    img.set_jsb_start(1, 1);
    img.set_recover(true);

    let m = mount(img);
    let _ = read_file(&m.sb, "hello");
    let img = unmount(m);
    let blk = &img[FILE_DATA_BLOCK as usize * BS..(FILE_DATA_BLOCK + 1) as usize * BS];
    assert_eq!(
        &blk[..4],
        &JBD2_MAGIC.to_be_bytes(),
        "ESCAPE 必须恢复 magic"
    );
    assert_eq!(&blk[4..8], b"REST");
}

/// 日志区回绕:事务描述符/数据/提交块跨日志末尾也能正确回放。
#[ktest]
fn recover_wraps_around_log_end() {
    let mut img = ExtImg::new();
    add_file(&mut img, 12, b"hello", FILE_DATA_BLOCK, b"OLD!");
    let data = {
        let mut d = vec![0u8; BS];
        d[..4].copy_from_slice(b"WRAP");
        d
    };
    // 普通日志区是 [1, 32);事务放在 30(descr), 31(data), 1(commit)。
    let descr = img.journal_descriptor(1, &[(FILE_DATA_BLOCK as u64, 0, data.clone())]);
    let commit = img.journal_commit(1, 1_700_000_000, false);
    img.inject_journal(&[(30, descr), (31, data), (1, commit)]);
    img.set_jsb_start(1, 30);
    img.set_recover(true);

    let m = mount(img);
    assert_eq!(read_file(&m.sb, "hello"), b"WRAP");
    let _ = unmount(m);
}

/// 挂载记账:rw 挂载清 VALID_FS,kill_sb 恢复;mnt_count 递增。
#[ktest]
fn mount_bookkeeping_state_and_count() {
    let img = ExtImg::new();
    let m = mount(img);
    let during = m.disk.dump();
    // rw 挂载:VALID_FS 被清除;s_mnt_count 递增为 1。
    assert_eq!(
        u16::from_le_bytes([during[1024 + 0x3a], during[1024 + 0x3b]]) & 1,
        0,
        "rw 挂载必须清 VALID_FS"
    );
    assert_eq!(
        u16::from_le_bytes([during[1024 + 0x34], during[1024 + 0x35]]),
        1
    );
    let img = unmount(m);
    assert_ne!(
        u16::from_le_bytes([img[1024 + 0x3a], img[1024 + 0x3b]]) & 1,
        0,
        "干净卸载必须恢复 VALID_FS"
    );
}

/// fast commit 回放:HEAD + INODE + CREAT + TAIL 创建一个新文件。
#[ktest]
fn fc_replay_creates_file() {
    let mut img = ExtImg::new();
    // 开启 FAST_COMMIT compat + journal FAST_COMMIT 特性,fc 区域 4 块。
    img.add_compat(COMPAT_FAST_COMMIT);
    img.add_incompat(INCOMPAT_RECOVER);
    img.write_jsb(
        1,
        1,
        JBD2_FEATURE_INCOMPAT_REVOKE
            | JBD2_FEATURE_INCOMPAT_CSUM_V3
            | JBD2_FEATURE_INCOMPAT_FAST_COMMIT,
    );
    img.set_jsb_num_fc(4);
    img.set_recover(true);

    // 新文件 inode(extent 空根)。
    let ib = extent_root(&[]);
    let raw = ExtImg::make_inode(S_IFREG | 0o644, 1, 0, &ib, EXT4_EXTENTS_FL);
    let fc_blk = img.fc_block(
        1,
        &[
            ExtImg::fc_inode(12, &raw),
            ExtImg::fc_dentry(3, 2, 12, b"fcfile"), // CREAT
        ],
    );
    // fc 区域 = 日志逻辑块 [32-4+1=29, 32]。写在块 29。
    img.inject_journal(&[(29, fc_blk)]);

    let m = mount(img);
    let inode =
        m.sb.root_inode
            .lookup("fcfile")
            .expect("fc 回放应创建 fcfile");
    assert_eq!(inode.kind(), FileType::Regular);
    let img = unmount(m);
    assert!(img_inode_used(&img, 12), "fc 回放必须置位 inode 位图");
}

fn img_inode_used(img: &[u8], ino: u32) -> bool {
    let bm = &img[INODE_BITMAP as usize * BS..(INODE_BITMAP + 1) as usize * BS];
    bm[((ino - 1) / 8) as usize] & (1u8 << ((ino - 1) % 8)) != 0
}

fn img_block_used(img: &[u8], blk: u32) -> bool {
    let bm = &img[BLOCK_BITMAP as usize * BS..(BLOCK_BITMAP + 1) as usize * BS];
    bm[(blk / 8) as usize] & (1u8 << (blk % 8)) != 0
}

/// orphan 链表:nlink=0 的孤儿在挂载时被回收(inode + 数据块)。
#[ktest]
fn orphan_list_inode_deleted_on_mount() {
    let mut img = ExtImg::new();
    // 孤儿:nlink=0,有一个数据块,但不在任何目录里。
    let ib = extent_root(&[(0, 1, FILE_DATA_BLOCK as u64)]);
    let mut raw = ExtImg::make_inode(S_IFREG | 0o644, 0, 4, &ib, EXT4_EXTENTS_FL);
    le32(&mut raw, 28, 8);
    le32(&mut raw, 20, 0); // i_dtime = 链表尾
    img.write_inode(12, &raw);
    img.set_inode_used(12, true);
    img.set_block_used(FILE_DATA_BLOCK, true);
    img.set_last_orphan(12);

    let m = mount(img);
    let img = unmount(m);
    assert!(!img_inode_used(&img, 12), "孤儿 inode 必须被回收");
    assert!(
        !img_block_used(&img, FILE_DATA_BLOCK),
        "孤儿数据块必须被释放"
    );
    assert_eq!(rd32(&img[1024..2048], 0xe8), 0, "s_last_orphan 必须清零");
}

/// orphan 链表:nlink>0 的孤儿按 i_size 完成截断。
#[ktest]
fn orphan_list_truncate_completed_on_mount() {
    let mut img = ExtImg::new();
    // 孤儿:nlink=1,size=100,映射了 lb0→49, lb1→50(应被截掉)。
    let ib = extent_root(&[(0, 2, 49u64)]);
    let mut raw = ExtImg::make_inode(S_IFREG | 0o644, 1, 100, &ib, EXT4_EXTENTS_FL);
    le32(&mut raw, 28, 16);
    le32(&mut raw, 20, 0);
    img.write_inode(13, &raw);
    img.set_inode_used(13, true);
    img.set_block_used(49, true);
    img.set_block_used(50, true);
    img.set_last_orphan(13);

    let m = mount(img);
    let img = unmount(m);
    assert!(img_inode_used(&img, 13), "截断中的 inode 必须保留");
    assert!(img_block_used(&img, 49), "i_size 内的块必须保留");
    assert!(!img_block_used(&img, 50), "i_size 之后的块必须被释放");
    assert_eq!(rd32(&img[1024..2048], 0xe8), 0);
}

/// orphan file(新式):条目被处理,ORPHAN_PRESENT 清除。
#[ktest]
fn orphan_file_processed_on_mount() {
    let mut img = ExtImg::new();
    // orphan file inode(ino 14):一个数据块,条目 [12, 0, ...]。
    let of_block: u32 = 60;
    let ib = extent_root(&[(0, 1, of_block as u64)]);
    let mut raw = ExtImg::make_inode(S_IFREG, 1, BS as u64, &ib, EXT4_EXTENTS_FL);
    le32(&mut raw, 28, 8);
    img.write_inode(14, &raw);
    img.set_inode_used(14, true);
    img.set_block_used(of_block, true);
    // orphan 块:条目 + tail(magic + csum)。
    {
        let csum_seed = img.csum_seed;
        let mut blk = vec![0u8; BS];
        le32(&mut blk, 0, 12); // 孤儿 inode 号
        let tail = BS - 8;
        le32(&mut blk, tail, 0x0b10_ca04);
        let mut seed = csum_seed;
        seed = crc_update(seed, &14u32.to_le_bytes());
        seed = crc_update(seed, &0u32.to_le_bytes()); // generation
        let mut csum = crc_update(seed, &(of_block as u64).to_le_bytes());
        csum = crc_update(csum, &blk[..BS - 8]);
        le32(&mut blk, tail + 4, csum);
        img.block_mut(of_block).copy_from_slice(&blk);
    }
    // 孤儿:nlink=0,有数据块。
    let ib = extent_root(&[(0, 1, FILE_DATA_BLOCK as u64)]);
    let mut raw = ExtImg::make_inode(S_IFREG | 0o644, 0, 4, &ib, EXT4_EXTENTS_FL);
    le32(&mut raw, 28, 8);
    img.write_inode(12, &raw);
    img.set_inode_used(12, true);
    img.set_block_used(FILE_DATA_BLOCK, true);
    // 打开 orphan_file 特性。
    img.add_compat(COMPAT_ORPHAN_FILE);
    img.add_ro_compat(RO_COMPAT_ORPHAN_PRESENT);
    img.set_orphan_file_inum(14);

    let m = mount(img);
    let img = unmount(m);
    assert!(!img_inode_used(&img, 12));
    assert!(!img_block_used(&img, FILE_DATA_BLOCK));
    assert_eq!(rd32(&img[1024..2048], 0x64) & RO_COMPAT_ORPHAN_PRESENT, 0);
    // orphan file inode 本身必须保留(不破坏 COMPAT_ORPHAN_FILE 语义)。
    assert!(img_inode_used(&img, 14));
}

fn crc_update(seed: u32, data: &[u8]) -> u32 {
    crate::crc::update(seed, data)
}
