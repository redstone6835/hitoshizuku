//! 内核构造的 Native ABI 启动区编码器。

use alloc::vec::Vec;

use crate::registry::{
    ABI_EPOCH, ObjectInterface, PAGE_SIZE, RequirementId, Rights, TargetArch, requirement,
};
use crate::{NativeHandle, wire};

const START_INFO_MAGIC: [u8; 4] = *b"syst";
const START_INFO_VERSION: u16 = 1;
const MAX_START_INFO_SIZE: usize = 1024 * 1024;
const MAX_RUNTIME_ARRAY_ENTRIES: u32 = 4096;
const RUNTIME_ARRAY_ENTRY_SIZE: u16 = 8;
const STATIC_TLS_FEATURE: u64 = 1 << 0;
const INIT_FINI_ARRAY_FEATURE: u64 = 1 << 1;
const RUN_INIT_ARRAY: u64 = 1 << 0;
const RUN_FINI_ARRAY: u64 = 1 << 1;
const KNOWN_RUNTIME_FLAGS: u64 = RUN_INIT_ARRAY | RUN_FINI_ARRAY;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeArrayInfo {
    pub offset: u64,
    pub count: u32,
    pub entry_size: u16,
}

impl RuntimeArrayInfo {
    pub const EMPTY: Self = Self {
        offset: 0,
        count: 0,
        entry_size: 0,
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InitialHandleRecord {
    pub requirement_id: RequirementId,
    pub object_interface: ObjectInterface,
    pub handle: NativeHandle,
    pub granted_rights: Rights,
}

#[derive(Clone, Copy)]
pub struct StartInfoInput<'a> {
    pub target_arch: TargetArch,
    pub enabled_features: u64,
    pub image_base: u64,
    pub initial_tls_base: u64,
    pub initial_tls_size: u64,
    pub initial_thread_pointer: u64,
    pub argv: &'a [Vec<u8>],
    pub env: &'a [Vec<u8>],
    pub initial_handles: &'a [InitialHandleRecord],
    pub call_slot_count: u32,
    pub random_seed: [u8; 32],
    pub runtime_flags: u64,
    pub init_array: RuntimeArrayInfo,
    pub fini_array: RuntimeArrayInfo,
    pub max_size: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartInfoBuildError {
    InvalidInput,
    TooLarge,
    ResourceExhausted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartInfoImage {
    bytes: Vec<u8>,
}

impl StartInfoImage {
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

pub fn build_start_info(input: StartInfoInput<'_>) -> Result<StartInfoImage, StartInfoBuildError> {
    validate_input(&input)?;

    let argv_offset = array_offset(wire::START_INFO_SIZE, input.argv.len())?;
    let argv_end = array_end(argv_offset, input.argv.len(), wire::STRING_REF_SIZE)?;
    let env_offset = array_offset(argv_end, input.env.len())?;
    let env_end = array_end(env_offset, input.env.len(), wire::STRING_REF_SIZE)?;
    let handle_offset = array_offset(env_end, input.initial_handles.len())?;
    let handle_end = array_end(
        handle_offset,
        input.initial_handles.len(),
        wire::INITIAL_HANDLE_SIZE,
    )?;
    let string_offset = align_up(handle_end, 8)?;
    let string_size = input
        .argv
        .iter()
        .chain(input.env)
        .try_fold(0usize, |total, value| {
            total
                .checked_add(value.len())
                .and_then(|total| total.checked_add(1))
                .ok_or(StartInfoBuildError::TooLarge)
        })?;
    let content_end = string_offset
        .checked_add(string_size)
        .ok_or(StartInfoBuildError::TooLarge)?;
    let total_size = align_up(content_end, 8)?;
    if total_size > usize::try_from(input.max_size).unwrap_or(usize::MAX)
        || total_size > MAX_START_INFO_SIZE
        || total_size > u32::MAX as usize
    {
        return Err(StartInfoBuildError::TooLarge);
    }

    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(total_size)
        .map_err(|_| StartInfoBuildError::ResourceExhausted)?;
    bytes.resize(total_size, 0);

    bytes[wire::start_info::MAGIC..wire::start_info::MAGIC + 4].copy_from_slice(&START_INFO_MAGIC);
    put_u16(&mut bytes, wire::start_info::VERSION, START_INFO_VERSION);
    put_u16(
        &mut bytes,
        wire::start_info::HEADER_SIZE,
        wire::START_INFO_SIZE as u16,
    );
    put_u32(&mut bytes, wire::start_info::TOTAL_SIZE, total_size as u32);
    put_u16(&mut bytes, wire::start_info::ABI_EPOCH, ABI_EPOCH);
    put_u16(
        &mut bytes,
        wire::start_info::TARGET_ARCH,
        input.target_arch as u16,
    );
    put_u64(
        &mut bytes,
        wire::start_info::ENABLED_FEATURES,
        input.enabled_features,
    );
    put_u64(&mut bytes, wire::start_info::IMAGE_BASE, input.image_base);
    put_u64(&mut bytes, wire::start_info::PAGE_SIZE, PAGE_SIZE);
    put_u64(
        &mut bytes,
        wire::start_info::INITIAL_TLS_BASE,
        input.initial_tls_base,
    );
    put_u64(
        &mut bytes,
        wire::start_info::INITIAL_TLS_SIZE,
        input.initial_tls_size,
    );
    put_u64(
        &mut bytes,
        wire::start_info::INITIAL_THREAD_POINTER,
        input.initial_thread_pointer,
    );
    put_u32(&mut bytes, wire::start_info::ARGC, input.argv.len() as u32);
    put_u32(&mut bytes, wire::start_info::ENVC, input.env.len() as u32);
    put_u32(
        &mut bytes,
        wire::start_info::ARGV_OFFSET,
        nonempty_offset(input.argv.len(), argv_offset),
    );
    put_u32(
        &mut bytes,
        wire::start_info::ENV_OFFSET,
        nonempty_offset(input.env.len(), env_offset),
    );
    put_u32(
        &mut bytes,
        wire::start_info::STRING_BYTES_OFFSET,
        nonempty_offset(string_size, string_offset),
    );
    put_u32(
        &mut bytes,
        wire::start_info::STRING_BYTES_SIZE,
        string_size as u32,
    );
    put_u32(
        &mut bytes,
        wire::start_info::INITIAL_HANDLE_COUNT,
        input.initial_handles.len() as u32,
    );
    put_u16(
        &mut bytes,
        wire::start_info::INITIAL_HANDLE_RECORD_SIZE,
        wire::INITIAL_HANDLE_SIZE as u16,
    );
    put_u32(
        &mut bytes,
        wire::start_info::INITIAL_HANDLE_OFFSET,
        nonempty_offset(input.initial_handles.len(), handle_offset),
    );
    put_u32(
        &mut bytes,
        wire::start_info::CALL_SLOT_COUNT,
        input.call_slot_count,
    );
    bytes[wire::start_info::RANDOM_SEED..wire::start_info::RANDOM_SEED + 32]
        .copy_from_slice(&input.random_seed);
    put_u64(
        &mut bytes,
        wire::start_info::RUNTIME_FLAGS,
        input.runtime_flags,
    );
    put_u64(
        &mut bytes,
        wire::start_info::INIT_ARRAY_OFFSET,
        input.init_array.offset,
    );
    put_u32(
        &mut bytes,
        wire::start_info::INIT_ARRAY_COUNT,
        input.init_array.count,
    );
    put_u16(
        &mut bytes,
        wire::start_info::INIT_ARRAY_ENTRY_SIZE,
        input.init_array.entry_size,
    );
    put_u64(
        &mut bytes,
        wire::start_info::FINI_ARRAY_OFFSET,
        input.fini_array.offset,
    );
    put_u32(
        &mut bytes,
        wire::start_info::FINI_ARRAY_COUNT,
        input.fini_array.count,
    );
    put_u16(
        &mut bytes,
        wire::start_info::FINI_ARRAY_ENTRY_SIZE,
        input.fini_array.entry_size,
    );

    let mut string_cursor = string_offset;
    encode_strings(&mut bytes, argv_offset, input.argv, &mut string_cursor);
    encode_strings(&mut bytes, env_offset, input.env, &mut string_cursor);
    for (index, handle) in input.initial_handles.iter().enumerate() {
        let offset = handle_offset + index * wire::INITIAL_HANDLE_SIZE;
        put_u32(
            &mut bytes,
            offset + wire::initial_handle::REQUIREMENT_ID,
            handle.requirement_id as u32,
        );
        put_u16(
            &mut bytes,
            offset + wire::initial_handle::OBJECT_INTERFACE,
            handle.object_interface as u16,
        );
        put_u64(
            &mut bytes,
            offset + wire::initial_handle::HANDLE,
            handle.handle.raw(),
        );
        put_u64(
            &mut bytes,
            offset + wire::initial_handle::GRANTED_RIGHTS,
            handle.granted_rights.bits(),
        );
    }

    Ok(StartInfoImage { bytes })
}

fn validate_input(input: &StartInfoInput<'_>) -> Result<(), StartInfoBuildError> {
    if input.image_base == 0
        || input.image_base % PAGE_SIZE != 0
        || input.max_size < wire::START_INFO_SIZE as u32
        || input.random_seed.iter().all(|byte| *byte == 0)
        || input.argv.len() > u32::MAX as usize
        || input.env.len() > u32::MAX as usize
        || input.initial_handles.len() > u32::MAX as usize
        || input.runtime_flags & !KNOWN_RUNTIME_FLAGS != 0
    {
        return Err(StartInfoBuildError::InvalidInput);
    }
    let no_tls = input.initial_tls_base == 0
        && input.initial_tls_size == 0
        && input.initial_thread_pointer == 0;
    let valid_tls = input.initial_tls_base != 0
        && input.initial_tls_size != 0
        && input.initial_thread_pointer == input.initial_tls_base;
    if (!no_tls && !valid_tls) || valid_tls != (input.enabled_features & STATIC_TLS_FEATURE != 0) {
        return Err(StartInfoBuildError::InvalidInput);
    }
    validate_runtime_array(input.init_array, input.runtime_flags, RUN_INIT_ARRAY)?;
    validate_runtime_array(input.fini_array, input.runtime_flags, RUN_FINI_ARRAY)?;
    let has_runtime_array = input.init_array.count != 0 || input.fini_array.count != 0;
    if has_runtime_array != (input.enabled_features & INIT_FINI_ARRAY_FEATURE != 0) {
        return Err(StartInfoBuildError::InvalidInput);
    }
    if input
        .argv
        .iter()
        .chain(input.env)
        .any(|value| value.contains(&0))
    {
        return Err(StartInfoBuildError::InvalidInput);
    }

    let mut previous = 0u32;
    for handle in input.initial_handles {
        let id = handle.requirement_id as u32;
        let spec = requirement(handle.requirement_id).ok_or(StartInfoBuildError::InvalidInput)?;
        if id <= previous
            || handle.object_interface != spec.interface
            || !handle.granted_rights.is_subset_of(spec.max_rights)
            || handle.handle.index == 0
            || handle.handle.generation == 0
        {
            return Err(StartInfoBuildError::InvalidInput);
        }
        previous = id;
    }
    Ok(())
}

fn validate_runtime_array(
    array: RuntimeArrayInfo,
    runtime_flags: u64,
    run_flag: u64,
) -> Result<(), StartInfoBuildError> {
    if array.count == 0 {
        if array != RuntimeArrayInfo::EMPTY || runtime_flags & run_flag != 0 {
            return Err(StartInfoBuildError::InvalidInput);
        }
        return Ok(());
    }
    if array.offset == 0
        || array.offset % RUNTIME_ARRAY_ENTRY_SIZE as u64 != 0
        || array.count > MAX_RUNTIME_ARRAY_ENTRIES
        || array.entry_size != RUNTIME_ARRAY_ENTRY_SIZE
        || runtime_flags & run_flag == 0
    {
        return Err(StartInfoBuildError::InvalidInput);
    }
    Ok(())
}

fn encode_strings(bytes: &mut [u8], array_offset: usize, values: &[Vec<u8>], cursor: &mut usize) {
    for (index, value) in values.iter().enumerate() {
        let record = array_offset + index * wire::STRING_REF_SIZE;
        put_u32(bytes, record + wire::string_ref::OFFSET, *cursor as u32);
        put_u32(bytes, record + wire::string_ref::LENGTH, value.len() as u32);
        bytes[*cursor..*cursor + value.len()].copy_from_slice(value);
        *cursor += value.len() + 1;
    }
}

fn array_offset(cursor: usize, count: usize) -> Result<usize, StartInfoBuildError> {
    if count == 0 {
        Ok(cursor)
    } else {
        align_up(cursor, 8)
    }
}

fn array_end(
    offset: usize,
    count: usize,
    record_size: usize,
) -> Result<usize, StartInfoBuildError> {
    count
        .checked_mul(record_size)
        .and_then(|size| offset.checked_add(size))
        .ok_or(StartInfoBuildError::TooLarge)
}

fn align_up(value: usize, alignment: usize) -> Result<usize, StartInfoBuildError> {
    value
        .checked_add(alignment - 1)
        .map(|value| value & !(alignment - 1))
        .ok_or(StartInfoBuildError::TooLarge)
}

fn nonempty_offset(count: usize, offset: usize) -> u32 {
    if count == 0 { 0 } else { offset as u32 }
}

fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}
