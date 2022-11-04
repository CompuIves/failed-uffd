# UFFD Handler Test

This reproducible shows a potential bug where a write protection bit is removed
without explicitly calling `remove_write_protection`. To run the test, you need
to run `run.sh`, this will repeat the test until the race condition happens. It
can take a long time until the race condition happens, I've been able to
reproduce it with this repo when running the test in parallel 3 times:

```sh
# All in a different shell
while cargo run --release  &> output-test1.log; do echo succeeded; done
while cargo run --release  &> output-test2.log; do echo succeeded; done
while cargo run --release  &> output-test3.log; do echo succeeded; done
```

## Structure

This project has two files:

- `main.rs`
- `manager_thread.rs`

In `main.rs` we setup up the project, and we spawn two VMs (0 & 1). 1 is a child
of 0. We randomly write to VM 0, and we randomly read from 1, expecting that the
pages are all from before the change went wrong.

In `manager_thread.rs` we run the main thread, this thread receives all UFFD
events (from both VMs) and it's responsible for operating the UFFD. For every
VM, it creates a "page source" vector, this vector describes for every page
where that page needs to be fetched from. By default this is `Zero`, but if a VM
is a child of another VM, that can be `Vm(u16)`.

After a clone of a VM, we copy the page source vector so this new VM has the
same sources. If a page is resolved by UFFD for a VM, we will replace that page
with its own source (so new children will know to fetch from it).
