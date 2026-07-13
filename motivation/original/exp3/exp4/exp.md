Refer `../general_manifest.md` for the general information about the experiment.

## Experiment 4

Single-thread GAPBS placement under background memory-bandwidth load.

## Goal

Measure how single-thread GAPBS performance changes as background memory load increases, while varying the CPU mask exposed to the workload:

* `same-core`: `taskset -c 4`
* `same-llc`: `taskset -c 4-6`
* `optimal`: `taskset -c 7-83`

The background noise always runs on physical cores `0-3`.

## Fixed Decisions

* benchmarks: `bfs`, `pr`
* graph source: synthetic `kronecker scale 20`
* GAPBS threading: `OMP_NUM_THREADS=1`
* load points: `0,20,40,60,80,100`
* repeats: `5`
* primary metric: GAPBS `Trial Time`
* secondary metric: slowdown versus the same `benchmark + placement` at `0%` load

## Noise Model

* tool: `/home/seunghyun/sched_bench/build/memory_benchmark`
* noise cores: `0,1,2,3`
* worker memory: `256MB` per thread
* worker mode: read-only `Mode=0`
* worker NUMA policy: `bind`
* worker NUMA node: `0`

The runner performs a short calibration pass with candidate rates `1000,600,500,300,220,200,150,100,50,0`, measures actual summed bandwidth, and maps those measurements to the target load points `20/40/60/80/100%`.

## Implemented Runner

* script: `/home/seunghyun/exp3/exp4/run_exp4.py`
* default benchmark set: `bfs`, `pr`
* default placement set: `same-core`, `same-llc`, `optimal`
* default load set: `0`, `20`, `40`, `60`, `80`, `100`
* default repeats: `5`
* topology guard:
  * `0-6` must share one `L3`
  * `same-core` CPU `4` must be inside that local `L3`
  * `7` must be on a different `L3`
* run order:
  * conditions are shuffled deterministically per repeat
  * repeat order and seed are recorded in metadata

## GAPBS Commands

* `bfs`: `taskset -c <mask> /home/seunghyun/gapbs/gapbs/bfs -g 20 -n1`
* `pr`: `taskset -c <mask> /home/seunghyun/gapbs/gapbs/pr -g 20 -i100 -n1`

The runner clears `OMP_PLACES` and `OMP_PROC_BIND` before launching GAPBS.

## Usage

```bash
python3 /home/seunghyun/exp3/exp4/run_exp4.py --dry-run
python3 /home/seunghyun/exp3/exp4/run_exp4.py
python3 /home/seunghyun/exp3/exp4/run_exp4.py --benchmarks bfs --placements same-llc --load-pcts 0 20 40 --repeats 1
```

## Output

* result root: `exp4/results/<timestamp>/`
* per-run CSV: `exp4_results.csv`
  * fields: `benchmark,placement,load_pct,rate,repeat,trial_time_s,slowdown_vs_load0,sum_noise_bandwidth_mbs`
* aggregated CSV: `exp4_summary_stats.csv`
  * fields: `benchmark,placement,load_pct,runs,mean_trial_time_s,stdev_trial_time_s,mean_slowdown_vs_load0,mean_noise_bandwidth_mbs`
* calibration CSV: `exp4_calibration.csv`
  * fields: `rate,target_pct,measured_sum_noise_bandwidth_mbs`
* raw logs: `raw/*.log`
* generated XML configs: `configs/*.xml`
* metadata: `run_metadata.txt`
