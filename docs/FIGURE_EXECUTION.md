# Figure-by-Figure Execution

Run every command below from the frozen artifact release root. The
shell wrappers use AE-local sources and build the scheduler source at the same
repository root by default; do not invoke historical runners directly.

```sh
cd /path/to/ae
```

The wrappers invoke `sudo -n` where the benchmark requires root privileges.
Do not run the wrapper itself with `sudo`, because doing so can make all result
files root-owned.

## Optional Characterization: Figures 2-6 and 8

These figures use `motivation/run.sh` and its standalone
`motivation/harness.py`. For each supported optional fresh-run target in the
table, the internal execution pattern is:

```sh
motivation/run.sh <target> check
motivation/run.sh <target> dry-run
motivation/run.sh <target> full
```

| Paper figure | Target | Frozen experiment runner | Required host setup |
| --- | --- | --- | --- |
| 2a | `fig2a` | `motivation/original/ycsb/script_old/run_{amd,intel}_stall_sweep.sh` | Run once on the AMD host and once on the Intel host |
| 2b | `fig2b` | `motivation/original/ycsb/script_old/run_multi_instance_noise_thread_sweep.sh` | Run once on each paper host |
| 3a/3b | `fig3` | `motivation/original/ycsb/scripts/run_chiplet_ycsb_harness.py` | NPS1, DIMM=NUMA0, CXL=NUMA1 |
| 4a/4b | `fig4 plot` | `motivation/original/ycsb/scripts/run_chiplet_ycsb_harness.py` retained for provenance | Fresh physical DIMM sweep is outside AE scope |
| 5a/5b | `fig5` | `motivation/original/ycsb/scripts/run_smt14_numa_backend_sweep.py` | NPS1, DIMM=NUMA0, CXL=NUMA1 |
| 6a | `fig6a` | `motivation/original/ycsb/scripts/run_bw_ineq_cc_io.py` | NPS1, DIMM=NUMA0, CXL=NUMA1 |
| 6b | `fig6b` | `motivation/original/ycsb/scripts/run_bw_ineq_intra_io.py` | NPS1, DIMM=NUMA0, CXL=NUMA1 |
| 6c | `fig6c` | `motivation/original/ycsb/scripts/run_bw_ineq_io_dimm.py` | NPS1, DIMM=NUMA0, CXL=NUMA1 |
| 6d | `fig6d` | `motivation/original/ycsb/scripts/run_bw_ineq_io_cxl.py` | NPS1, DIMM=NUMA0, CXL=NUMA1 |
| 8a/8b | `fig8ab` | `motivation/original/exp3/exp1/run_exp1.py` | NPS1, DIMM=NUMA0, CXL=NUMA1 |
| 8c | `fig8c` | `motivation/original/exp3/exp4/run_exp4.py` | NPS1, DIMM=NUMA0, CXL=NUMA1 |
| 8d | `fig8d plot` | `motivation/original/exp3/exp_case3/run_exp_case3.py` retained for provenance | Fresh NPS4/two-DIMM run is outside AE scope |
| 8e | `fig8e` | `motivation/original/exp3/exp_case4-1/run_exp_case4_1.py` | NPS1, DIMM=NUMA0, CXL=NUMA1 |
| 8f graph | `fig8f` | `motivation/original/exp3/exp_case4-2/run_exp_case4_2.py` | NPS1, DIMM=NUMA0, CXL=NUMA1 |
| 8f BW table | `fig8f-bw` | `motivation/original/exp3/exp3/run_exp3.py` | NPS1, DIMM=NUMA0, CXL=NUMA1 |

### Figure 2

Execute both panels on both paper hosts. Normally the harness detects the CPU
vendor; the explicit override makes the intended result label unambiguous.

```sh
# AMD host
MOTIVATION_CPU_VENDOR=amd motivation/run.sh fig2a full
MOTIVATION_CPU_VENDOR=amd motivation/run.sh fig2b full

# Intel host
MOTIVATION_CPU_VENDOR=intel motivation/run.sh fig2a full
MOTIVATION_CPU_VENDOR=intel motivation/run.sh fig2b full
```

### Figure 3

```sh
motivation/run.sh fig3 check
motivation/run.sh fig3 full
```

The single target runs both the YCSB backend density sweep and DuckDB Q21.

### Figure 4

Fresh Figure 4 execution is outside the AE scope because each point
requires a physical DIMM population change. The original runner and result
provenance remain available for future use; evaluators regenerate the
published plot from the frozen paper input:

```sh
motivation/run.sh fig4 plot
```

### Figure 5

```sh
motivation/run.sh fig5 check
motivation/run.sh fig5 full
```

The runner measures both NUMA0 DIMM and NUMA1 CXL placements.

### Figure 6

Each panel is an independent bandwidth-inequality experiment.

```sh
motivation/run.sh fig6a full
motivation/run.sh fig6b full
motivation/run.sh fig6c full
motivation/run.sh fig6d full
```

### Figure 8

Run the in-scope panels under the normal NPS1 setup; no NPS4 reboot is required
for AE:

```sh
motivation/run.sh fig8ab full
motivation/run.sh fig8c full
motivation/run.sh fig8e full
motivation/run.sh fig8f full
motivation/run.sh fig8f-bw full
```

Fresh Figure 8d execution is outside the AE scope because it requires an
NPS4 reboot and a two-DIMM population. Verify that panel from its frozen paper
input together with the other preserved plots. This regenerates the published
plot and does not rerun the experiment:

```sh
motivation/run.sh fig8d plot
```

`motivation/run.sh fig8 full` and `motivation/run.sh all full` are
intentionally rejected; use the in-scope per-panel fresh-run commands above.

Render the preserved paper plot inputs with:

```sh
motivation/run.sh all plot
```

This plot command verifies the frozen paper inputs. Fresh raw motivation runs
still require the panel-specific aggregation steps described in
`motivation/REPRODUCIBILITY_GAPS.md` before replacing those inputs.

## Primary AE Targets: Figures 10-13

These figures use the top-level `reproduce.sh`, which dispatches to the
standalone `ae/harness.py` and then summarizes and plots successful runs. The
recommended AE setting requires one successful round and preserves up to three
attempt slots for failed points:

```sh
AE_REPEATS=1 AE_MAX_ATTEMPTS=3 ./reproduce.sh primary
```

The primary command launches cSwitch (`paper-greedy`) and the EEVDF, ARCAS,
and Caladan+ ports together. The core reproduction claim is interpreted from
the cSwitch results; port results provide contextual comparisons and do not
claim independent reproduction of their original systems.

| Paper figure | Target | Execution command | Default result directory |
| --- | --- | --- | --- |
| 10 | `fig10` | `AE_REPEATS=1 AE_MAX_ATTEMPTS=3 ./reproduce.sh fig10` | `ae/results/fig10/` |
| 11 | `fig11` | `AE_REPEATS=1 AE_MAX_ATTEMPTS=3 ./reproduce.sh fig11` | `ae/results/fig11/` |
| 12a | `fig12a` | `AE_REPEATS=1 AE_MAX_ATTEMPTS=3 ./reproduce.sh fig12a` | `ae/results/fig12a/` |
| 12b | `fig12b` | `AE_REPEATS=1 AE_MAX_ATTEMPTS=3 ./reproduce.sh fig12b` | `ae/results/fig12b/` |
| 13 | `fig13` | `AE_REPEATS=1 AE_MAX_ATTEMPTS=3 ./reproduce.sh fig13` | `ae/results/fig13/` |

Use the same target for command inspection or a short functional run:

```sh
./reproduce.sh fig10 dry-run
./reproduce.sh fig11 dry-run
./reproduce.sh fig12a dry-run
./reproduce.sh fig12b dry-run
./reproduce.sh fig13 dry-run

./reproduce.sh fig13 smoke
```

The default `full` action performs the summary and plot steps automatically.
For advanced manual recovery from an existing result tree, use:

```sh
ae/summarize.sh fig10
ae/summarize.sh fig11
ae/summarize.sh fig12a
ae/summarize.sh fig12b
ae/summarize.sh fig13
ae/plot.sh all
```

`ae/plot.sh` reads only the AE-local summaries and writes PDF/PNG files and
normalized plot data under `ae/results/figures/`.

To render an independently reproduced Figure 12 result tree without requiring
Figures 10, 11, and 13, run `ae/plot.sh fig12`.

## Output Isolation

Override output roots when testing a modified command without touching an
existing full run:

```sh
MOTIVATION_OUT_ROOT=/tmp/cswitch-motivation motivation/run.sh fig8c full
AE_OUT_ROOT=/tmp/ae-results ae/run.sh fig13 smoke
```

All normal outputs remain under `motivation/results/` or `ae/results/` and are
excluded from the frozen source commit.
