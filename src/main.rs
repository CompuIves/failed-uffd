mod manager_thread;

use std::{
    env::args,
    ffi::CString,
    fs::File,
    os::unix::prelude::{AsRawFd, FromRawFd},
    ptr::{copy_nonoverlapping, null_mut},
    thread,
};

use crossbeam_channel::{bounded, Sender};
use manager_thread::UffdMessage;
use nix::{
    libc,
    poll::{poll, PollFd, PollFlags},
    sys::{
        memfd::{self, MemFdCreateFlag},
        mman::{self, ProtFlags},
    },
};
use rand::Rng;
use tracing::{debug, error, info, metadata::LevelFilter, trace, warn};
use userfaultfd::{FeatureFlags, RegisterMode, Uffd, UffdBuilder};

fn create_memfd(size: u64) -> File {
    let memfd = {
        let name = CString::new("memory-snap").expect("failed to create memory-snap CString");

        memfd::memfd_create(&name, MemFdCreateFlag::empty()).expect("failed to create memfd")
    };
    let file = unsafe { File::from_raw_fd(memfd) };
    file.set_len(size)
        .expect("failed to set size of memfd file");

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
        mman::mmap(
            null_mut(),
            size as _,
            ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
            mman::MapFlags::MAP_SHARED,
            memfd.as_raw_fd(),
            0,
        )
        .expect("failed to create mapping A")
    };

    let b = unsafe {
        mman::mmap(
            null_mut(),
            size as _,
            ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
            mman::MapFlags::MAP_SHARED,
            memfd.as_raw_fd(),
            0,
        )
        .expect("failed to create mapping B")
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
        .expect("failed to create uffd");

    trace!("[{vm_idx}] registering uffd");
    // Register the mapping
    uffd.register_with_mode(
        mapping_a as _,
        size as _,
        RegisterMode::WRITE_PROTECT | RegisterMode::MISSING,
    )
    .expect("failed to register uffd with mdoes");

    let copied_uffd = unsafe { Uffd::from_raw_fd(uffd.as_raw_fd()) };

    let (tx, rx) = bounded(0);
    trace!("[{vm_idx}] sending vm add event");
    // Send the new UFFD to the manager thread
    sender
        .send(UffdMessage::AddVm(
            manager_thread::Vm {
                vm_idx,
                uffd_address: mapping_a,
                memfd_address: mapping_b,
                memfd_size: size,
                uffd_handler: copied_uffd,
                source_vm_idx,
            },
            tx,
        ))
        .expect("failed to send message to manager thread");

    trace!("[{vm_idx}] spawning uffd thread");
    // Spawn a thread that listens for new events that can come in
    std::thread::spawn(move || {
        // Loop, handling incoming events on the userfaultfd file descriptor.
        let pollfd = PollFd::new(uffd.as_raw_fd(), PollFlags::POLLIN);
        loop {
            // Wait for fd to become available
            poll(&mut [pollfd], -1).expect("failed to poll uffd");
            let revents = pollfd.revents().expect("failed to get revents");

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
                    .expect("failed to send message to manager thread");
            } else {
                warn!("uffd event was None");
            }
        }
    });

    // Wait for a confirmation that the uffd is armed
    rx.recv().expect("failed to send message to manager thread");
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

    info!("spawning pf handler thread for mem size of {size} bytes");
    let sender = manager_thread::start_thread();

    info!("initializing VM 0");
    let (mapping_0_a, mapping_0_b) = create_mappings(size);
    arm_uffd(0, mapping_0_a, mapping_0_b, size, sender.clone(), None);

    info!("writing a ton of data to VM 0");
    // First initialise all memory for VM 0
    unsafe {
        libc::memset(mapping_0_a as _, 1, size as _);
    }

    // Then create a new VM (1) which is a child of VM 0
    info!("initializing VM 1 as child of 0");
    let (mapping_1_a, mapping_1_b) = create_mappings(size);
    arm_uffd(1, mapping_1_a, mapping_1_b, size, sender, Some(0));

    let mut threads = vec![];
    // Finally, simultaneously write to VM 0 and read from VM 1
    threads.push(
        thread::Builder::new()
            .name("write_thread".to_string())
            .spawn(move || {
                write_randomly(0, mapping_0_a, 0, size as _, 2);
            })
            .unwrap(),
    );

    threads.push(
        thread::Builder::new()
            .name("read_thread".to_string())
            .spawn(move || {
                read_randomly(1, mapping_1_a, 0, size as _, 1);
            })
            .unwrap(),
    );

    for thread in threads {
        thread.join().expect("thread join failed");
    }

    info!("done!");
}

fn write_randomly(vm_idx: u16, addr: u64, base_offset: u64, len: usize, byte: u8) {
    let mut rng = rand::thread_rng();

    let mut offset = 0;
    while offset < len {
        let len = std::cmp::min(len - offset, rng.gen_range(1..(4096 * 2)));
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

        unsafe { libc::memset((addr + offset as u64) as _, byte as _, len) };

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

fn read_randomly(idx: u16, addr: u64, base_offset: u64, len: usize, expected_byte: u8) {
    let mut rng = rand::thread_rng();

    let mut offset = 0;
    while offset < len {
        let len = std::cmp::min(len - offset, rng.gen_range(1..4096));
        // let len = 4096;

        let start_page = (base_offset + offset as u64) / 4096;
        let pages = (start_page..=(start_page + (len as u64 / 4096)))
            .map(|n| format!("({})", n))
            .collect::<Vec<_>>()
            .join(",");
        debug!(
            "[{}] READ: offset: {}, len: {}, pages: {}",
            idx, offset, len, pages
        );
        let slice = vec![expected_byte; len];
        let read = read_from_pointer(addr + offset as u64, len);
        // debug!(
        //     "[{}] READ DONE: offset: {}, len: {}, pages: {}",
        //     idx, offset, len, pages
        // );

        let is_two_pages = (offset % 4096) + len > 4096;

        let wrong_index = read.iter().enumerate().find(|(i, &b)| b != slice[*i]);
        if let Some((index, value)) = wrong_index {
            error!(
                "[{}] could not read from base addr {} relative position {}, wrong val: {}, len: {}, page: {}, two pages: {}",
                idx,
                addr,
                index,
                value,
                len,
                (base_offset as usize + offset) / 4096,
                is_two_pages
            );

            panic!(
                "[{}] could not read from base addr {} relative position {}, wrong val: {}, len: {}, page: {}, two pages: {}",
                idx,
                addr,
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
