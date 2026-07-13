# Motivation Figure Map

| Paper panel | Frozen runner | Exact conditions | Preserved plot input |
| --- | --- | --- | --- |
| 2a | `original/ycsb/script_old/run_{amd,intel}_stall_sweep.sh` | RocksDB random 64-byte reads; 4, 16, 32, 64, 128, 256, 512 MiB | `plots/fig2-6/stall_per_op/workingset/rawdata-wl` |
| 2b | `original/ycsb/script_old/run_multi_instance_noise_thread_sweep.sh` | OrientDB and Elasticsearch; 128 MiB; memory-benchmark nthreads 1-7 | `plots/fig2-6/stall_per_op/core/rawdata-wl` |
| 3a/3b | `original/ycsb/scripts/run_chiplet_ycsb_harness.py` | RocksDB, OrientDB, Elasticsearch, DuckDB Q21; 1-7 apps per chiplet | `plots/fig2-6/core_stripped/{lat,bw}/data` |
| 4a/4b | same YCSB harness with `config_old/numa0_prefix_duckdb_tpch_sf1_single_thread.json` | DuckDB TPC-H Q1-22; 1,7,...,84 cores; 1/2/4/12 DIMMs | `plots/fig2-6/dimm-scaling/{lat,stall}/data` |
| 5a/5b | `original/ycsb/scripts/run_smt14_numa_backend_sweep.py` | llama.cpp and NPB FT/CG/MG; 1-14 SMT-aware CPUs; NUMA0 DIMM and NUMA1 CXL | `plots/fig2-6/per_chiplet/bw/{DIMM,CXL}/data` |
| 6a-6d | `original/ycsb/scripts/run_bw_ineq_*.py` | DuckDB Q21 and llama.cpp; CC-I/O, intra-I/O, I/O-DIMM, I/O-CXL | `plots/fig2-6/bw-ineq/*/data` |
| 8a/8b | `original/exp3/exp1/run_exp1.py` | PageRank, 3 threads; free/busy chiplets; EEVDF, chiplet-affinity, oracle | `plots/fig8/ne-cores/*/{data,plot.plt}` |
| 8c | `original/exp3/exp4/run_exp4.py` | Single-thread PageRank; same-core, same-LLC, oracle; 0-100% traffic | `plots/fig8/case2/data` |
| 8d | `original/exp3/exp_case3/run_exp_case3.py` | NPS4, two DIMMs; victim core/memory/CC/non-overlap; constant core21-to-NUMA1 traffic | `plots/fig8/case3/data` |
| 8e | `original/exp3/exp_case4-1/run_exp_case4_1.py` | PageRank Twitter graph; skew 0/25/50/75/100 | `plots/fig8/case4-1/data` |
| 8f graph | `original/exp3/exp_case4-2/run_exp_case4_2.py` | node-replication skiplist, 50% writes, 9 threads | `plots/fig8/case4-2/data` |
| 8f table | `original/exp3/exp3/run_exp3.py` | one-way 256 KiB cache-to-cache bandwidth, 10 repeats | `reference/exp3/fig8f_bw/exp3_results.csv` |

The Figure 8c paper data was recovered from `chipletos` commit `bdcd9cd`.
The file later found at the same workspace path is retained as
`plots/fig8/case2/data.overwritten-20260710` and is not used for plotting.

Fresh Figure 4 and Figure 8d runs are outside the AE scope because they require
physical DIMM changes and an NPS4/two-DIMM reboot, respectively. Their table
rows document the preserved runner and provenance; evaluators use the frozen
plot inputs for those panels.
