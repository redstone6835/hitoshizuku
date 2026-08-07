use alloc::vec;
use alloc::vec::Vec;

use errno::Errno;
use ktest::ktest;
use native_abi::{NativeHandle, ObjectInterface, OperationId, Rights, TargetArch, wire};
use soyo::{
    SliceSoyoReader, SoyoReadAt, SoyoReadLimits, read_soyo,
    registry::{SOYO_MAGIC, SegmentPermissions},
};

use super::test_image::{
    loader_image, loader_init_array_image, loader_small_start_info_image, process_exit_image,
    process_exit_payload,
};
use super::{current_target_arch, load_soyo_image_from_reader, prepare_soyo_runtime};
use crate::exec::prepare_native_initial_frame;
use crate::native_runtime::KernelNativeObject;
use crate::user::{ExecutableFormat, detect_executable_format};

struct MemoryReader<'a> {
    bytes: &'a [u8],
}

impl SoyoReadAt for MemoryReader<'_> {
    type Error = Errno;

    fn len(&self) -> u64 {
        self.bytes.len() as u64
    }

    fn read_exact_at(&self, offset: u64, output: &mut [u8]) -> Result<(), Self::Error> {
        let start = usize::try_from(offset).map_err(|_| Errno::EIO)?;
        let end = start.checked_add(output.len()).ok_or(Errno::EIO)?;
        let input = self.bytes.get(start..end).ok_or(Errno::EIO)?;
        output.copy_from_slice(input);
        Ok(())
    }
}

struct FailingReader;

impl SoyoReadAt for FailingReader {
    type Error = Errno;

    fn len(&self) -> u64 {
        8192
    }

    fn read_exact_at(&self, _offset: u64, _output: &mut [u8]) -> Result<(), Self::Error> {
        Err(Errno::EIO)
    }
}

#[ktest]
fn executable_probe_distinguishes_soyo_elf_script_and_unknown() {
    assert_eq!(detect_executable_format(b"soyo"), ExecutableFormat::Soyo);
    assert_eq!(detect_executable_format(b"\x7fELF"), ExecutableFormat::Elf);
    assert_eq!(
        detect_executable_format(b"#!/bin/sh"),
        ExecutableFormat::Script
    );
    assert_eq!(detect_executable_format(b"text"), ExecutableFormat::Unknown);
}

#[ktest]
fn soyo_reader_preserves_source_errno() {
    assert_eq!(
        load_soyo_image_from_reader(&FailingReader).err(),
        Some(Errno::EIO)
    );
}

#[ktest]
fn direct_fixture_is_canonical_for_both_architectures() {
    for target in [TargetArch::Riscv64, TargetArch::LoongArch64] {
        let bytes = process_exit_image(target);
        assert_eq!(&bytes[..4], &SOYO_MAGIC);
        assert!(!bytes.windows(4).any(|window| window == b"\x7fELF"));
        assert_eq!(
            &bytes[4096..4096 + process_exit_payload(target).len()],
            process_exit_payload(target)
        );

        let metadata = read_soyo(&SliceSoyoReader::new(&bytes), SoyoReadLimits::portable())
            .expect("直接 fixture 必须通过共享 parser");
        assert_eq!(metadata.header.target_arch, target);
        assert!(metadata.runtime.start_info_max_size >= 4096);
    }
}

#[ktest]
fn loader_rejects_the_other_architecture() {
    let other = match current_target_arch() {
        TargetArch::Riscv64 => TargetArch::LoongArch64,
        TargetArch::LoongArch64 => TargetArch::Riscv64,
    };
    let bytes = process_exit_image(other);
    let reader = MemoryReader { bytes: &bytes };

    assert_eq!(
        load_soyo_image_from_reader(&reader).err(),
        Some(Errno::ENOEXEC)
    );
}

#[ktest]
fn loader_builds_private_native_runtime_with_exact_permissions() {
    let bytes = process_exit_image(current_target_arch());
    let reader = MemoryReader { bytes: &bytes };
    let loaded = load_soyo_image_from_reader(&reader).expect("当前架构 fixture 应完成映像装载");
    let prepared = prepare_soyo_runtime(
        loaded,
        &[b"native".to_vec(), vec![0xff, b'x']],
        &[b"KEY=VALUE".to_vec()],
        None,
    )
    .expect("fixture 应完成 Native 运行时准备");

    let vmas = general::mm::vm_space::dump_vmas(&prepared.vm);
    let code = vmas
        .iter()
        .find(|(range, _)| range.contains(&prepared.entry_pc))
        .expect("入口必须位于映射中");
    let expected_code = SegmentPermissions::READ.bits() | SegmentPermissions::EXECUTE.bits();
    assert_eq!(code.1.bits() & 0b111, u32::from(expected_code));

    let start_info_vma = vmas
        .iter()
        .find(|(range, _)| range.contains(&prepared.start_info_address))
        .expect("StartInfo 必须位于映射中");
    assert!(start_info_vma.1.has(mm::VmFlags::READ));
    assert!(!start_info_vma.1.has(mm::VmFlags::WRITE));
    assert!(!start_info_vma.1.has(mm::VmFlags::EXEC));

    let stack = vmas
        .iter()
        .find(|(range, _)| range.contains(&(prepared.user_sp - 1)))
        .expect("初始 SP 必须位于栈顶");
    assert!(stack.1.has(mm::VmFlags::READ));
    assert!(stack.1.has(mm::VmFlags::WRITE));
    assert!(!stack.1.has(mm::VmFlags::EXEC));
    assert!(
        !vmas
            .iter()
            .any(|(range, _)| range.contains(&(stack.0.start - 1)))
    );

    let start_info = read_vm_bytes(
        &prepared.vm,
        prepared.start_info_address,
        prepared.start_info_size,
    );
    assert_eq!(
        &start_info[wire::start_info::MAGIC..wire::start_info::MAGIC + 4],
        b"syst"
    );
    assert_eq!(
        u32_at(&start_info, wire::start_info::TOTAL_SIZE),
        prepared.start_info_size as u32
    );
    assert_eq!(u32_at(&start_info, wire::start_info::ARGC), 2);
    assert!(start_info.windows(2).any(|window| window == [0xff, b'x']));
    assert_eq!(
        u32_at(&start_info, wire::start_info::INITIAL_HANDLE_COUNT),
        1
    );
    assert_eq!(prepared.tls_base, 0);
    assert_eq!(prepared.personality.binding.call_slots.len(), 1);
    assert_eq!(
        prepared.personality.binding.call_slots[0].operation,
        Some(OperationId::ProcessExit)
    );

    let handles = prepared.personality.handles.lock();
    let handle = handles
        .lookup(
            NativeHandle::from_parts(1, 1),
            Some(ObjectInterface::Process),
            Rights::TERMINATE_SELF,
        )
        .expect("SELF_PROCESS 初始 handle 必须存在");
    assert!(matches!(handle.object, KernelNativeObject::SelfProcess));
}

#[ktest]
fn loader_applies_relocation_and_preserves_data_bss_tls_zero_fill() {
    let bytes = loader_image(current_target_arch());
    let loaded = load_soyo_image_from_reader(&MemoryReader { bytes: &bytes })
        .expect("完整 fixture 应完成段装载");
    let data_address = loaded.image_base + soyo::test_support::LOADER_FIXTURE_DATA_OFFSET as usize;
    let data = read_vm_bytes(&loaded.vm, data_address, 32);
    assert_eq!(
        u64::from_le_bytes(data[..8].try_into().unwrap()),
        loaded.image_base as u64
    );
    assert_eq!(&data[8..16], &soyo::test_support::LOADER_FIXTURE_DATA[8..]);
    assert!(data[16..].iter().all(|byte| *byte == 0));

    let bss_address = loaded.image_base + soyo::test_support::LOADER_FIXTURE_BSS_OFFSET as usize;
    assert!(
        read_vm_bytes(&loaded.vm, bss_address, 32)
            .iter()
            .all(|byte| *byte == 0)
    );
    let rodata_address =
        loaded.image_base + soyo::test_support::LOADER_FIXTURE_RODATA_OFFSET as usize;
    assert_eq!(
        &read_vm_bytes(
            &loaded.vm,
            rodata_address,
            soyo::test_support::LOADER_FIXTURE_RODATA.len()
        ),
        &soyo::test_support::LOADER_FIXTURE_RODATA
    );

    let vmas = general::mm::vm_space::dump_vmas(&loaded.vm);
    let permissions = [
        (loaded.image_base, 0b101u32),
        (rodata_address, 0b001u32),
        (data_address, 0b011u32),
        (bss_address, 0b011u32),
    ];
    for (address, expected) in permissions {
        let (_, flags) = vmas
            .iter()
            .find(|(range, _)| range.contains(&address))
            .expect("每个普通段都必须存在映射");
        assert_eq!(flags.bits() & 0b111, expected);
        assert!(!(flags.has(mm::VmFlags::WRITE) && flags.has(mm::VmFlags::EXEC)));
    }

    let prepared = prepare_soyo_runtime(loaded, &[b"arg".to_vec()], &[], None)
        .expect("完整 fixture 应完成运行时准备");
    let tls = read_vm_bytes(
        &prepared.vm,
        prepared.tls_base,
        soyo::test_support::LOADER_FIXTURE_TLS_SIZE,
    );
    assert_eq!(
        &tls[..soyo::test_support::LOADER_FIXTURE_TLS.len()],
        &soyo::test_support::LOADER_FIXTURE_TLS
    );
    assert!(
        tls[soyo::test_support::LOADER_FIXTURE_TLS.len()..]
            .iter()
            .all(|byte| *byte == 0)
    );
    let start_info = read_vm_bytes(
        &prepared.vm,
        prepared.start_info_address,
        prepared.start_info_size,
    );
    assert_eq!(
        u64_at(&start_info, wire::start_info::INITIAL_TLS_BASE),
        prepared.tls_base as u64
    );
    assert_eq!(
        u64_at(&start_info, wire::start_info::INITIAL_TLS_SIZE),
        soyo::test_support::LOADER_FIXTURE_TLS_SIZE as u64
    );
}

#[ktest]
fn loader_rejects_nonempty_init_array_for_native_exec() {
    let bytes = loader_init_array_image(current_target_arch());
    assert_eq!(
        load_soyo_image_from_reader(&MemoryReader { bytes: &bytes }).err(),
        Some(Errno::ENOEXEC)
    );
}

#[ktest]
fn loader_maps_start_info_size_overflow_to_e2big() {
    let bytes = loader_small_start_info_image(current_target_arch());
    let loaded = load_soyo_image_from_reader(&MemoryReader { bytes: &bytes })
        .expect("StartInfo 边界 fixture 的映像部分应合法");
    let result = prepare_soyo_runtime(loaded, &[vec![b'a'; 64]], &[], None);
    assert_eq!(result.err(), Some(Errno::E2BIG));
}

#[ktest]
fn native_initial_frame_contains_start_info_contract() {
    let frame =
        prepare_native_initial_frame(0x4000, 0x8000, 0x7000, 256, 0x1000, 0x6000, 0xdead_0000);
    assert_eq!(frame.pc(), 0x4000);
    assert_eq!(frame.sp(), 0x8000);

    #[cfg(target_arch = "riscv64")]
    {
        let mut raw = arch::riscv64::trap_frame::TrapFrame::default();
        frame.apply_to_context(&mut raw as *mut _ as usize);
        assert_eq!(raw.a0, 0x7000);
        assert_eq!(raw.a1, 256);
        assert_eq!(raw.a2, 0x1000);
        assert_eq!(raw.tp, 0x6000);
        assert_eq!(raw.ra, 0);
        assert_eq!(raw.kstack_top, 0xdead_0000);
    }

    #[cfg(target_arch = "loongarch64")]
    {
        let mut raw = arch::loongarch64::TrapFrame::default();
        frame.apply_to_context(&mut raw as *mut _ as usize);
        assert_eq!(raw.a0, 0x7000);
        assert_eq!(raw.a1, 256);
        assert_eq!(raw.a2, 0x1000);
        assert_eq!(raw.tp, 0x6000);
        assert_eq!(raw.ra, 0);
    }
}

fn read_vm_bytes(vm: &general::mm::VmSpace, address: usize, length: usize) -> Vec<u8> {
    let mut output = Vec::new();
    output.reserve_exact(length);
    let mut cursor = address;
    while output.len() < length {
        let remaining = length - output.len();
        let copied = unsafe {
            vm.with_user_read_slice(cursor, remaining, |input| {
                output.extend_from_slice(input);
                input.len()
            })
        }
        .expect("已驻留只读映射必须可读取");
        assert_ne!(copied, 0);
        cursor += copied;
    }
    output
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn u64_at(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}
