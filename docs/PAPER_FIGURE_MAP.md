# Paper Figure Map

This file maps `cSwitch_SOSP26-1.pdf` evaluation figures to repo-local
reproduction scripts.

The AE entry points use the standalone `ae/harness.py` runner. They do not
invoke the historical `eval/exp*` harnesses.

The primary AE workflow launches cSwitch (`paper-greedy`) and the artifact's
EEVDF, ARCAS, and Caladan+ ports together. The cSwitch results define the core
reproduction claim. Port results are comparison context rather than independent
reproductions of the original systems or their papers.

Figures 2-6 and 8 are optional characterization targets using the separately
frozen motivation harness; see `../motivation/FIGURE_MAP.md`. Fresh Figure 4
and Figure 8d runs are outside the AE scope because they require physical DIMM
changes and a different BIOS/DIMM topology, respectively. Their frozen inputs
remain available to regenerate the published plots without rerunning those
experiments.

## Figure 10: Overall Performance Matrix

- Paper caption: speedup of cSwitch, ARCAS, and Caladan+ over Linux EEVDF when
  the I/O chiplet is unloaded and loaded.
- AE entry point: `ae/run.sh fig10 full`.
- Underlying runner: `ae/harness.py`.
- Campaign-relative AE output: `<campaign>/fig10/`.
- Summary files: `results/raw_results.tsv`,
  `results/aggregate_results.tsv`, `results/results_matrix.csv`, and
  `results/summary.md`.
- Current standalone workload coverage: `ycsb_rocksdb_256mib`,
  `ycsb_orientdb_256mib`, `ycsb_elasticsearch_256mib_lvalue`,
  `gapbs_bc_kron20_twitter`, `gapbs_pr_kron20`,
  `llamacpp_llama31_8b`, `node_replication_skiplist_rw50`,
  `node_replication_rwlock_rw50`, `filebench_fileserver`,
  `filebench_webproxy`, `filebench_webserver`, and `filebench_varmail`.
- Variant mapping: cSwitch=`paper-greedy`, ARCAS=`arcas`,
  Caladan+=`nsdi-delay-range`, EEVDF=`eevdf`.
- Fig10 loaded defaults mirror the old load75 CPU split:
  workload CPUs `0-4,7-11,21-25,28-32`, noise CPUs
  `5-6,12-13,26-27,33-34`, and per-noise-thread rate `50`.
- Note: workload specs, Filebench templates, and the GAPBS/node-replication
  helper live under `ae/`; do not route AE reproduction through `eval/exp1`.
- AE plot source: `ae/plots/figure10.plt`, prepared by `ae/plot.py` and
  rendered by `ae/plot.sh`. The AE plot normalizes to cSwitch rather than
  EEVDF; unavailable measurements remain blank.

## Figure 11: Free/Busy Heterogeneous Core Selection

- Paper caption: cSwitch, ARCAS, EEVDF, and Caladan+ on llama.cpp, PageRank,
  and File Server under heterogeneous free/busy cores.
- AE entry point: `ae/run.sh fig11 full`.
- Underlying runner: `ae/harness.py`.
- Campaign-relative AE output: `<campaign>/fig11/`.
- Summary files: `results/raw_results.tsv`, `results/aggregate_results.tsv`,
  `results/summary.md`.
- Workload coverage: `llamacpp_llama31_8b`, `gapbs_pr_kron20`, and
  `filebench_fileserver`.
- Variant mapping: cSwitch=`paper-greedy`, ARCAS=`arcas`,
  Caladan+=`nsdi-delay-range`, EEVDF=`eevdf`.
- Default active workload CPUs: `0-13,21-34`; default workload threads: `3`.
- `free-cores` noise: `0-3`, `7-10`, `21-24`, `28-31` at per-chiplet rates
  `50`, `200`, `2000`, `10000`.
- `busy-cores` noise: `0-6`, `7-13`, `21-27`, `28-34` at the same
  per-chiplet rates.
- AE plot source: `ae/plots/figure11.plt`. The AE plot normalizes to cSwitch
  rather than EEVDF; unavailable measurements remain blank.

## Figure 12a: Compute-I/O Chiplet Link Load

- Paper caption: performance degradation while varying compute-I/O chiplet link
  traffic load.
- AE entry point: `ae/run.sh fig12a full`.
- Underlying runner: `ae/harness.py`.
- Campaign-relative AE output: `<campaign>/fig12a/`.
- Summary files: `results/raw_results.tsv`, `results/aggregate_results.tsv`,
  `results/summary.md`.
- Variant coverage: `paper-greedy`, `arcas`, `eevdf`, `nsdi-delay-range`.
- Workload/noise shape: single-thread `gapbs_pr_kron20` starts on `0-6`,
  widens to `0-13,21-34`, and launches five scheduler-managed noise workers
  on `0-4`.
- Rate sweep: no-noise sentinel `0`, then limiter values `50`, `100`, `500`,
  `1000`, `5000`, `10000`. Smaller positive values generate more traffic.
- Note: the AE harness summarizes timing degradation. It does not require
  `perf sched` for the default AE path.
- AE plot source: the left panel of `ae/plots/figure12.plt`. Each scheduler is
  normalized to its own true no-noise point, and positive limiter values are
  ordered so traffic increases from left to right.

## Figure 12b: Overall I/O Chiplet Load

- Paper caption: performance degradation while varying overall I/O chiplet
  traffic load.
- AE entry point: `ae/run.sh fig12b full`.
- Underlying runner: `ae/harness.py`.
- Campaign-relative AE output: `<campaign>/fig12b/`.
- Summary files: `results/raw_results.tsv`, `results/aggregate_results.tsv`,
  `results/summary.md`.
- Variant coverage: `paper-greedy`, `arcas`, `eevdf`, `nsdi-delay-range`.
- Workload/noise shape: four single-thread `gapbs_pr_kron20` instances start
  on `0-6`, `7-13`, `21-27`, and `28-34`, then widen to
  `0-13,21-34,42-55,63-76`.
- Managed rate-sweep noise runs on `0-4`, `7-11`, `21-25`, and `28-32`.
- Persistent external sidecar noise runs on `14-15,35-36,56-57,77-78` at rate
  `50`: two workers per excluded CCX, 8 total, for the 8-DIMM AE host.
- One round is summarized as the arithmetic mean of the four concurrent
  PageRank instance times. Multiple successful rounds are summarized by the
  median of their per-round means.
- Rate sweep: no-noise sentinel `0`, then limiter values `50`, `100`, `500`,
  `1000`, `5000`, `10000`. The zero point launches neither managed nor
  external noise; smaller positive values generate more traffic.
- Note: the harness records concrete `memory_benchmark` rates; converting the
  x-axis to paper-style percentage labels requires calibration data.
- AE plot source: the right panel of `ae/plots/figure12.plt`. The plot keeps
  the concrete limiter labels instead of inventing percentage calibration.

## Figure 13: PageRank Scaling

- Paper caption: PageRank performance while varying thread count, with and
  without chiplet load, normalized to the single-thread case.
- AE entry point: `ae/run.sh fig13 full`.
- Underlying runner: `ae/harness.py`.
- Campaign-relative AE output: `<campaign>/fig13/`.
- Summary files: `results/raw_results.tsv`, `results/aggregate_results.tsv`,
  `results/summary.md`.
- Variant coverage: `paper-greedy`, `arcas`, `eevdf`, `nsdi-delay-range`.
- Workload: `gapbs_pr_kron20` over `0-13,21-34,42-55,63-76`.
- Thread counts: `1`, `2`, `4`, `6`, `8`, `10`, `12`, `14`, `16`, `18`.
- Cases: `clean` and `loaded`; loaded sidecar traffic runs on
  `0-3,7-10,21-24,28-31,42,49,63,70`.
- AE plot source: `ae/plots/figure13.plt`. Each variant is normalized to its
  own single-thread result, matching the paper.
