# cSwitch SOSP 2026 Artifact Evaluation

The top-level `README.md` and `reproduce.sh` are the evaluator entry point for
cSwitch. The frozen `sosp26-ae-v3` release keeps the artifact implementation
separate from the scheduler source directories and uses the standalone
`ae/harness.py`; historical `eval/exp*` runners are not invoked.

The scheduler source is at the repository root. Its paper base commit is
recorded in `ae/SOURCE_COMMIT`; see `docs/SOURCE_SNAPSHOT.md` for snapshot
verification and for using another checkout through `AE_SCHEDULER_ROOT`.

## Scope

The primary fresh-execution targets evaluated for the Reproduced badge are
Figures 10-13 of the cSwitch SOSP 2026 paper:

- Figure 10: overall clean/loaded performance matrix, backed by
  `ae/harness.py`.
- Figure 11: heterogeneous free/busy core selection, backed by
  `ae/harness.py`.
- Figure 12a: compute-I/O chiplet contention sweep, backed by `ae/harness.py`.
- Figure 12b: wider I/O chiplet contention sweep, backed by `ae/harness.py`.
- Figure 13: PageRank core scaling, backed by `ae/harness.py`.

Primary fresh campaigns launch cSwitch (`paper-greedy`) and the artifact's
EEVDF, ARCAS, and Caladan+ ports together. The core reproduction claim and its
interpretation concern cSwitch. Port results are contextual comparisons, not
independent reproductions of the original systems or their papers.
Bibliographic references for ARCAS and Caladan+ are listed in the
[top-level README](../README.md#references).

The characterization experiments for Figures 2-6 and 8 are included under
`motivation/` for completeness and future reuse, but are optional for AE. That
bundle has its own source snapshot, topology checks, provenance records, and
plotting entry point. Fresh Figure 4 and Figure 8d runs are outside the AE
scope because they require physical DIMM changes and an NPS4/two-DIMM reboot,
respectively; those panels use frozen plot inputs.

See [PAPER_FIGURE_MAP.md](./PAPER_FIGURE_MAP.md) for the detailed mapping.

## Figure-by-Figure Commands

The complete execution runbook is
[FIGURE_EXECUTION.md](./FIGURE_EXECUTION.md). It lists the frozen runner,
preflight command, full-run command, topology, and result path for every paper
panel covered by the artifact:

| Paper figure | Execution target |
| --- | --- |
| 2a | Optional: `./reproduce.sh fig2a optional` on both AMD and Intel hosts |
| 2b | Optional: `./reproduce.sh fig2b optional` on both AMD and Intel hosts |
| 3 | Optional: `./reproduce.sh fig3 optional` |
| 4 | Regenerate the published plot from frozen input; physical DIMM reruns are out of scope |
| 5 | Optional: `./reproduce.sh fig5 optional` |
| 6a | Optional: `./reproduce.sh fig6a optional` |
| 6b | Optional: `./reproduce.sh fig6b optional` |
| 6c | Optional: `./reproduce.sh fig6c optional` |
| 6d | Optional: `./reproduce.sh fig6d optional` |
| 8a/8b | Optional: `./reproduce.sh fig8ab optional` |
| 8c | Optional: `./reproduce.sh fig8c optional` |
| 8d | Regenerate the published plot from frozen input; NPS4/two-DIMM rerun is out of scope |
| 8e | Optional: `./reproduce.sh fig8e optional` |
| 8f | Optional: `./reproduce.sh fig8 optional` |
| 10 | `AE_REPEATS=1 AE_MAX_ATTEMPTS=3 ./reproduce.sh fig10` |
| 11 | `AE_REPEATS=1 AE_MAX_ATTEMPTS=3 ./reproduce.sh fig11` |
| 12a | `AE_REPEATS=1 AE_MAX_ATTEMPTS=3 ./reproduce.sh fig12a` |
| 12b | `AE_REPEATS=1 AE_MAX_ATTEMPTS=3 ./reproduce.sh fig12b` |
| 13 | `AE_REPEATS=1 AE_MAX_ATTEMPTS=3 ./reproduce.sh fig13` |

## Quick Start

Start from the exact release and run the non-root preflight first:

```sh
git describe --tags --exact-match
./reproduce.sh check
```

Exercise runnable paths without starting schedulers:

```sh
ae/run.sh fig10 dry-run
ae/run.sh fig13 dry-run
ae/run.sh fig12a dry-run
```

The top-level equivalent for the complete primary plan is
`./reproduce.sh primary dry-run`. It does not acquire the campaign lock or
start a scheduler.

Run a small scheduler smoke for one figure after confirming the host has
passwordless `sudo -n`, `sched_ext`, and the benchmark inputs:

```sh
ae/run.sh fig13 smoke
```

Refresh human-readable summaries from saved outputs:

```sh
ae/summarize.sh all
```

Render paper-style Figures 10-13 from those summaries:

```sh
ae/plot.sh all
ae/plot.sh fig12
```

The rendered PDF/PNG files, normalized plot inputs, and an exact list of blank
values are written under the selected campaign's `figures/` directory by
default. For top-level fresh runs, `ae/results/latest` points to that campaign.

Inspect or render the motivation figures separately:

```sh
motivation/run.sh all dry-run
motivation/run.sh all plot
```

See `motivation/README.md` and `motivation/TOPOLOGY_AUDIT.md` before running
their full experiments.

## Plotting And Normalization

`ae/plot.sh` uses the AE-local `ae/plot.py` data preparation step and gnuplot
sources under `ae/plots/`; it does not read the historical `eval/exp*` trees or
the author workspace under `/home/seunghyun/chipletos`.

- Figures 10 and 11 normalize each workload/panel to `paper-greedy` (cSwitch),
  so the available cSwitch bar is `1.0`. Legacy result trees fall back to
  `la-default` until those figures are rerun. This intentionally differs from
  the paper's EEVDF baseline because the current AE run has many missing EEVDF
  points.
- Figure 12 normalizes each scheduler to its true no-noise point and reports
  that point as `100%`, preserving the paper's performance-retention
  comparison. The AE harness represents no noise with a `rate=0` sentinel but
  does not launch `memory_benchmark` for that point, because the benchmark
  itself interprets rate zero as maximum traffic. Positive limiter values are
  ordered from `10000` down to `50`, so traffic increases from left to right.
- Figure 13 normalizes each scheduler to its own single-thread result, matching
  the paper caption.
- Missing measurements and any Figure 10/11 point without a cSwitch baseline
  remain `NaN`; gnuplot leaves those bars or line points blank rather than
  interpolating them.

Use `AE_PLOT_OUT_ROOT` to choose another output directory. `gnuplot` and
`pdftoppm` are required in addition to Python 3.

## Full Runs

Full runs are long, root-level scheduler experiments. The wrapper commands are:

```sh
ae/run.sh fig10 full
ae/run.sh fig11 full
ae/run.sh fig12a full
ae/run.sh fig12b full
ae/run.sh fig13 full
```

Use the top-level wrapper for evaluator runs so the machine-wide lock,
preflight, summary, and plotting stages are applied. Expected wall times,
storage, and qualitative outcomes are listed in
[EXPECTED_RESULTS.md](./EXPECTED_RESULTS.md).

By default, fresh AE outputs go under `ae/results/`. Historical runners and
their result archives are not included in the evaluation branch; the AE
wrappers do not read or summarize them.

The default AE mode keeps the paper figure axes intact while making the runs
lighter to operate:

- `AE_LIGHT=1`: keep the paper workload, variant, core, and noise-rate axes;
  use one repeat by default.
- `AE_LIGHT_BUILD=1`: build local cSwitch variants with
  `--no-default-features --features tick-resched,...`, removing diagnostics
  CLI/log code from the AE binaries.
- `AE_CS_VILLAIN_THROTTLE=true`: run cSwitch local variants with CS-link
  villain throttling explicitly enabled.
- `AE_SCHEDULER_LOGS=0`: do not pass `--decision-log-path`,
  `--runtime-log-path`, or `--monitor`.
- `AE_KEEP_HEAVY_WORKLOAD_ARTIFACTS=0`: after each run records its status and
  summary inputs, prune regenerated benchmark database/workload directories
  under `harness_results/*/runs/*/workload` to keep full runs bounded in disk
  usage. When `AE_USE_SUDO=1`, root-owned workload files are removed through a
  `sudo -n rm -rf` fallback.
- No AE command uses `perf sched` by default.

For slower diagnostic runs, set `AE_LIGHT_BUILD=0`, increase `AE_REPEATS`, and
pass explicit `AE_VARIANTS`, `AE_CORES`, `AE_NOISE_RATES`, or `AE_BENCHMARKS`
as needed.

Figure 10 defaults to four variants (`paper-greedy`, `arcas`, `eevdf`,
`nsdi-delay-range`), two cases (`clean`, `loaded`), and these workloads:

- YCSB: `ycsb_rocksdb_256mib`, `ycsb_orientdb_256mib`,
  `ycsb_elasticsearch_256mib_lvalue`.
- GAPBS: `gapbs_bc_kron20_twitter`, `gapbs_pr_kron20`.
- llama.cpp: `llamacpp_llama31_8b`.
- node-replication: `node_replication_skiplist_rw50`,
  `node_replication_rwlock_rw50`.
- Filebench: `filebench_fileserver`, `filebench_webproxy`,
  `filebench_webserver`, `filebench_varmail`.

The Fig10 `loaded` case uses workload CPUs `0-4,7-11,21-25,28-32`, sidecar
noise CPUs `5-6,12-13,26-27,33-34`, and `AE_FIG10_NOISE_RATE=50` by default.

Figure 11 defaults to the paper workload set (`llamacpp_llama31_8b`,
`gapbs_pr_kron20`, and `filebench_fileserver`), four variants
(`paper-greedy`, `arcas`, `eevdf`, `nsdi-delay-range`), and two cases
(`free-cores`, `busy-cores`). Workloads run over `0-13,21-34` with
`AE_FIG11_THREADS=3`. The free case uses four noise threads per active chiplet
on `0-3`, `7-10`, `21-24`, and `28-31`; the busy case uses all seven cores per
active chiplet on `0-6`, `7-13`, `21-27`, and `28-34`. Per-chiplet rates are
`50`, `200`, `2000`, and `10000`.

Figure 12a defaults to single-thread PageRank. The workload starts on `0-6`,
widens to `0-13,21-34`, and then managed `memory_benchmark` traffic runs on
`0-4` (five workers) while sweeping `AE_NOISE_RATES` (default
`0,50,100,500,1000,5000,10000`). The zero point is a true no-noise baseline;
positive values are NOP rate-limiters, so smaller values generate more traffic.

Figure 12b defaults to four single-thread PageRank instances starting on
`0-6`, `7-13`, `21-27`, and `28-34`, then widening to
`0-13,21-34,42-55,63-76`. It also launches scheduler-managed noise on
`0-4`, `7-11`, `21-25`, and `28-32` for the rate sweep, plus a persistent
external sidecar on `14-15,35-36,56-57,77-78` at rate `50`. The external
sidecar uses two workers per excluded CCX (8 total) for this 8-DIMM host.
Neither managed nor external noise is launched for the zero baseline.
The metric for one round is the arithmetic mean of the four PageRank instance
times. When `AE_REPEATS` is greater than one, summaries then take the median
of those per-round means.

Figure 13 defaults to PageRank over `0-13,21-34,42-55,63-76`, variants
`paper-greedy`, `arcas`, `eevdf`, and `nsdi-delay-range`, cases `clean` and
`loaded`, and thread counts `1,2,4,6,8,10,12,14,16,18`. The loaded case uses
asymmetric sidecar traffic on `0-3,7-10,21-24,28-31,42,49,63,70`.

## Common Environment Variables

- `AE_SCHEDULER_ROOT`: scheduler source checkout, default the evaluation
  repository root.
- `AE_GAPBS_ROOT`: GAPBS checkout, default
  `/home/seunghyun/gapbs/gapbs`.
- `AE_GAPBS_GRAPH_ROOT`: GAPBS graph directory, default
  `$AE_GAPBS_ROOT/benchmark/graphs`.
- `AE_YCSB_ROOT`: YCSB AE workspace containing scripts, workloads, and
  benchmark adapters, default `/home/seunghyun/ycsb`.
- `AE_YCSB_RUNNER`: YCSB harness entry point, default
  `$AE_YCSB_ROOT/scripts/run_chiplet_ycsb_harness.py`.
- `AE_EEVDF_ROOT`: EEVDF scheduler checkout, default
  `/home/seunghyun/scx_rustland_eevdf`.
- `AE_EXP3_ROOT`: exp3 workspace, default `/home/seunghyun/exp3`.
- `AE_NODE_REPLICATION_ROOT`: node-replication Rust package, default
  `$AE_EXP3_ROOT/node-replication/node-replication`.
- `AE_LLAMA_ROOT`: llama.cpp benchmark directory, default
  `$AE_YCSB_ROOT/benchmarks/llama.cpp`.
- `AE_LLAMA_MODEL`: llama.cpp GGUF model, default
  `/home/seunghyun/llama.cpp/models/Meta-Llama-3.1-8B-Instruct-Q4_K_M.gguf`.
- `AE_MEMORY_BENCHMARK`: memory-noise binary. When unset, the harness checks
  `$AE_YCSB_ROOT/memory_benchmark` and then
  `/home/seunghyun/sched_bench/build/memory_benchmark`.
- `AE_ALLOW_SOURCE_MISMATCH`: set to `1` only for intentional development runs
  whose scheduler-owned files differ from the `ae/SOURCE_COMMIT` snapshot.
- `AE_OUT_ROOT`: output root, default `ae/results`.
- `AE_BUILD_SCRATCH_ROOT`: temporary Cargo target root, default
  `/dev/shm/scx-ae-build-<uid>`. The UID suffix isolates evaluator accounts
  from stale build files owned by another user.
- `AE_MACHINE_LOCK`: machine-wide campaign lock file, default
  `/run/lock/cswitch-ae.lock`. The administrator creates this root-owned,
  world-readable file once; evaluator scripts never modify its contents.
- `AE_LOCK_WAIT_SECONDS`: bounded wait for the machine lock, default `0`
  (fail immediately when another campaign is active).
- `AE_CCM_MAPPING_PATH`: scheduler mapping file, default
  `ae/ccm_mapping.txt`.
- `AE_LIGHT`: keep paper axes with lightweight repeat defaults, default `1`.
- `AE_LIGHT_BUILD`: set to `0` to build default diagnostic-enabled binaries,
  default `1`.
- `AE_SCHEDULER_LOGS`: set to `1` to opt back into scheduler
  decision/runtime logs, default `0`.
- `AE_CS_VILLAIN_THROTTLE`: pass `true` or `false` to
  `--cs-villain-throttle`, default `true`.
- `AE_CS_VILLAIN_RESLICE_US`: short slice granted to a selected CS villain,
  default `1000`.
- `AE_CS_VILLAIN_REFILL_DIVISOR`: token refill divisor, default `4`.
- `AE_CS_VILLAIN_SETTLE_MS`: per-link villain latch hold window, default `40`.
- `AE_CS_VILLAIN_RELEASE_SAMPLES`: capacity-cleared samples before latch
  release, default `2`.
- `AE_TICK_REEVAL_EVERY`: BPF tick reevaluation cadence, default `1`.
- `AE_TICK_DEFER_MAX`: token bucket capacity in reslice units, default `1`.
- `AE_REPEATS`: successful repeats required for full runs, default `1`.
- `AE_MAX_ATTEMPTS`: max attempts per point, default `1`.
- `AE_PROFILE`: scheduler build profile, default `release`.
- `AE_USE_SUDO`: set to `0` to pass `--no-sudo` where supported.
- `AE_SKIP_BUILD`: set to `1` to reuse existing AE-built binaries. Without it,
  the harness rebuilds so scheduler or mapping-parser changes are not masked by
  a stale binary.
- `AE_KEEP_HEAVY_WORKLOAD_ARTIFACTS`: set to `1` to preserve large
  regenerated benchmark database/workload directories under `harness_results`;
  default `0`. With sudo-enabled AE runs, pruning can remove root-owned
  workload directories while preserving logs, summary TSVs, configs, and
  status files.
- `AE_VARIANTS`: space/comma separated variants, for example
  `paper-greedy arcas eevdf nsdi-delay-range`. The aliases `cswitch` and `la`
  both select `paper-greedy`; use the explicit `la-default` key only for
  legacy-policy diagnostics.
- `AE_CORES`: Figure 13 core list.
- `AE_NOISE_RATES`: Figure 12 noise-rate list; `0` is the AE no-noise
  sentinel, while positive values are `memory_benchmark` NOP rate-limiters.
- `AE_PAPER_NOISE_DURATION_SEC`: long-running paper sidecar duration, default
  `86400`; wrappers stop sidecars when the workload exits.
- `AE_FIG12_NOISE_DURATION_SEC`: Fig12 managed-noise duration, default `3600`.
- `AE_FIG12_SCOPE_WIDEN_DELAY_SEC`: delay before Fig12 affinity widening,
  default `0`.
- `AE_FIG10_LOADED_WORKLOAD_CPUS`: Fig10 loaded workload CPU mask, default
  `0-4,7-11,21-25,28-32`.
- `AE_FIG10_NOISE_CPUS`: Fig10 loaded sidecar CPU mask, default
  `5-6,12-13,26-27,33-34`.
- `AE_FIG10_NOISE_RATE`: Fig10 loaded per-noise-thread rate, default `50`.
- `AE_FIG10_NOISE_DURATION_SEC`: Fig10 sidecar config duration, default
  `86400`; the runner stops it when the workload exits.

Standalone workload specs live in `WORKLOADS` in `ae/harness.py`; the
GAPBS/node-replication external helper is `ae/external_workload.py`, and
AE-local Filebench templates are under `ae/templates/`. Do not route AE
workload coverage through `eval/exp*`.

Full and smoke runs treat `r01`, `r02`, ... as attempt slots. A logical point
stops once it reaches `AE_REPEATS` successful runs with a finite primary metric
greater than zero, up to `AE_MAX_ATTEMPTS` slots. Failed attempt directories
and their `status.json` files are preserved
for inspection/resume; rerun with `--force` only when intentionally replacing
old attempt artifacts. If a workload metric was already parsed but wrapper
cleanup exits non-zero, the attempt is marked `salvaged` and counts as a
successful repeat. `dry-run` emits one plan slot per point and does not clobber
existing run status files. By default, the harness preserves logs, configs,
summary TSVs, and `status.json`, but prunes regenerated workload database
directories after recording the status so full AE runs do not fill the root
filesystem. For sudo runs this pruning uses `sudo -n` when required by
root-owned benchmark outputs.

Pressing `Ctrl-C` terminates the active scheduler/workload process group and
releases the top-level wrapper's machine lock. When `AE_OUT_ROOT` is unset,
each top-level full or smoke command creates a timestamped directory below
`ae/results/campaigns/` and updates `ae/results/latest`. This prevents a second
evaluator from silently reusing another evaluator's measurements. To resume,
rerun the printed command with the same explicit `AE_OUT_ROOT`; successful
points are then skipped and failed or unfinished points continue in later
attempt slots.

Public summary tables and plot-data files use the display labels `cSwitch`,
`ARCAS`, `EEVDF`, and `Caladan+`. Internal variant keys such as
`paper-greedy` and `nsdi-delay-range` remain only in run-directory names,
status/config provenance, and command-line interfaces so existing campaigns
can be resumed safely.

## CS-Link Throttle Defaults

The scheduler CLI default for `--cs-villain-throttle` is already `true`; the AE
harness passes it explicitly for local cSwitch variants. The effective AE
defaults are:

| knob | AE value | meaning |
| :--- | -------: | :------ |
| `--cs-villain-throttle` | `true` | enable per-link latch plus token pressure |
| `--cs-villain-reslice-us` | `1000` | short slice for a selected villain |
| `--cs-villain-refill-divisor` | `4` | refill one quarter of elapsed time as tokens |
| `--cs-villain-settle-ms` | `40` | hold the selected link villain during DF settle |
| `--cs-villain-release-samples` | `2` | release after two capacity-cleared DF-CS samples |
| `--tick-reeval-every` | `1` | reevaluate every scheduler tick |
| `--tick-defer-max` | `1` | token capacity in reslice-slice units |

CS links are sampled as `CS0..CS11`. The AE-local mapping file keeps mapped CCM
placement capacity at `20000 MiB/s` and sets every CS throttle capacity to
`40000 MiB/s`, so AE runs do not need a profiler-tree mapping file.

## AE Checklist

The SOSP 2026 artifact checklist pasted into the task is summarized in
[SOSP26_AE_CHECKLIST.md](./SOSP26_AE_CHECKLIST.md). The short version:

- Functional path: `ae/check.sh`, `ae/run.sh fig13 dry-run`.
- Minimal working example: `ae/run.sh fig13 smoke`.
- Reproduction entry points: `ae/run.sh <figure> full`.
- Summary/plot entry points: `ae/summarize.sh <figure|all>`.

See [EXTERNAL_DEPENDENCIES.md](./EXTERNAL_DEPENDENCIES.md) for pinned workload
sources and [EXPECTED_RESULTS.md](./EXPECTED_RESULTS.md) for resource and
outcome expectations.
