#!/usr/bin/env bash

cargo build --release && while ./target/release/uffd-vm-test &> output-test.log; do echo succeeded; done