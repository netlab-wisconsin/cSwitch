# cSwitch Artifact Evaluation

This repository contains the scheduler source and artifact for *Scheduling
Linux Threads under I/O Chiplet Wall Using cSwitch* (SOSP 2026). Evaluators
only need this README and the top-level `reproduce.sh` entry point. Run every
command below from the repository root.

The `main` branch contains scheduler source only. The `artifact-evaluation`
branch adds the evaluation harness, frozen motivation inputs, and plotting
code. The frozen AEC release is `sosp26-ae-v1`; `./reproduce.sh check` verifies
that the checkout matches this tag and the pinned scheduler source.

For a public checkout, clone the release directly into a directory named
`ae`. Do not switch to a mutable branch afterward:

```sh
git clone --branch sosp26-ae-v1 --depth 1 \
  https://github.com/netlab-wisconsin/cSwitch.git ae
cd ae
```

## Quick Start

```sh
git rev-parse HEAD
./reproduce.sh check
./reproduce.sh primary dry-run
./reproduce.sh fig10 smoke
./reproduce.sh optional
```

The dry-run prints the complete plan without starting a scheduler. The Figure
10 smoke runs one short scheduler point and therefore acquires the machine-wide
campaign lock. `optional` checks the characterization bundle and regenerates
Figures 2-6 and 8 from frozen plot inputs; it does not launch those full
characterization experiments.

## Full Reproduction

The following command launches every primary campaign across cSwitch and the
three comparison ports. It can take approximately 5-9 hours on the supplied
host and must not run concurrently with another performance experiment:

```sh
./reproduce.sh primary
```

`AE_MAX_ATTEMPTS=3` preserves up to three attempt slots for a failed point
while requiring one successful round. See
[`docs/EXPECTED_RESULTS.md`](docs/EXPECTED_RESULTS.md) for per-figure runtime,
storage, outputs, and qualitative success criteria.

## Primary AE Targets

Figures 10-13 evaluate the main end-to-end claims of cSwitch. These are the
primary fresh-execution targets evaluated for the Reproduced badge:

```sh
./reproduce.sh fig10
./reproduce.sh fig11
./reproduce.sh fig12a
./reproduce.sh fig12b
./reproduce.sh fig13
```

`all` is an alias for `primary`. `./reproduce.sh fig12` combines both Figure 12
panels. Omitting the action on a primary target defaults to `full`; specify
`dry-run`, `smoke`, or `plot` when needed.

| Command | Result |
| --- | --- |
| `./reproduce.sh check` | Validate software, hardware interfaces, external workloads, and source pin |
| `./reproduce.sh primary` | Run and plot the four-variant Figures 10-13 campaign |
| `./reproduce.sh optional` | Validate and plot the frozen optional characterization bundle |
| `./reproduce.sh fig10` | Run, summarize, and plot Figure 10 |
| `./reproduce.sh fig11` | Run, summarize, and plot Figure 11 |
| `./reproduce.sh fig12` | Run both Figure 12 panels, summarize, and plot them together |
| `./reproduce.sh fig13` | Run, summarize, and plot Figure 13 |
| `./reproduce.sh <figure> dry-run` | Print the exact experiment plan without running it |
| `./reproduce.sh <figure> smoke` | Run the figure's short functional subset without plotting |
| `./reproduce.sh <figure> plot` | Rebuild a graph from existing result files |

The primary figures are written to:

```text
ae/results/figures/figure10.pdf
ae/results/figures/figure11.pdf
ae/results/figures/figure12.pdf
ae/results/figures/figure13.pdf
```

PNG versions and normalized plot data are stored in the same directory.
Motivation plots are written under `motivation/results/figures/`. All generated
results are ignored by Git.

Primary runs launch `paper-greedy` (cSwitch), EEVDF, [ARCAS][arcas-paper], and
the [Caladan+][caladan-plus-paper] port in the same campaign. The evaluation
interpretation and badge claim focus on cSwitch. The three port series provide
comparison context; they are not claims of independently reproducing the
original EEVDF, ARCAS, or Caladan+ systems or their papers. Unavailable
measurements remain blank.

## Optional Characterization

Scripts and data for Figures 2-6 and 8 are included for completeness and
future reuse. These characterization experiments are optional and are not
required for the recommended artifact-evaluation workflow.

Run the quick characterization smoke to verify the preserved-data checksums
and render all frozen paper inputs:

```sh
./reproduce.sh optional
```

Inspect or launch a supported fresh characterization runner with:

```sh
./reproduce.sh fig2 dry-run
./reproduce.sh fig2 optional
./reproduce.sh fig3 optional
```

| Figures | Optional support | Cost or limitation |
| --- | --- | --- |
| 2a/2b | Fresh runner plus frozen-data plot | Long vendor-specific sweeps; the supplied host regenerates only AMD points |
| 3 | Fresh runner plus frozen-data plot | Long YCSB and DuckDB density sweeps |
| 4 | Raw data and plot only | Fresh run excluded because it requires physical DIMM changes |
| 5 | Fresh runner plus frozen-data plot | Long NUMA backend sweep |
| 6a-6d | Fresh runners plus frozen-data plots | Long bandwidth-inequality sweeps |
| 8a-c/8e-f | Fresh runners plus frozen-data plots | Long workload-specific experiments on NPS1 |
| 8d | Raw data and plot only | Fresh run excluded because it requires NPS4 and two DIMMs |

Optional fresh outputs are stored under `motivation/results/`. Several
characterization panels retain panel-specific aggregation steps, so the graph
rendered after an optional run is explicitly the preserved reference plot, not
a relabelled fresh result. See
[`motivation/REPRODUCIBILITY_GAPS.md`](motivation/REPRODUCIBILITY_GAPS.md).

The reusable bundle is organized as `motivation/original/` for frozen runners,
`motivation/reference/` for raw and intermediate data, `motivation/plots/` for
processing/plot inputs, and `motivation/results/` for newly generated outputs.

## Evaluation Scope

Fresh Figures 10-13 runs target the supplied AMD EPYC 9634 evaluation machine
with CXL.mem, the artifact's `sched_ext` kernel, cgroup v2, MSR access, and
passwordless `sudo -n`.

- Primary campaigns may execute this artifact's EEVDF, ARCAS, and Caladan+
  ports alongside cSwitch. Interpretation of those port results is outside the
  core reproduction claim: they are contextual comparisons, not independent
  validation of the original systems or their papers.
- Figures 2a and 2b contain AMD and Intel measurements. Fresh Intel points
  require a separate GenuineIntel host; the complete frozen plots can be
  rendered on the supplied AMD host.
- Fresh Figure 4 reproduction is outside the AE scope because it requires
  physical DIMM population changes.
- Fresh Figure 8d reproduction is outside the AE scope because it requires an
  NPS4 reboot and a two-DIMM population.
- For Figure 4 and Figure 8d, the published plots can be regenerated from the
  frozen paper inputs through `./reproduce.sh optional` or their individual
  `plot` action. This is plot regeneration, not a fresh experiment.

Detailed panel-by-panel scope and topology are recorded in
[`docs/FIGURE_EXECUTION.md`](docs/FIGURE_EXECUTION.md).

## References

- **cSwitch paper:** *Scheduling Linux Threads under I/O Chiplet Wall Using
  cSwitch*, SOSP 2026 submission. The manuscript has no public DOI at artifact
  preparation time, and the submitted PDF is intentionally not duplicated in
  this repository. The title here is the canonical paper identifier for the
  release.

- **[ARCAS][arcas-paper]** (published as **CHARM**): Alessandro Fogli, Bo Zhao,
  Peter Pietzuch, and Jana Giceva. "CHARM: Chiplet Heterogeneity-Aware Runtime
  Mapping System."
  *Proceedings of the 21st European Conference on Computer Systems
  (EuroSys '26)*, 2026.
- **[Caladan+][caladan-plus-paper]**: Sarah McClure, Amy Ousterhout, Scott
  Shenker, and Sylvia Ratnasamy. "Efficient Scheduling Policies for
  Microsecond-Scale Tasks." *19th USENIX Symposium on Networked Systems Design
  and Implementation (NSDI '22)*, pages 1-18, 2022.

[arcas-paper]: https://dl.acm.org/doi/abs/10.1145/3767295.3769390
[caladan-plus-paper]: https://www.usenix.org/conference/nsdi22/presentation/mcclure

## AEC Evaluation Host

Full fresh-execution campaigns require the author-provided AMD EPYC 9634
evaluation host. Hostname, username, SSH port, and account lifetime are
distributed privately through the AEC/HotCRP channel. Evaluators should send a
comment-free SSH public key through that anonymous channel rather than directly
revealing personal contact information to the authors. The host uses normal
SSH and system accounting logs but contains no artifact analytics or tracking.

After logging in, run:

```sh
cd ~/ae
git describe --tags --exact-match
./reproduce.sh check
./reproduce.sh primary dry-run
./reproduce.sh fig10 smoke
```

The checkout is already pinned to the artifact release. Do not reboot the host,
change BIOS/NPS settings, alter DIMM population, or start another benchmark
while a campaign is running. Do not store personal data, private keys, tokens,
or unrelated credentials on the evaluation host. The private handoff checklist
is provided in
[`docs/AEC_PRIVATE_ACCESS_TEMPLATE.md`](docs/AEC_PRIVATE_ACCESS_TEMPLATE.md).

## External Inputs

Large benchmarks, models, databases, and graph inputs remain installed on the
evaluation machine. `./reproduce.sh check` verifies them before a real run.
The author-machine locations are defaults and can be overridden, for example:

```sh
AE_GAPBS_ROOT=/opt/gapbs \
AE_YCSB_ROOT=/opt/cswitch-ycsb \
AE_MEMORY_BENCHMARK=/opt/bin/memory_benchmark \
./reproduce.sh fig12 dry-run
```

The complete environment-variable list is in
[`docs/AE_REFERENCE.md`](docs/AE_REFERENCE.md#common-environment-variables).
Exact upstream URLs, commits, author-local patches, model checksums, and build
instructions are documented in
[`docs/EXTERNAL_DEPENDENCIES.md`](docs/EXTERNAL_DEPENDENCIES.md).

## Results And Resume

Fresh primary outputs are written only under `ae/results/` and are ignored by
Git; frozen characterization inputs remain under `motivation/reference/`.
Successful points are skipped when the same command is rerun, while failed
attempt directories and logs are retained and the next attempt slot is used.
For an entirely independent campaign, select a new output root:

```sh
AE_OUT_ROOT="$HOME/ae-results/fresh-$(date -u +%Y%m%dT%H%M%SZ)" \
  ./reproduce.sh primary
```

Pressing `Ctrl-C` terminates the active scheduler/workload process group and
releases the machine lock. Rerun the same command to resume completed points.
Use `--force` only through the internal harness when deliberately replacing
saved attempts; details are in
[`docs/AE_REFERENCE.md`](docs/AE_REFERENCE.md).

## Machine Access And Concurrency

Each evaluator should use a separate checkout, normally `~/ae`. The wrapper
uses `umask 002`; it never makes the checkout world-writable. Experiment runs
acquire `/run/lock/cswitch-ae.lock` with `flock` and fail clearly when another
campaign owns the machine. Set `AE_LOCK_WAIT_SECONDS` to a bounded number of
seconds to wait instead.

An administrator prepares the root-owned lock once:

```sh
sudo install -o root -g root -m 0644 /dev/null /run/lock/cswitch-ae.lock
```

If a central checkout is unavoidable, use a dedicated trusted Unix group and
group-writable result directories; never use `chmod -R a+rwX`. Source and
scripts executed through `sudo` should remain non-world-writable.

## Licensing

The cSwitch scheduler, author-written AE code, and author-generated measurement
data are distributed under GPL-2.0-only unless a file states otherwise. Bundled
or externally installed third-party components retain their upstream licenses
and are not relicensed by this artifact. See [LICENSE](LICENSE),
[LICENSES.md](LICENSES.md), and
[`motivation/LICENSE.md`](motivation/LICENSE.md).

## Repository Layout

| Path | Purpose |
| --- | --- |
| `reproduce.sh` | Single evaluator entry point for checks, runs, summaries, and plots |
| `src/`, `vendor/`, `Cargo.toml` | Frozen cSwitch scheduler source |
| `ae/` | Internal Figures 10-13 harness, configuration, and plotting implementation |
| `motivation/` | Frozen Figures 2-6 and 8 sources and paper plot inputs |
| `third_party/patches/` | Patches against pinned external workload revisions |
| `docs/` | Detailed runbook, dependencies, expected results, source pin, and packaging notes |
| `LICENSES.md` | Project and third-party licensing boundaries |
| `SCHEDULER.md` | Standalone scheduler build and runtime reference |

The paper PDF, historical result trees, and build outputs are intentionally not
distributed. The scheduler source pin is documented in
[`docs/SOURCE_SNAPSHOT.md`](docs/SOURCE_SNAPSHOT.md).
