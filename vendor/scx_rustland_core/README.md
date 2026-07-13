# Vendored `scx_rustland_core` Snapshot

This directory contains the vendored `scx_rustland_core` framework used by
`scx-rustland-la`.

It still provides the same basic role as upstream rustland:

- a BPF backend template for userspace schedulers
- a Rust builder that copies/regenerates the backend into the root crate
- a generated Rust connector for queueing and dispatch

However, this snapshot is not a pristine upstream copy. The local tree extends
the ABI and backend specifically for `scx-rustland-la`.

## What Is Different In This Snapshot

Relative to a generic rustland backend, the local vendored snapshot adds:

- richer queued-task telemetry
  - `tid`, `tgid`
  - current CPU/domain
  - per-thread fill-bandwidth proxy
  - IPC / stall telemetry
  - queue trigger and tick sequence
- richer dispatched-task control
  - `tick_holdoff`
- more userspace-visible kernel maps
  - `cpu_domain_map`
  - `cpu_state_map`
  - `llc_state_map`
  - `df_state_map`
- dispatch-path counters for ring-buffer drains, DSQ inserts/consumes,
  stale-dispatch rescues, cancellations, bounces, kicks, and missing task
  lookups
- direct BPF dispatch and tick fast-pathing for one-CPU-affinity tasks, which
  have no userspace placement alternative
- optional tick-reschedule support in BPF
- optional BPF stay-fastpath support for cold tasks
- a self-hosted usersched path so the root scheduler's main loop can enter
  `sched_ext` and be woken by BPF while helper/control-plane threads remain
  normal host threads; the Rust side publishes the local pending count after
  draining queued work, then publishes the remaining count and yields once at
  completion so BPF `dispatch()` can drain userspace dispatch submissions

The root project then copies those assets into:

- `intf.h`
- `main.bpf.c`
- `src/bpf.rs`
- `src/bpf_intf.rs`
- `src/bpf_skel.rs`

## Primary Files

- `assets/bpf/intf.h`: source of truth for the shared BPF/userspace ABI
- `assets/bpf/main.bpf.c`: source of truth for the BPF backend template
- `assets/bpf.rs`: source of truth for the generated Rust connector template
- `src/rustland_builder.rs`: asset-copy and generation entrypoint
- `src/alloc.rs`: local allocator tweak that makes `disable_mmap()` a no-op

## How The Root Project Uses This Crate

`scx-rustland-la` does not edit the generated root BPF files directly. Instead,
it edits this vendored asset set and lets the build step regenerate the root
outputs.

That means:

- change the ABI/backend here first
- rebuild the root project
- treat the generated root bindings as derived artifacts

## Scope

This vendored snapshot is maintained as implementation support for
`scx-rustland-la`, not as a general-purpose standalone crate release. For the
actual scheduler behavior and workflow, read the root
[`README.md`](../../README.md).
