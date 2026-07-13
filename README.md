# scx-rustland-la

`scx-rustland-la` is the scheduler source repository. It contains the Rust
userspace scheduler, the shared BPF backend, vendored build inputs, and
scheduler tests. Paper evaluation and artifact-evaluation runners intentionally
live in a separate repository/package.

## Requirements

- Linux with `sched_ext` support
- cgroup v2
- Rust and Cargo
- the `libbpf`, Clang, and bpftool toolchain used by `scx_cargo`
- root privileges and `/dev/cpu/<n>/msr` access at runtime
- a host-specific CCM/CS-link mapping file

## Build

Build the default scheduler:

```sh
cargo build --release --bin scx-rustland-la
```

Build the scheduler used as cSwitch in the paper:

```sh
cargo build --release --bin scx-rustland-la \
  --no-default-features \
  --features tick-resched,scheduler-paper-greedy
```

Other mutually exclusive policy features include `scheduler-arcas` and
`scheduler-nsdi`.

## Run

Inspect the CLI:

```sh
./target/release/scx-rustland-la --help
```

Launch one managed command:

```sh
sudo ./target/release/scx-rustland-la \
  --cgroup-path /sys/fs/cgroup/scx-rustland-la-demo \
  --ccm-mapping-path /path/to/ccm_mapping.txt \
  -- /bin/sleep 1
```

## Test

Run non-hardware tests with:

```sh
cargo test
```

Hardware smoke tests are marked ignored and require the corresponding host,
MSR access, and benchmark helpers.

## Repository Boundary

This repository does not contain `ae/`, `eval/`, paper plots, benchmark result
trees, or paper PDFs. The external artifact package supplies those files and
points its harness at this checkout through `AE_SCHEDULER_ROOT`.
