# Active Python Scripts

This directory is the supported Python-only surface for the current experiment workflow.

Entry points:
- `run_chiplet_ycsb_harness.py`
- `run_chiplet_fixed_workload_memnoise_sweep.py`
- `run_single_chiplet_smt_numa_noise_sweep.py`
- `run_smt14_numa_backend_sweep.py`
- `run_smt14_numa_backend_schedrange_sweep.py`
- `run_bw_ineq_cc_io.py`
- `run_bw_ineq_intra_io.py`
- `run_bw_ineq_io_dimm.py`
- `run_bw_ineq_io_cxl.py`
- `auto_merge_results.py`
- `merge_fixed_workload_noise_group_summary.py`
- `merge_single_chiplet_smt_numa_group_summary.py`
- `merge_smt14_numa_group_summary.py`
- `merge_bw_ineq_group_summary.py`
- `archive_outdated_results.py`
- `export_group_summary_compat.py`
- `export_group_summary_per_query_compat.py`

Shared modules:
- `active_configs.py`
  Central catalog of the curated active configs and merge patterns.
- `experiment_utils.py`
  Shared runner helpers for temporary config generation, memory benchmark orchestration,
  result discovery, SMT discovery, and external noise artifact persistence.
- `run_bw_ineq_memload_sweep.py`
  Shared runner for the `bw-ineq-*` experiments. It launches one workload instance from
  each selected backend config and scales external `memory_benchmark` processes under a
  configurable shared CPU set, with one XML/log directory per noise process.
- `merge_utils.py`
  Shared result-directory parsing and union-header merge helpers.

Validation baseline:
- `python3 -m py_compile scripts/*.py`
- Active configs load and validate with `validate_config(..., enforce_runtime_requirements=False)`.

External backend notes:
- `duckdb_tpch`, `npb_*`, `llamacpp`, `gapbs_pagerank`, `gapbs_bfs`, `xsbench`, and `filebench_fileserver`
  are handled as external backends.
- `llamacpp` uses `llama.cpp/tools/llama-bench` and defaults to `--numa numactl`
  so it respects the harness NUMA binding policy.
- `gapbs_pagerank` uses `/home/seunghyun/gapbs/gapbs/pr` and honors
  `backend.options.graph` via `-f <graph>`.
- `gapbs_bfs` uses `/home/seunghyun/gapbs/gapbs/bfs` and also honors
  `backend.options.graph` via `-f <graph>`.
- `filebench_fileserver` renders a per-instance `fileserver.f` workload with a
  unique data path under the instance result directory.
- `xsbench` uses the CPU OpenMP implementation under
  `benchmarks/XSBench/openmp-threading`.

Current `bw-ineq-*` presets:
- `run_bw_ineq_cc_io.py`
  Workload CPU set `0-6,84-90`, noise CPU set `0-6,84-90`, NUMA 0, mode 0, rate 200,
  default noise counts `1,4,7,10,13`.
- `run_bw_ineq_intra_io.py`
  Workload CPU `0`, noise CPU set `7-83,91-167`, NUMA 0, mode 0, rate 100,
  default noise counts `11,22,33,44,55,66,77`.
- `run_bw_ineq_io_dimm.py`
  Same topology as `run_bw_ineq_intra_io.py`, separate prefix for later parameter edits.
- `run_bw_ineq_io_cxl.py`
  Same topology as `run_bw_ineq_intra_io.py`, but workload and noise memory bind to NUMA 1.

Example commands:
- `python3 scripts/run_bw_ineq_cc_io.py --dry-run`
- `python3 scripts/run_bw_ineq_intra_io.py 11 33 55`
- `python3 scripts/run_bw_ineq_io_cxl.py --config configs/llamacpp.example.json`

Auto-merge behavior:
- `run_chiplet_ycsb_harness.py` now updates recognized merged TSVs automatically after a successful run.
- Recognized families currently include fixed-workload noise, single-chiplet SMT NUMA, SMT14 NUMA, and `bw-ineq-*`.
- Set `HARNESS_AUTO_MERGE=0` if you need to disable the post-run merge step temporarily.

Legacy shell wrappers and ad-hoc scripts were backed up to `/home/seunghyun/ycsb/script_old`.
