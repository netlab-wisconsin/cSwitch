# Active Configs

This directory now keeps the curated, currently supported configs.

- Chiplet density workloads
  - `chiplet_density_1to7_256mib.json`
  - `chiplet_density_1to7_256mib_memread_noise.json`
  - `duckdb_tpch_sf1_single_thread_chiplet_density.json`
  - `duckdb_tpch_sf1_single_thread_chiplet_density_q10.json`
  - `duckdb_tpch_sf1_single_thread_chiplet_density_q21.json`
  - `npb_*_class_b*_chiplet_density.json`

- Fixed-workload bases used by current Python sweep runners
  - `chiplet_fixed_1perchiplet_256mib.json`
  - `chiplet_fixed_1perchiplet_duckdb_tpch_sf1_q10.json`
  - `chiplet_fixed_1perchiplet_duckdb_tpch_sf1_q21.json`
  - `chiplet_fixed_1perchiplet_npb_*_class_b_single_thread.json`

- Single-chiplet SMT NUMA sweeps
  - Use the same fixed-workload base configs above.
  - The runner overrides assignments at runtime, so separate `*-1chiplet.json` copies are no longer needed.

- SMT14 backend-only NUMA sweeps
  - Use the same fixed-workload base configs above.
  - The runner overrides assignments at runtime with the explicit CPU order `0,1,2,3,4,5,6,84,85,86,87,88,89,90`.
  - DF monitoring is overridden to `CCM0`.

- Examples / smoke configs
  - `custom_assignment.example.json`
  - `duckdb_tpch.example.json`
  - `duckdb_tpch_smoke_sf0_01.json`
  - `filebench_fileserver.example.json`
  - `gapbs_bfs.example.json`
  - `gapbs_bfs_skewed_noise.example.json`
  - `gapbs_pagerank.example.json`
  - `gapbs_pagerank_skewed_noise.example.json`
  - `chiplet_fixed_1perchiplet_filebench_fileserver.json`
  - `chiplet_fixed_1perchiplet_llamacpp_qwen25_0_5b.json`
  - `chiplet_fixed_1perchiplet_xsbench.json`
  - `chiplet_fixed_1perchiplet_xsbench_bw_heavy.json`
  - `llamacpp.example.json`
  - `npb_instances.example.json`
  - `npb_omp.example.json`
  - `xsbench.example.json`
  - `xsbench_bw_heavy.example.json`

Notes:
- Active runner defaults are defined in `scripts/active_configs.py`.
- Active configs load cleanly against the current harness schema.
- Result archival decisions also use the active config catalog, so outdated prefixes can be moved to `results_old/`.

Legacy and ad-hoc configs were backed up to `/home/seunghyun/ycsb/config_old`.
