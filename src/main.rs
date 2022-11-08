mod manager_thread;

use std::{
    env::args,
    ffi::CString,
    fs::File,
    os::unix::prelude::{AsRawFd, FromRawFd},
    ptr::{copy_nonoverlapping, null_mut},
};

use crossbeam_channel::Sender;
use manager_thread::UffdMessage;
use nix::{
    libc::{self, memfd_create},
    poll::{poll, PollFd, PollFlags},
};
use rand::Rng;
use tracing::{debug, error, info, metadata::LevelFilter, trace, warn};
use userfaultfd::{FeatureFlags, RegisterMode, Uffd, UffdBuilder};

fn create_memfd(size: u64) -> File {
    let memfd = unsafe {
        let name = CString::new("memory-snap").unwrap();
        memfd_create(name.as_ptr() as _, 0)
    };
    let file = unsafe { File::from_raw_fd(memfd) };
    file.set_len(size).unwrap();

    file
}

/// Creates a VM with two mappings on a single memfd:
/// - A: a shared mapping that's used to copy from and to
/// - B: the UFFD armed mapping that's used inside the VM
///
/// Returns the addresses (a tuple of the two addresses)
fn create_mappings(size: u64) -> (u64, u64) {
    let memfd = create_memfd(size);

    let a = unsafe {
        libc::mmap(
            null_mut(),
            size as _,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            memfd.as_raw_fd(),
            0,
        )
    };

    let b = unsafe {
        libc::mmap(
            null_mut(),
            size as _,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            memfd.as_raw_fd(),
            0,
        )
    };

    (a as u64, b as u64)
}

/// Registers the UFFD for the given mapping and size, then it will spawn a thread
/// that listens to the uffd socket and send events for the manager thread.
fn arm_uffd(
    vm_idx: u16,
    mapping_a: u64,
    mapping_b: u64,
    size: u64,
    sender: Sender<UffdMessage>,
    source_vm_idx: Option<u16>,
) {
    trace!("[{vm_idx}] creating uffd");
    // First initialise UFFD
    let uffd = UffdBuilder::new()
        .close_on_exec(true)
        .require_features(
            FeatureFlags::EVENT_REMOVE
                | FeatureFlags::EVENT_REMAP
                | FeatureFlags::EVENT_FORK
                | FeatureFlags::EVENT_UNMAP
                | FeatureFlags::MISSING_SHMEM
                | FeatureFlags::MINOR_SHMEM
                | FeatureFlags::PAGEFAULT_FLAG_WP,
        )
        .non_blocking(true)
        .user_mode_only(false)
        .create()
        .unwrap();

    trace!("[{vm_idx}] registering uffd");
    // Register the mapping
    uffd.register_with_mode(
        mapping_a as _,
        size as _,
        RegisterMode::WRITE_PROTECT | RegisterMode::MISSING,
    )
    .unwrap();

    let copied_uffd = unsafe { Uffd::from_raw_fd(uffd.as_raw_fd()) };

    trace!("[{vm_idx}] sending vm add event");
    // Send the new UFFD to the manager thread
    sender
        .send(UffdMessage::AddVm(manager_thread::Vm {
            vm_idx,
            uffd_address: mapping_a,
            memfd_address: mapping_b,
            memfd_size: size,
            uffd_handler: copied_uffd,
            source_vm_idx,
        }))
        .unwrap();

    trace!("[{vm_idx}] spawning uffd thread");
    // Spawn a thread that listens for new events that can come in
    std::thread::spawn(move || {
        // Loop, handling incoming events on the userfaultfd file descriptor.
        let pollfd = PollFd::new(uffd.as_raw_fd(), PollFlags::POLLIN);
        loop {
            // Wait for fd to become available
            poll(&mut [pollfd], -1).unwrap();
            let revents = pollfd.revents().unwrap();

            if revents.contains(PollFlags::POLLERR) {
                error!("poll returned POLLERR");
            }

            // Read an event from the userfaultfd.
            let event = uffd.read_event().expect("Failed to read uffd_msg");

            if let Some(event) = event {
                sender
                    .send(UffdMessage::UffdEvent {
                        vm_idx,
                        event: event.into(),
                    })
                    .unwrap();
            } else {
                warn!("uffd event was None");
            }
        }
    });
}

fn main() {
    // Logging
    tracing_subscriber::fmt()
        .compact()
        .with_max_level(LevelFilter::TRACE)
        .with_ansi(false)
        .with_thread_ids(true)
        .try_init()
        .unwrap();

    let size = args()
        .nth(1)
        .unwrap_or_else(|| "256".into())
        .parse::<u64>()
        .unwrap()
        * 1024
        * 1024; // 256MiB
    let buffer_1 = vec![1; size as usize];

    info!("spawning pf handler thread for mem size of {size} bytes");
    let sender = manager_thread::start_thread();

    info!("initializing VM 0");
    let (mapping_0_a, mapping_0_b) = create_mappings(size);
    arm_uffd(0, mapping_0_a, mapping_0_b, size, sender.clone(), None);

    info!("writing a ton of data to VM 0");
    // First initialise all memory for VM 0
    write_to_pointer(mapping_0_a, &buffer_1);

    // Then create a new VM (1) which is a child of VM 0
    info!("initializing VM 1 as child of 0");
    let (mapping_1_a, mapping_1_b) = create_mappings(size);
    arm_uffd(1, mapping_1_a, mapping_1_b, size, sender, Some(0));

    let mut threads = vec![];
    // Finally, simultaneously write to VM 0 and read from VM 1
    threads.push(std::thread::spawn(move || {
        write_randomly(0, mapping_0_a, 0, vec![2; size as usize]);
    }));

    threads.push(std::thread::spawn(move || {
        read_randomly(1, mapping_1_a, 0, &buffer_1);
    }));

    for thread in threads {
        thread.join().unwrap();
    }
}

fn write_randomly(vm_idx: u16, addr: u64, base_offset: u64, buffer: Vec<u8>) {
    let mut rng = rand::thread_rng();

    let mut offset = 0;
    while offset < buffer.len() {
        let len = std::cmp::min(buffer.len() - offset, rng.gen_range(1..(4096 * 2)));
        if len == 0 {
            offset += len;

            continue;
        }
        let page_idx = (base_offset + offset as u64) / 4096;
        let pages = (page_idx..=(page_idx + (len as u64 / 4096)))
            .map(|n| format!("({})", n))
            .collect::<Vec<_>>()
            .join(",");

        debug!(
            "[{}] WRITE: {} {:?} {:?} pages: {}",
            vm_idx,
            addr + offset as u64,
            offset,
            len,
            pages
        );

        let slice = &buffer[offset..offset + len];
        write_to_pointer(addr + offset as u64, slice);

        debug!(
            "[{}] WRITE DONE: {} {:?} {:?} pages: {}",
            vm_idx,
            addr + offset as u64,
            offset,
            len,
            pages
        );
        offset += len;
    }
}

fn read_randomly(idx: u16, addr: u64, base_offset: u64, buffer: &Vec<u8>) {
    let mut rng = rand::thread_rng();

    let mut offset = 0;
    while offset < buffer.len() {
        let len = std::cmp::min(buffer.len() - offset, rng.gen_range(1..4096));
        // let len = 4096;

        let start_page = base_offset + offset as u64;
        let pages = (start_page..=(start_page + (len as u64 / 4096)))
            .map(|n| format!("({})", n))
            .collect::<Vec<_>>()
            .join(",");
        debug!(
            "[{}] READ: offset: {}, len: {}, pages: {}",
            idx, offset, len, pages
        );
        let slice = &buffer[offset..offset + len];
        let read = read_from_pointer(addr + offset as u64, len);
        // debug!(
        //     "[{}] READ DONE: offset: {}, len: {}, pages: {}",
        //     idx, offset, len, pages
        // );

        let is_two_pages = (offset % 4096) + len > 4096;

        let wrong_index = read.iter().enumerate().find(|(i, &b)| b != slice[*i]);
        if let Some((index, value)) = wrong_index {
            error!(
                "[{}] could not read from relative position {}, wrong val: {}, len: {}, page: {}, two pages: {}",
                idx,
                index,
                value,
                len,
                (base_offset as usize + offset) / 4096,
                is_two_pages
            );

            panic!(
                "[{}] could not read from relative position {}, wrong val: {}, len: {}, page: {}, two pages: {}",
                idx,
                index,
                value,
                len,
                (base_offset as usize + offset) / 4096,
                is_two_pages
            );
        }
        offset += len;
    }
}

fn read_from_pointer(addr: u64, size: usize) -> Vec<u8> {
    let mut buf = vec![0; size];
    unsafe {
        copy_nonoverlapping(addr as *const u8, buf.as_mut_ptr(), size);
    }

    buf
}

fn write_to_pointer(addr: u64, buf: &[u8]) {
    unsafe {
        copy_nonoverlapping(buf.as_ptr(), addr as *mut u8, buf.len());
    }
}
