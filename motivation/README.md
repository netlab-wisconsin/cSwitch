# Optional Characterization Reproduction

This directory freezes the original experiment sources, exact paper plot
inputs, and selected result provenance for cSwitch paper Figures 2-6 and 8.
It is independent of later edits to `~/ycsb`, `~/exp3`, and
`~/chipletos/gnuplot`.

These experiments are included for completeness and future reuse. They are
optional and are not required for the primary Figures 10-13 AE workflow.

## Entry Points

The recommended evaluator entry points are:

```sh
./reproduce.sh optional
./reproduce.sh fig3 optional
```

The commands below expose the internal harness for advanced use.

For a paper-panel index with the exact full-run command and required host
topology, see `../docs/FIGURE_EXECUTION.md`.

Inspect the exact command plan without running benchmarks:

```sh
motivation/run.sh all dry-run
motivation/run.sh fig8 dry-run
```

Render every preserved paper plot:

```sh
motivation/run.sh all plot
```

Preflight and run an in-scope experiment:

```sh
motivation/run.sh fig8e check
motivation/run.sh fig8e full
```

All generated configs, worktrees, raw outputs, and plots are kept under
`motivation/results/`, or under `MOTIVATION_OUT_ROOT` when set.

## Hardware Profiles

The supplied AMD EPYC 9634 evaluation host covers supported optional fresh
runs for Figures 3, 5, 6, 8a-c, 8e-f, and the AMD half of Figure 2. Figures 2a
and 2b must also be run on a separate GenuineIntel host to regenerate their
Intel comparison points. The complete preserved plots can still be rendered
without access to that Intel host.

Supported optional fresh runs use the paper's normal NPS1 layout:

- NUMA 0: all CPU cores and local DRAM.
- NUMA 1: CPU-less CXL.mem.

Fresh runs for Figure 4 and Figure 8d are outside the AE scope: Figure 4
requires physically changing the DIMM population, while Figure 8d requires an
NPS4 reboot with two populated DRAM domains. Their runners are retained for
provenance and future use. Evaluators regenerate the published plots from
frozen paper inputs; these commands do not rerun the experiments:

```sh
motivation/run.sh fig4 plot
motivation/run.sh fig8d plot
```

See `TOPOLOGY_AUDIT.md` before changing BIOS or DIMM settings.

## External Benchmark Inputs

The experiment and plotting source is frozen here. Large benchmark binaries,
models, databases, and graph inputs remain external. Their default locations
match the author machine and can be overridden with:

- `MOTIVATION_MEMORY_BENCHMARK`
- `MOTIVATION_YCSB_HOME`
- `MOTIVATION_TWITTER_GRAPH`
- `MOTIVATION_CPU_VENDOR=amd|intel`
- `MOTIVATION_USE_SUDO=0|1`
- `MOTIVATION_OUT_ROOT`

The source/result mapping and known manual aggregation gaps are documented in
`FIGURE_MAP.md` and `REPRODUCIBILITY_GAPS.md`.
