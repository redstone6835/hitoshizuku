#![no_main]
#![no_std]

use core::panic::PanicInfo;

use anonlib::{
    AddressSpace, Completion, Directory, FileRights, MemoryCreate, MemoryPermissions, Process,
    Submission,
};

const PAGE_SIZE: usize = 4096;
const OBJECT_SIZE: u64 = (PAGE_SIZE * 2) as u64;
const FILE_PATH: &[u8] = b"tmp/soyo-ring-io.bin";
const PAYLOAD: &[u8] = b"SOYO SubmissionRing file and channel payload";
const PASS_MESSAGE: &[u8] = b"SOYO Ring File/Channel PASS\n";

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    anonlib::abort()
}

fn wait_for(
    ring: &anonlib::Ring,
    completions: &mut [Completion],
    expected: usize,
) -> Result<(), ()> {
    let count = ring.wait(completions, expected, 0).map_err(|_| ())?;
    if count != expected {
        return Err(());
    }
    Ok(())
}

fn valid_completion(completion: &Completion, user_data: u64, value0: usize) -> bool {
    completion.user_data() == user_data
        && completion.status().is_ok()
        && completion.values() == (value0 as u64, 0)
}

unsafe fn write_payload(target: *mut u8) {
    unsafe { core::ptr::copy_nonoverlapping(PAYLOAD.as_ptr(), target, PAYLOAD.len()) };
}

unsafe fn clear_payload(target: *mut u8) {
    unsafe { core::ptr::write_bytes(target, 0, PAYLOAD.len()) };
}

unsafe fn payload_matches(target: *const u8) -> bool {
    let source = PAYLOAD.as_ptr();
    let mut index = 0;
    while index < PAYLOAD.len() {
        if unsafe { target.add(index).read() } != unsafe { source.add(index).read() } {
            return false;
        }
        index += 1;
    }
    true
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let Some(process) = Process::current() else {
        return 10;
    };
    let Some(address_space) = AddressSpace::current() else {
        return 11;
    };
    let Some(root) = Directory::root() else {
        return 12;
    };

    let _ = root.remove(FILE_PATH, false);
    let file_rights = FileRights::READ | FileRights::WRITE | FileRights::RESIZE;
    let Ok(file) = root.create_file(FILE_PATH, file_rights) else {
        return 13;
    };
    if file.resize(OBJECT_SIZE).is_err() {
        return 14;
    }

    let Ok(memory) =
        process.create_memory(MemoryCreate::anonymous(OBJECT_SIZE, PAGE_SIZE as u64).shared())
    else {
        return 15;
    };
    let Ok(mapping) = memory.map(
        &address_space,
        0,
        OBJECT_SIZE,
        MemoryPermissions::READ_WRITE,
    ) else {
        return 16;
    };
    if mapping.length() != OBJECT_SIZE {
        return 17;
    }
    let source = mapping.address() as *mut u8;
    let target = unsafe { source.add(PAGE_SIZE) };
    unsafe {
        write_payload(source);
        clear_payload(target);
    }

    let Ok(ring) = process.create_ring(8) else {
        return 18;
    };
    let Ok(registration) = ring.register(&memory, 0, OBJECT_SIZE) else {
        return 19;
    };

    let file_write = Submission::file_write(&file, &registration, 0, PAYLOAD.len() as u64, 0, 1);
    if ring.kick(&[file_write]).ok() != Some(1) {
        return 20;
    }
    let mut completions = [Completion::default(); 2];
    if wait_for(&ring, &mut completions[..1], 1).is_err()
        || !valid_completion(&completions[0], 1, PAYLOAD.len())
    {
        return 21;
    }

    let file_read = Submission::file_read(
        &file,
        &registration,
        PAGE_SIZE as u64,
        PAYLOAD.len() as u64,
        0,
        2,
    );
    if ring.kick(&[file_read]).ok() != Some(1)
        || wait_for(&ring, &mut completions[..1], 1).is_err()
        || !valid_completion(&completions[0], 2, PAYLOAD.len())
        || !unsafe { payload_matches(target) }
    {
        return 22;
    }

    unsafe { clear_payload(target) };
    let Ok((sender, receiver)) = process.create_channel(4) else {
        return 23;
    };
    let channel_calls = [
        Submission::channel_send(&sender, &registration, 0, PAYLOAD.len() as u64, 3),
        Submission::channel_receive(
            &receiver,
            &registration,
            PAGE_SIZE as u64,
            PAYLOAD.len() as u64,
            4,
        ),
    ];
    if ring.kick(&channel_calls).ok() != Some(2)
        || wait_for(&ring, &mut completions, 2).is_err()
        || !valid_completion(&completions[0], 3, PAYLOAD.len())
        || !valid_completion(&completions[1], 4, PAYLOAD.len())
        || !unsafe { payload_matches(target) }
    {
        return 24;
    }

    let Ok(statistics) = memory.statistics() else {
        return 25;
    };
    if statistics.mapped_pages < 2
        || statistics.materialized_pages == 0
        || statistics.resident_mappings < statistics.materialized_pages
        || statistics.read_operations < 2
        || statistics.write_operations < 2
        || statistics.bytes_read < (PAYLOAD.len() * 2) as u64
        || statistics.bytes_written < (PAYLOAD.len() * 2) as u64
        || statistics.writeback_operations != 0
    {
        return 26;
    }

    let Ok(mapped_file) = root.open_file(FILE_PATH, FileRights::READ | FileRights::MAP) else {
        return 27;
    };
    let Ok(file_memory) = mapped_file.memory(0, OBJECT_SIZE, MemoryPermissions::READ) else {
        return 28;
    };
    let Ok(file_mapping) = file_memory.map(&address_space, 0, OBJECT_SIZE, MemoryPermissions::READ)
    else {
        return 29;
    };
    let _ =
        unsafe { core::ptr::read_volatile((file_mapping.address() as *const u8).add(PAGE_SIZE)) };
    let Ok(resizer) = root.open_file(FILE_PATH, FileRights::RESIZE) else {
        return 30;
    };
    if resizer.resize(PAGE_SIZE as u64).is_err()
        || file_memory.query().ok().map(|info| info.source_size) != Some(PAGE_SIZE as u64)
    {
        return 31;
    }
    if address_space.unmap(file_mapping).is_err() {
        return 32;
    }
    drop(file_memory);

    drop((sender, receiver));
    if ring.unregister(registration).is_err() {
        return 33;
    }
    drop(ring);
    if address_space.unmap(mapping).is_err() || memory.revoke().is_err() {
        return 34;
    }
    drop(file);
    if root.remove(FILE_PATH, false).is_err() {
        return 35;
    }

    let Some(stdout) = anonlib::stdout() else {
        return 36;
    };
    match stdout.write(PASS_MESSAGE) {
        Ok(written) if written == PASS_MESSAGE.len() => 0,
        _ => 37,
    }
}
