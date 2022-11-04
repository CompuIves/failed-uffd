use std::collections::HashMap;

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
    AddVm(Vm),
    /// UFFD event.
    UffdEvent { vm_idx: u16, event: UffdEvent },
}

#[derive(Clone, Copy, Eq, PartialEq, Hash)]
pub enum VmBackend {
    Zero,
    Vm(u16),
}

pub(crate) fn start_thread() -> Sender<UffdMessage> {
    let (tx, rx) = crossbeam_channel::unbounded();

    // Spawn a thread that listens for new events that can come in, either for a
    // new page fault or us adding new VMs.
    std::thread::spawn(move || {
        let mut vms: HashMap<u16, Vm> = HashMap::new();
        let mut page_sources: HashMap<u16, Vec<VmBackend>> = HashMap::new();

        loop {
            let event = rx.recv().unwrap();

            match event {
                UffdMessage::AddVm(vm) => {
                    if let Some(parent_vm) = vm.source_vm_idx {
                        let parent_vm: &Vm = vms.get(&parent_vm).unwrap();

                        debug!("[{}] write protecting full memory range", parent_vm.vm_idx);
                        parent_vm
                            .uffd_handler
                            .write_protect(parent_vm.uffd_address as _, parent_vm.memfd_size as _)
                            .unwrap();

                        page_sources.insert(
                            vm.vm_idx,
                            page_sources.get(&parent_vm.vm_idx).unwrap().clone(),
                        );
                    } else {
                        page_sources.insert(vm.vm_idx, vec![VmBackend::Zero; vm.memfd_size as _]);
                    }

                    vms.insert(vm.vm_idx, vm);
                }
                UffdMessage::UffdEvent { vm_idx, event } => {
                    let vm = vms.get(&vm_idx).expect("vm not found");

                    if event.kind == FaultKind::Missing {
                        let page_idx = (event.address - vm.uffd_address) / 4096;
                        trace!("[{vm_idx}] handling missing fault on page ({page_idx})");

                        let vm_backends = page_sources.get_mut(&vm_idx).unwrap();
                        let backend = vm_backends[page_idx as usize];

                        let is_own_backend = match backend {
                            VmBackend::Zero => false,
                            VmBackend::Vm(backend_vm_idx) => backend_vm_idx == vm_idx,
                        };
                        if is_own_backend {
                            trace!("[{vm_idx}] page ({page_idx}) is own backend, waking...");
                            vm.uffd_handler.wake(event.address as _, 4096).unwrap();
                            continue;
                        };

                        match backend {
                            VmBackend::Zero => unsafe {
                                trace!("[{vm_idx}] zeroing page ({page_idx})");
                                vm.uffd_handler
                                    .zeropage(event.address as _, 4096, true)
                                    .unwrap();
                            },
                            VmBackend::Vm(parent_vm_idx) => {
                                trace!(
                                    "[{vm_idx}] copying page ({page_idx}) from [{parent_vm_idx}]"
                                );
                                let relative_address = page_idx * 4096;
                                let parent_vm = vms.get(&parent_vm_idx).unwrap();

                                unsafe {
                                    vm.uffd_handler
                                        .copy(
                                            (parent_vm.memfd_address + relative_address) as _,
                                            (vm.uffd_address + relative_address) as _,
                                            4096,
                                            true,
                                            false,
                                        )
                                        .unwrap();
                                }
                            }
                        }

                        // Mark the source of this page now as the current VM.
                        vm_backends[page_idx as usize] = VmBackend::Vm(vm_idx);
                    } else {
                        // Write protected fault
                        let page_idx = (event.address - vm.uffd_address) / 4096;
                        let relative_addr = page_idx * 4096;

                        trace!("[{vm_idx}] handling write protection fault on page ({page_idx})");

                        // Copy the contents immediately to every VM that has this VM as source for
                        // this page.
                        let min_bytes_copied = page_sources
                            .iter_mut()
                            .filter(|(child_vm_idx, v)| {
                                **child_vm_idx != vm_idx
                                    && v[page_idx as usize] == VmBackend::Vm(vm_idx)
                            })
                            .map(|(child_vm_idx, child_sources)| {
                                let child_vm = vms.get(child_vm_idx).unwrap();
                                trace!("[{vm_idx}] copying page ({page_idx}) to [{child_vm_idx}]");
                                let bytes_copied = unsafe {
                                    child_vm
                                        .uffd_handler
                                        .copy(
                                            (vm.memfd_address + relative_addr) as _,
                                            (child_vm.uffd_address + relative_addr) as _,
                                            4096,
                                            false,
                                            false,
                                        )
                                        .unwrap()
                                };

                                child_sources[page_idx as usize] = VmBackend::Vm(*child_vm_idx);

                                bytes_copied
                            })
                            .min()
                            .unwrap_or(4096);

                        debug!("[{vm_idx}] removing write protection for page ({page_idx})");
                        vm.uffd_handler
                            .remove_write_protection(event.address as _, min_bytes_copied, true)
                            .unwrap();
                    }
                }
            };

            // dbg!(event);
        }
    });

    tx
}
