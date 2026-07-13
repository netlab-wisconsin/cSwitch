# SOSP 2026 AE Checklist Notes

This note translates the pasted SOSP 2026 Systems Research Artifacts checklist
into concrete artifact-package tasks.

## Available

- Public repository: <https://github.com/netlab-wisconsin/cSwitch>, with the
  evaluator tree pinned by tag `sosp26-ae-v2`. Archive the tagged release on a
  long-term service such as Zenodo when the final public version is ready.
- License: the scheduler and author material use GPL-2.0-only. Third-party
  provenance and licensing boundaries are recorded in `LICENSES.md`,
  `motivation/LICENSE.md`, and `docs/EXTERNAL_DEPENDENCIES.md`.
- README and figure mapping: the top-level `README.md` and
  `docs/PAPER_FIGURE_MAP.md`
  document the reproduced figures; `SCHEDULER.md` describes the scheduler.

## Functional

- Artifact components:
  - scheduler code and BPF backend: repository-root `src/`, `vendor/`, and
    `Cargo.toml`; the default `main` branch remains scheduler-only.
  - evaluator entry point: top-level `reproduce.sh`; internal AE components are
    `ae/harness.py`, `ae/check.sh`, `ae/run.sh`, `ae/summarize.sh`,
    `ae/plot.py`, and `ae/plot.sh`.
  - AE-local workload helpers: `ae/external_workload.py` for GAPBS and
    node-replication, plus Filebench templates under `ae/templates/`.
- Environment:
  - full runs target the supplied AMD EPYC 9634+CXL evaluation host.
  - fresh Intel comparison points for Figures 2a and 2b require a separate
    GenuineIntel host; the supplied AMD host covers their AMD points and all
    other supported current-host runs.
  - fresh Figure 4 and Figure 8d runs are outside the AE scope because they
    require physical DIMM changes and an NPS4/two-DIMM reboot. Their frozen
    published plots can still be regenerated from their frozen paper inputs.
  - root privileges and passwordless `sudo -n` for scheduler runs.
  - Linux kernel with `sched_ext`.
  - cgroup v2.
  - `/dev/cpu/<n>/msr` for hardware counter samplers.
  - AE-local `ae/ccm_mapping.txt` with mapped CCM capacity `20000 MiB/s` and
    CS0-CS11 throttle capacity `40000 MiB/s`
    CCM/CS links.
  - benchmark inputs default to `/home/seunghyun/gapbs`,
    `/home/seunghyun/ycsb`, and `/home/seunghyun/scx_rustland_eevdf`;
    `AE_GAPBS_ROOT`, `AE_YCSB_ROOT`, `AE_EEVDF_ROOT`, and the other path
    variables documented in `docs/AE_REFERENCE.md` override the author paths.
    Exact source URLs, revisions, patches, and input-generation instructions
    are in `docs/EXTERNAL_DEPENDENCIES.md`.
  - each evaluator uses a separate checkout, normally `~/ae`, with `umask 002`.
    A root-owned `/run/lock/cswitch-ae.lock` serializes fresh campaigns across
    all checkouts. Shared source and scripts are not world-writable.
- Minimal working example:
  - non-root CLI validation: `ae/check.sh`.
  - scheduler smoke: `ae/run.sh fig13 smoke`.
  - figure dry-run: `ae/run.sh fig13 dry-run`.
- Expected resource use:
  - dry-runs are seconds to minutes and do not start schedulers.
  - smoke runs are single-point root scheduler runs.
  - default AE runs keep the paper workload, variant, core, and rate axes, but
    use one repeat, no scheduler decision/runtime logs, and no `perf sched`.
  - the same campaign may launch the EEVDF, ARCAS, and Caladan+ ports, but the
    core reproduction claim is interpreted from cSwitch. Port results are
    contextual comparisons, not independent validation of the original systems.
  - Figure 10 defaults to the 12-workload paper matrix: RocksDB, OrientDB,
    Elasticsearch, GAPBS BC/PR, llama.cpp, two node-replication workloads, and
    four Filebench workloads.
  - Figure 11 defaults to llama.cpp, PageRank, and File Server under both
    heterogeneous free-core and busy-core asymmetric-noise cases.
  - Figure 12a and 12b use distinct paper launch shapes for compute-I/O link
    load and overall I/O-chiplet load, including a true no-noise baseline that
    launches no `memory_benchmark` process.
  - Figure 13 defaults to the paper PageRank thread-count sweep with clean and
    loaded cases.
  - CS-link villain throttling is explicitly enabled for local cSwitch
    variants with `--cs-villain-throttle true`.
  - default Figure 10-13 wall-time, storage, outputs, and expected qualitative
    trends are tabulated in `docs/EXPECTED_RESULTS.md`.
- Unusual behavior:
  - scheduler-backed runs create managed cgroups and require clean cgroup
    teardown.
  - `perf sched` is intentionally not part of the AE harness default path.
  - some runners intentionally retry failed points and preserve prior attempt
    directories for resume.
  - AE scripts do not invoke `eval/exp*`; add missing AE workloads directly to
    `ae/harness.py`.
  - throttle strength defaults are `--cs-villain-reslice-us 1000`,
    `--cs-villain-refill-divisor 4`, `--cs-villain-settle-ms 40`, and
    `--cs-villain-release-samples 2`.
  - AE runs use the checked-in mapping by default, so no profiler-tree mapping
    file is required.

## Reproduced

- Primary fresh-execution entry points:
  - `./reproduce.sh primary` runs all five targets.
  - `./reproduce.sh fig10`
  - `./reproduce.sh fig11`
  - `./reproduce.sh fig12a`
  - `./reproduce.sh fig12b`
  - `./reproduce.sh fig13`
- Internal single-experiment entry points:
  - `ae/run.sh fig10 full`
  - `ae/run.sh fig11 full`
  - `ae/run.sh fig12a full`
  - `ae/run.sh fig12b full`
  - `ae/run.sh fig13 full`
- Optional characterization:
  - `./reproduce.sh optional` verifies checksums and renders
    the frozen Figures 2-6 and 8 inputs.
  - supported current-host fresh runners use
    `./reproduce.sh <figure> optional` and are not required for AE.
  - Figure 4 and Figure 8d remain data+plot only because their required DIMM
    and NPS4 configurations are outside the evaluation scope. These commands
    regenerate the published plots; they do not rerun the experiments.
- Human-readable result conversion:
  - `ae/summarize.sh fig10`
  - `ae/summarize.sh fig11`
  - `ae/summarize.sh fig12a`
  - `ae/summarize.sh fig12b`
  - `ae/summarize.sh fig13`
- Figure rendering:
  - `ae/plot.sh` renders `figure10.pdf` through `figure13.pdf` plus PNGs under
    `ae/results/figures/`.
  - Figure 10/11 use cSwitch normalization for this AE result set; missing
    measurements remain blank and are listed in `missing_values.tsv`.
- Known scope notes to call out in the submission:
  - Figure 12 uses concrete traffic rates; paper-style percentage load labels
    require calibration. Positive rate-limiter values are ordered so traffic
    increases from left to right.
  - optional characterization fresh runs are long and some panels retain
    panel-specific aggregation steps; the frozen plot pipeline is the default
    optional workflow.
