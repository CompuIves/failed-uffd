use std::{collections::HashMap, ops::Range};

use crossbeam_channel::Sender;
use tracing::{debug, trace};
use userfaultfd::{FaultKind, ReadWrite, Uffd};

#[derive(Debug)]
pub(crate) struct Vm {
    pub vm_idx: u16,
    pub uffd_address: u64,
    pub memfd_address: u64,
    pub memfd_size: u64,
    pub uffd_handler: Uffd,
    pub source_vm_idx: Option<u16>,
}

pub(crate) struct UffdEvent {
    pub address: u64,
    pub kind: FaultKind,
    pub rw: ReadWrite,
}

impl From<userfaultfd::Event> for UffdEvent {
    fn from(val: userfaultfd::Event) -> Self {
        match val {
            userfaultfd::Event::Pagefault { kind, rw, addr } => UffdEvent {
                address: addr as u64,
                kind,
                rw,
            },
            _ => {
                unimplemented!()
            }
        }
    }
}

pub(crate) enum UffdMessage {
    /// Adds a VM to the list of VMs that are being tracked by the manager thread.
    AddVm(Vm, Sender<()>),
    /// UFFD event.
    UffdEvent { vm_idx: u16, event: UffdEvent },
}

#[derive(Clone, Copy, Eq, PartialEq, Hash)]
pub enum VmBackend {
    Zero,
    Vm(u16),
}

pub(crate) fn start_thread() -> Sender<UffdMessage> {
    let (tx, rx) = crossbeam_channel::bounded(0);

    const MAX_PAGES_RW: usize = 1024;

    // Spawn a thread that listens for new events that can come in, either for a
    // new page fault or us adding new VMs.
    std::thread::spawn(move || {
        let mut vms: HashMap<u16, Vm> = HashMap::new();
        let mut page_sources: HashMap<u16, Vec<VmBackend>> = HashMap::new();

        loop {
            let event = rx.recv().expect("failed to recv message");

            match event {
                UffdMessage::AddVm(vm, ready_sender) => {
                    if let Some(parent_vm) = vm.source_vm_idx {
                        let parent_vm: &Vm = vms.get(&parent_vm).expect("parent vm not found");

                        debug!("[{}] write protecting full memory range", parent_vm.vm_idx);
                        parent_vm
                            .uffd_handler
                            .write_protect(parent_vm.uffd_address as _, parent_vm.memfd_size as _)
                            .expect("write protecting failed");
                        debug!("[{}] write protected full memory range", parent_vm.vm_idx);

                        // Now clone the page source from the parent for this VM
                        page_sources.insert(
                            vm.vm_idx,
                            page_sources
                                .get(&parent_vm.vm_idx)
                                .expect("failed to get page source")
                                .clone(),
                        );
                    } else {
                        page_sources.insert(
                            vm.vm_idx,
                            vec![VmBackend::Zero; (vm.memfd_size / 4096) as _],
                        );
                    }

                    vms.insert(vm.vm_idx, vm);

                    // Send back that the VM is initialised
                    ready_sender.send(()).expect("failed to send ready signal");
                }
                UffdMessage::UffdEvent { vm_idx, event } => {
                    let vm = vms.get(&vm_idx).expect("vm not found");

                    if event.rw == ReadWrite::Read || event.kind == FaultKind::Missing {
                        let page_idx = (event.address - vm.uffd_address) / 4096;
                        trace!("[{vm_idx}] handling missing fault on page ({page_idx})");
                        let copied = {
                            let vm_sources =
                                page_sources.get(&vm_idx).expect("failed to get vm sources");
                            let backend = vm_sources[page_idx as usize];

                            // Find the backend that we should populate from
                            let is_own_backend = match backend {
                                VmBackend::Zero => false,
                                VmBackend::Vm(backend_vm_idx) => backend_vm_idx == vm_idx,
                            };
                            if is_own_backend {
                                // If this page is our own backend, we just wake the process
                                trace!("[{vm_idx}] page ({page_idx}) is own backend, waking...");
                                vm.uffd_handler
                                    .wake(event.address as _, 4096)
                                    .expect("failed to wake range");
                                continue;
                            };

                            match backend {
                                VmBackend::Zero => unsafe {
                                    let max_pages_to_copy =
                                        get_max_range(vm_sources, page_idx as usize, MAX_PAGES_RW);
                                    trace!(
                                        "[{vm_idx}] zeroing pages {}",
                                        print_pages_pretty(
                                            page_idx..page_idx + max_pages_to_copy as u64
                                        )
                                    );
                                    vm.uffd_handler
                                        .zeropage(
                                            event.address as _,
                                            max_pages_to_copy * 4096,
                                            true,
                                        )
                                        .expect("failed to zero page")
                                },
                                VmBackend::Vm(parent_vm_idx) => {
                                    let relative_address = page_idx * 4096;
                                    let parent_vm =
                                        vms.get(&parent_vm_idx).expect("failed to get vm");

                                    let max_pages_to_copy_source =
                                        get_max_range(vm_sources, page_idx as usize, MAX_PAGES_RW);
                                    let max_pages_to_copy_dest = get_max_range(
                                        page_sources
                                            .get(&parent_vm_idx)
                                            .expect("failed to get page sources"),
                                        page_idx as usize,
                                        MAX_PAGES_RW,
                                    );

                                    let max_pages_to_copy = std::cmp::min(
                                        max_pages_to_copy_source,
                                        max_pages_to_copy_dest,
                                    );

                                    trace!(
                                        "[{vm_idx}] copying pages {} from [{parent_vm_idx}]",
                                        print_pages_pretty(
                                            page_idx..page_idx + max_pages_to_copy as u64
                                        )
                                    );
                                    unsafe {
                                        vm.uffd_handler
                                            .copy(
                                                (parent_vm.memfd_address + relative_address) as _,
                                                (vm.uffd_address + relative_address) as _,
                                                max_pages_to_copy * 4096,
                                                true,
                                                false,
                                            )
                                            .expect("failed to copy bytes using uffd handler")
                                    }
                                }
                            }
                        };

                        let vm_sources = page_sources
                            .get_mut(&vm_idx)
                            .expect("failed to get page sources");

                        for i in 0..(copied / 4096) {
                            // Mark the source of the copied pages now as the current VM. This is important because
                            // clones will then know to load from this VM.
                            vm_sources[page_idx as usize + i] = VmBackend::Vm(vm_idx);
                        }
                    } else {
                        // Write protected fault
                        let page_idx = (event.address - vm.uffd_address) / 4096;
                        let relative_addr = page_idx * 4096;

                        trace!("[{vm_idx}] handling write protection fault on page ({page_idx})");

                        let max_pages_to_copy_source = get_max_range(
                            page_sources
                                .get(&vm_idx)
                                .expect("failed to get page sources"),
                            page_idx as usize,
                            MAX_PAGES_RW,
                        );
                        // Copy the contents immediately to every VM that has this VM as source for
                        // this page.
                        let min_bytes_copied = page_sources
                            .iter_mut()
                            .filter(|(child_vm_idx, v)| {
                                **child_vm_idx != vm_idx
                                    && v[page_idx as usize] == VmBackend::Vm(vm_idx)
                            })
                            .map(|(child_vm_idx, child_sources)| {
                                let child_vm =
                                    vms.get(child_vm_idx).expect("failed to get child vm");

                                let max_pages_to_copy_dest =
                                    get_max_range(child_sources, page_idx as usize, MAX_PAGES_RW);

                                let max_pages_to_copy =
                                    std::cmp::min(max_pages_to_copy_source, max_pages_to_copy_dest);

                                trace!(
                                    "[{vm_idx}] copying pages ({}) to [{child_vm_idx}]",
                                    print_pages_pretty(
                                        page_idx..page_idx + max_pages_to_copy as u64
                                    )
                                );

                                let bytes_copied = unsafe {
                                    child_vm
                                        .uffd_handler
                                        .copy(
                                            (vm.memfd_address + relative_addr) as _,
                                            (child_vm.uffd_address + relative_addr) as _,
                                            max_pages_to_copy * 4096,
                                            false,
                                            false,
                                        )
                                        .expect("failed to copy bytes to child using uffd handler")
                                };

                                child_sources[page_idx as usize] = VmBackend::Vm(*child_vm_idx);

                                bytes_copied
                            })
                            .min()
                            .unwrap_or(4096);

                        debug!(
                            "[{vm_idx}] removing write protection for pages: {}",
                            print_pages_pretty(page_idx..page_idx + min_bytes_copied as u64 / 4096)
                        );
                        vm.uffd_handler
                            .remove_write_protection(event.address as _, min_bytes_copied, true)
                            .expect("failed to remove write protection");
                    }
                }
            };

            // dbg!(event);
        }
    });

    tx
}

/// Finds how long this specific source exists after `index`.
fn get_max_range(page_sources: &[VmBackend], index: usize, limit: usize) -> usize {
    let checked_source = page_sources[index];

    let mut max_range = 0;
    for i in 0..limit {
        if !matches!(page_sources.get(index + i), Some(source) if *source == checked_source) {
            break;
        }

        max_range += 1;
    }

    max_range
}

fn print_pages_pretty(range: Range<u64>) -> String {
    range
        .into_iter()
        .map(|page_idx| format!("({page_idx})"))
        .collect::<Vec<_>>()
        .join(",")
}
