//! 特性级行为的镜像测试:HTree 二级索引目录、MMP、强制只读语义位、
//! encrypt/verity/casefold 以及未知特性位的门禁。

extern crate std;

use std::sync::Arc;
use std::vec;
use std::vec::Vec;

use ktest::ktest;
use ktest_mock::MemDisk;
use vfs::cred::Credentials;
use vfs::error::VfsError;
use vfs::file::OpenOptions;
use vfs::mount::MountFlags;
use vfs::superblock::{FsDriver, Superblock as VfsSuperblock};

use super::extimg::*;
use crate::state::{BlockBackend, ExtFsDriver};

const INCOMPAT_MMP: u32 = 0x0100;
const INCOMPAT_ENCRYPT: u32 = 0x10000;
const INCOMPAT_CASEFOLD: u32 = 0x20000;
const RO_COMPAT_BIGALLOC: u32 = 0x0200;
const RO_COMPAT_VERITY: u32 = 0x8000;

struct Mounted {
    driver: ExtFsDriver,
    sb: Arc<VfsSuperblock>,
    #[allow(dead_code)]
    disk: Arc<MemDisk>,
}

fn mount(img: ExtImg) -> Mounted {
    let disk = Arc::new(MemDisk::from_bytes(img.data, 512));
    let driver = ExtFsDriver::new();
    let backend: Arc<dyn BlockBackend> = disk.clone();
    driver.bind_backend(backend);
    let sb = driver.mount(None, "").expect("挂载测试镜像");
    Mounted { driver, sb, disk }
}

fn mount_res(img: ExtImg) -> Result<Mounted, VfsError> {
    let disk = Arc::new(MemDisk::from_bytes(img.data, 512));
    let driver = ExtFsDriver::new();
    let backend: Arc<dyn BlockBackend> = disk.clone();
    driver.bind_backend(backend);
    driver
        .mount(None, "")
        .map(|sb| Mounted { driver, sb, disk })
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

fn add_file(img: &mut ExtImg, ino: u32, name: &[u8], data_block: u32, content: &[u8], flags: u32) {
    let ib = extent_root(&[(0, 1, data_block as u64)]);
    let mut raw = ExtImg::make_inode(
        S_IFREG | 0o644,
        1,
        content.len() as u64,
        &ib,
        EXT4_EXTENTS_FL | flags,
    );
    le32(&mut raw, 28, 8);
    img.write_inode(ino, &raw);
    img.set_inode_used(ino, true);
    img.set_block_used(data_block, true);
    let blk = img.block_mut(data_block);
    blk[..content.len()].copy_from_slice(content);
    img.add_root_entry(ino, 1, name);
}

/// 二级索引 HTree 目录:线性扫描必须跳过 dx_node 块,不产生伪条目,
/// 也不能把 dx 元数据当成目录项。
#[ktest]
fn htree_two_level_dir_scans_only_leaves() {
    let mut img = ExtImg::new();
    // 根目录:EXT4_INDEX_FL,extent 映射 lb0→12(dx_root), lb1→48, lb2→49, lb3→50(dx_node)。
    let ib = extent_root(&[(0, 4, ROOT_DIR_BLOCK as u64)]);
    // 手动构造 4 条 extent:lb0→12, lb1→48, lb2→49, lb3→50。
    let ib = {
        let _ = ib;
        extent_root(&[
            (0, 1, ROOT_DIR_BLOCK as u64),
            (1, 1, 48u64),
            (2, 1, 49u64),
            (3, 1, 50u64),
        ])
    };
    let mut raw = ExtImg::make_inode(
        S_IFDIR | 0o755,
        4,
        4 * BS as u64,
        &ib,
        EXT4_EXTENTS_FL | EXT4_INDEX_FL,
    );
    le32(&mut raw, 28, 4 * 8);
    img.write_inode(2, &raw);

    // dx_root(块 12):. / .. + dx_root_info(indirect_levels=1, count=1) + entry→lb3。
    {
        let blk = img.block_mut(ROOT_DIR_BLOCK);
        blk.fill(0);
        le32(blk, 0, 2);
        le16(blk, 4, 12);
        blk[6] = 1;
        blk[7] = 2;
        blk[8] = b'.';
        le32(blk, 12, 2);
        le16(blk, 16, (BS - 24) as u16);
        blk[18] = 2;
        blk[19] = 2;
        blk[20] = b'.';
        blk[21] = b'.';
        // dx_root_info @24:indirect_levels=1 → 我们的扫描要跳过其指向的 dx_node。
        blk[28] = 2; // hash_version
        blk[29] = 8; // info_length
        blk[30] = 1; // indirect_levels
        le16(blk, 32, 10); // limit
        le16(blk, 34, 1); // count
        le32(blk, 36, 0x1234_5678); // entry[0].hash
        le32(blk, 40, 3); // entry[0].block = 逻辑块 3(dx_node)
    }
    // dx_node(块 50 = 逻辑块 3):两条 entry 指向叶块 1、2。
    {
        let blk = img.block_mut(50);
        blk.fill(0);
        le32(blk, 0, 0x1111_1111);
        le32(blk, 4, 1);
        le32(blk, 8, 0x2222_2222);
        le32(blk, 12, 2);
    }
    // 叶块 1(块 48):"file-a" → ino 12;叶块 2(块 49):"file-b" → ino 13。
    for (blk_no, name, ino) in [(48u32, &b"file-a"[..], 12u32), (49, &b"file-b"[..], 13)] {
        let blk = img.block_mut(blk_no);
        blk.fill(0);
        le32(blk, 0, ino);
        le16(blk, 4, (BS - 12) as u16);
        blk[6] = name.len() as u8;
        blk[7] = 1;
        blk[8..8 + name.len()].copy_from_slice(name);
        img.write_dir_tail(blk_no, 2, 0);
        // 对应文件 inode。
        let ib = extent_root(&[]);
        let raw = ExtImg::make_inode(S_IFREG | 0o644, 1, 0, &ib, EXT4_EXTENTS_FL);
        img.write_inode(ino, &raw);
        img.set_inode_used(ino, true);
    }
    img.set_block_used(48, true);
    img.set_block_used(49, true);
    img.set_block_used(50, true);

    let m = mount(img);
    // 两个叶块里的条目都必须可见。
    m.sb.root_inode.lookup("file-a").expect("lookup file-a");
    m.sb.root_inode.lookup("file-b").expect("lookup file-b");
    // dx_node 块的内容不得产生伪条目(hash 值当 ino、垃圾名)。
    let f =
        m.sb.root_inode
            .open_ops(&OpenOptions::default(), &Credentials::root())
            .expect("打开根目录");
    let mut names = Vec::new();
    let mut pos = 0u64;
    loop {
        let next = f
            .readdir(pos, &mut |e| {
                names.push(std::string::String::from(e.name.as_str()));
                core::ops::ControlFlow::Continue(())
            })
            .expect("readdir");
        if next == pos {
            break;
        }
        pos = next;
    }
    assert!(
        names.contains(&std::string::String::from("file-a")),
        "{names:?}"
    );
    assert!(
        names.contains(&std::string::String::from("file-b")),
        "{names:?}"
    );
    // 伪条目:dx_node 的前 4 字节 0x11111111 是 hash,不可能成为合法名;
    // readdir 结果只能包含 . .. file-a file-b。
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(
        sorted,
        vec![
            std::string::String::from("."),
            std::string::String::from(".."),
            std::string::String::from("file-a"),
            std::string::String::from("file-b"),
        ],
        "dx_node 索引块不得泄漏伪条目: {sorted:?}"
    );
    let _ = m;
}

/// MMP:序列号为 CLEAN 时允许挂载,否则拒绝。
#[ktest]
fn mmp_check_accepts_clean_and_rejects_busy() {
    let build = |seq: u32| {
        let mut img = ExtImg::new();
        img.add_incompat(INCOMPAT_MMP);
        // s_mmp_block = 60。
        {
            let csum_seed = img.csum_seed;
            let sb = &mut img.data[1024..2048];
            le64(sb, 0x168, 60);
            let sum = crate::crc::crc32c(&sb[..0x3fc]);
            le32(sb, 0x3fc, sum);
            let blk = img.block_mut(60);
            le32(blk, 0, 0x004d_4d50); // MMP magic
            le32(blk, 4, seq);
            // mmp csum:crc32c(csum_seed, mmp[..1020])
            let csum = crate::crc::update(csum_seed, &blk[..1020]);
            le32(blk, 1020, csum);
        }
        img.set_block_used(60, true);
        img
    };

    // CLEAN:允许挂载。
    let m = mount_res(build(0xff4d_4d50));
    assert!(m.is_ok(), "MMP CLEAN 必须允许挂载: {:?}", m.err());
    // FSCK:拒绝。
    let m = mount_res(build(0xe24d_4d50));
    assert!(m.is_err(), "MMP FSCK 序列必须拒绝挂载");
    // 其它序列(疑似挂载中):拒绝。
    let m = mount_res(build(0x0000_0042));
    assert!(m.is_err(), "MMP 非 CLEAN 序列必须拒绝挂载");
}

/// MMP:挂载夺占所有权(写非 CLEAN 序列),心跳推进序列号,干净卸载写回 CLEAN。
#[ktest]
fn mmp_claim_heartbeat_and_clean_unmount() {
    let mut img = ExtImg::new();
    img.add_incompat(INCOMPAT_MMP);
    {
        let csum_seed = img.csum_seed;
        let sb = &mut img.data[1024..2048];
        le64(sb, 0x168, 60); // s_mmp_block = 60
        le16(sb, 0x166, 5); // s_mmp_update_interval = 5s
        let sum = crate::crc::crc32c(&sb[..0x3fc]);
        le32(sb, 0x3fc, sum);
        let blk = img.block_mut(60);
        le32(blk, 0, 0x004d_4d50); // MMP magic
        le32(blk, 4, 0xff4d_4d50); // CLEAN
        // mmp csum:crc32c(csum_seed, mmp[..1020])
        let csum = crate::crc::update(csum_seed, &blk[..1020]);
        le32(blk, 1020, csum);
    }
    img.set_block_used(60, true);

    let m = mount(img);
    let seq_at = |dump: &std::vec::Vec<u8>| rd32(dump, 60 * BS + 4);
    // 挂载后必须夺占所有权(写回首个非 CLEAN 序列号)。
    assert_eq!(seq_at(&m.disk.dump()), 1, "挂载必须写回首个非 CLEAN 序列号");

    // 拿 FsState 引用,手动推进心跳(now=10s > interval=5s)。
    let ops =
        m.sb.ops
            .as_any()
            .downcast_ref::<crate::state::ExtFsSuperblockOps>()
            .expect("downcast ExtFsSuperblockOps");
    let state = &ops.state;
    crate::mmp::heartbeat_at(state, 10_000_000_000, 10);
    assert_eq!(seq_at(&m.disk.dump()), 2, "心跳必须推进序列号");

    // 干净卸载写回 CLEAN。
    crate::mmp::mark_clean(state);
    assert_eq!(
        seq_at(&m.disk.dump()),
        0xff4d_4d50,
        "干净卸载必须写回 CLEAN"
    );
}

/// BIGALLOC 与未知 ro_compat 位:强制只读挂载(写操作返回 EROFS)。
#[ktest]
fn bigalloc_and_unknown_ro_compat_force_read_only() {
    for extra in [RO_COMPAT_BIGALLOC, 0x8000_0000] {
        let mut img = ExtImg::new();
        add_file(&mut img, 12, b"hello", FILE_DATA_BLOCK, b"OLD!", 0);
        img.add_ro_compat(extra);
        let m = mount(img);
        // 读正常。
        assert_eq!(read_file(&m.sb, "hello"), b"OLD!");
        // 写被拒(EROFS)。
        let err =
            m.sb.root_inode
                .create(
                    "newfile",
                    vfs::stat::FileMode::new(0o644),
                    &Credentials::root(),
                )
                .map(|_| ())
                .expect_err("强制只读时 create 必须失败");
        assert_eq!(err, VfsError::ReadOnlyFilesystem, "特性位 {extra:#x}");
        let _ = m;
    }
}

/// 未知 incompat 位:挂载直接拒绝。
#[ktest]
fn unknown_incompat_rejected() {
    let mut img = ExtImg::new();
    img.add_incompat(0x0004_0000); // 未分配位
    let m = mount_res(img);
    assert!(m.is_err(), "未知 incompat 位必须拒绝挂载");
}

/// ENCRYPT:挂载成功;加密文件读写报 Enokey(与 Linux 无密钥一致),lookup 不受影响。
#[ktest]
fn encrypt_inode_io_rejected_but_lookup_works() {
    let mut img = ExtImg::new();
    img.add_incompat(INCOMPAT_ENCRYPT);
    add_file(
        &mut img,
        12,
        b"secret",
        FILE_DATA_BLOCK,
        b"DATA",
        EXT4_ENCRYPT_FL,
    );
    let m = mount(img);
    let inode = m.sb.root_inode.lookup("secret").expect("lookup 加密文件");
    let f = inode
        .open_ops(&OpenOptions::default(), &Credentials::root())
        .expect("打开加密文件");
    let mut buf = vec![0u8; 4];
    let err = f.read_at(&mut buf, 0).expect_err("读加密文件必须失败");
    assert_eq!(err, VfsError::Enokey);
    let err = f.write_at(b"xxxx", 0).expect_err("写加密文件必须失败");
    assert_eq!(err, VfsError::Enokey);
}

/// VERITY:verity 文件读正常,写/截断报 ReadOnlyFilesystem。
#[ktest]
fn verity_inode_write_rejected() {
    let mut img = ExtImg::new();
    img.add_ro_compat(RO_COMPAT_VERITY);
    add_file(
        &mut img,
        12,
        b"vfile",
        FILE_DATA_BLOCK,
        b"DATA",
        EXT4_VERITY_FL,
    );
    let m = mount(img);
    assert_eq!(read_file(&m.sb, "vfile"), b"DATA", "verity 文件读必须正常");
    let inode = m.sb.root_inode.lookup("vfile").expect("lookup");
    let f = inode
        .open_ops(&OpenOptions::default(), &Credentials::root())
        .expect("open");
    let err = f.write_at(b"xxxx", 0).expect_err("写 verity 文件必须失败");
    assert_eq!(err, VfsError::ReadOnlyFilesystem);
    // O_TRUNC 打开路径同样被拦截。
    let trunc_opts = OpenOptions {
        access: vfs::file::AccessMode::WriteOnly,
        truncate: true,
        ..OpenOptions::default()
    };
    let err = inode
        .open_ops(&trunc_opts, &Credentials::root())
        .map(|_| ())
        .expect_err("O_TRUNC 打开 verity 文件必须失败");
    assert_eq!(err, VfsError::ReadOnlyFilesystem);
}

/// CASEFOLD 目录:ASCII 大小写不敏感 lookup。
#[ktest]
fn casefold_dir_lookup_ascii_insensitive() {
    let mut img = ExtImg::new();
    img.add_incompat(INCOMPAT_CASEFOLD);
    add_file(&mut img, 12, b"Hello", FILE_DATA_BLOCK, b"DATA", 0);
    // 根目录加 CASEFOLD 标志。
    {
        let mut raw = {
            // 读出根 inode 修改 flags
            let mut r = [0u8; INODE_SIZE];
            let table_off = INODE_TABLE as usize * BS + (2 - 1) * INODE_SIZE;
            r.copy_from_slice(&img.data[table_off..table_off + INODE_SIZE]);
            r
        };
        let flags = rd32(&raw, 32) | EXT4_CASEFOLD_FL;
        le32(&mut raw, 32, flags);
        img.write_inode(2, &raw);
    }
    let m = mount(img);
    m.sb.root_inode.lookup("hello").expect("小写 lookup");
    m.sb.root_inode.lookup("HELLO").expect("大写 lookup");
    m.sb.root_inode.lookup("HeLLo").expect("混合 lookup");
    assert!(
        m.sb.root_inode.lookup("hellp").is_err(),
        "错误拼写必须 NotFound"
    );
}

/// CASEFOLD 目录:非 ASCII 名按 Unicode 简单小写折叠做大小写不敏感 lookup。
#[ktest]
fn casefold_dir_lookup_unicode_insensitive() {
    let mut img = ExtImg::new();
    img.add_incompat(INCOMPAT_CASEFOLD);
    // 文件名 "Café"(é = U+00E9,UTF-8 2 字节)。
    add_file(&mut img, 12, "Café".as_bytes(), FILE_DATA_BLOCK, b"DATA", 0);
    // 根目录加 CASEFOLD 标志。
    {
        let mut raw = [0u8; INODE_SIZE];
        let table_off = INODE_TABLE as usize * BS + (2 - 1) * INODE_SIZE;
        raw.copy_from_slice(&img.data[table_off..table_off + INODE_SIZE]);
        let flags = rd32(&raw, 32) | EXT4_CASEFOLD_FL;
        le32(&mut raw, 32, flags);
        img.write_inode(2, &raw);
    }
    let m = mount(img);
    // "CAFÉ"(É = U+00C9) 与 "café" 必须命中 "Café";去掉变音符号不得命中。
    m.sb.root_inode.lookup("CAFÉ").expect("大写变音 lookup");
    m.sb.root_inode.lookup("café").expect("小写 lookup");
    assert!(
        m.sb.root_inode.lookup("Cafe").is_err(),
        "去掉变音符号必须 NotFound"
    );
}

/// remount:rw → ro 转 ValidFS,ro → rw 在强制只读文件系统上被拒。
#[ktest]
fn remount_transitions_state_flags() {
    let img = ExtImg::new();
    let m = mount(img);
    // rw → ro:VALID_FS 置位。
    m.sb.ops
        .remount(&m.sb, MountFlags::RDONLY)
        .expect("remount ro");
    let dump = m.disk.dump();
    assert_ne!(
        u16::from_le_bytes([dump[1024 + 0x3a], dump[1024 + 0x3b]]) & 1,
        0,
        "ro remount 必须置 VALID_FS"
    );
    // ro → rw:允许(非强制只读)。
    m.sb.ops
        .remount(&m.sb, MountFlags::default())
        .expect("remount rw");
    let dump = m.disk.dump();
    assert_eq!(
        u16::from_le_bytes([dump[1024 + 0x3a], dump[1024 + 0x3b]]) & 1,
        0,
        "rw remount 必须清 VALID_FS"
    );
}

/// BIGALLOC 文件系统不允许 ro → rw remount。
#[ktest]
fn remount_rw_rejected_on_forced_ro_fs() {
    let mut img = ExtImg::new();
    img.add_ro_compat(RO_COMPAT_BIGALLOC);
    let m = mount(img);
    let err =
        m.sb.ops
            .remount(&m.sb, MountFlags::default())
            .expect_err("BIGALLOC 必须拒绝 rw remount");
    assert_eq!(err, VfsError::ReadOnlyFilesystem);
}
