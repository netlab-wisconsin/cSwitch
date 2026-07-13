# Vendored Core Manifest

This file is a quick browsing guide for the vendored `scx_rustland_core`
snapshot used by `scx-rustland-la`.

This is not a pristine upstream copy anymore. The root project depends on local
ABI/backend changes in this vendored subtree.

## Top Level

- `Cargo.toml`: vendored crate manifest used by the root build.
- `README.md`: crate-level overview for this local vendored snapshot.
- `LICENSE`: upstream GPL-2.0-only license.
- `MANIFEST.md`: this file.
- `bindings.h`: bindgen entry used by the builder toolchain.

## assets

- `assets/bpf/intf.h`: shared ABI between BPF and userspace. In this tree it
  includes per-task telemetry such as `tid`, `tgid`, current CPU/domain,
  fill-bandwidth proxy, IPC/stall fields, queue triggers, and `tick_holdoff`,
  plus per-domain LLC/DF state structures.
- `assets/bpf/main.bpf.c`: vendored rustland BPF backend template. In this tree
  it adds perf-event sampling, tick-reschedule hooks, per-CPU/domain state maps,
  per-domain LLC/DF state maps, richer queued-task telemetry, userspace-fed
  domain hotness data, direct dispatch and tick fast-pathing for
  one-CPU-affinity tasks, stale-dispatch rescue plus dispatch-path counters,
  and a self-hosted usersched wake path.
- `assets/bpf.rs`: template for the generated Rust BPF connector emitted into
  the root crate as `src/bpf.rs`; the local template exposes explicit
  re-entry into `sched_ext` for the main userspace scheduler thread, publishes
  pending counts after queued-work drains and at completion, and yields once at
  completion so BPF can drain userspace dispatch submissions.

## src

- `src/lib.rs`: exports the allocator and `RustLandBuilder`.
- `src/rustland_builder.rs`: writes the vendored asset set into the root crate
  and invokes the generation flow for BPF bindings/skeletons.
- `src/alloc.rs`: custom userspace allocator and memory-locking helpers.
  `disable_mmap()` is intentionally a no-op in this vendored tree so local
  builds do not require a `libseccomp` development package.
- `src/bpf_intf.rs`: generated bindings used internally by the vendored crate.
- `src/bpf_skel.rs`: generated skeleton support used internally by the vendored
  crate.

## bpf_h

- `bpf_h/`: bundled `scx` and helper headers consumed by `scx_cargo` /
  libbpf during BPF compilation.

## Local Modifications

The main local changes relative to upstream are concentrated in:

1. `assets/bpf/intf.h`
2. `assets/bpf/main.bpf.c`
3. `assets/bpf.rs`
4. `src/alloc.rs`

If you need to rebase this vendored snapshot on a newer upstream version,
review those files first.
