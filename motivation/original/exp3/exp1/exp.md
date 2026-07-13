## things you need before reading this
* Chiplet topology in our system -> 7 cores per chiplet, 12 chiplets total
* Currently SMT enabled, so please only use 0-83 cores for pinning (84-167 are SMT siblings) / or use lstopo to check the topology and pinning
* For this exp, we will only use 4 chiplets (domains)
* domains refers LLC domains, which is chiplet in our case.
* We want to say that we need to consider link capacity between compute chiplet to IO chiplet when we schedule jobs.
    * Compute chiplet to IO link capacity : ~35GB/s 

## Tools you may need
* gapbs : main benchmark to run, placed in /home/seunghyun/gapbs/gapbs
* memory_benchmark : tool to generate a load. /home/seunghyun/sched_bench/build/memory_benchmark
    * It requires config xml, refer /home/seunghyun/gapbs/profiler for the sample use. 


실험1: free cores are not same / busy cores are not same
* domain 4개 25, 50,75,100% load (with 4 threads) -> free cores (3개)
    * 25% load : 4 threads on rate 500
    * 50% load : 4 threads on rate 220
    * 75% load : 4 threads on rate 150
    * 100% load : 4 threads on rate 50
* busy cores scenario : all 7 threads on same rate. 
* on EEVDF / optimal / putting jobs on same chiplet 3가지 policy.
    * eevdf, just don't pin jobs, taskset to the range of 4 chiplets
    * optimal, pin jobs to the chiplet with 25% load. 
    * same chiplet - free-cores에서는 `4,5,6`, `11,12,13`, `18,19,20`, `25,26,27` 4개 placement를 모두 돌리고 평균낸다.
* 구현 자체는 pinning으로 간단. 
* measure the performance of gapbs (pagerank, bfs, and few more) with 3 OpenMP threads, and see how the performance changes with different policies on free/busy scenarios.

## Implemented Runner
* script: `/home/seunghyun/exp3/exp1/run_exp1.py`
* default benchmark set: `bfs`, `pr`, `cc`
* default policy set: `eevdf`, `optimal`, `same-chiplet`
* default scenario set: `free-cores`, `busy-cores`
* fixed OpenMP setting: `OMP_NUM_THREADS=3`
* default graph source: synthetic `kronecker scale 20`
* `perf sched` capture mode: `auto`
* use `--perf-sched-sudo` when tracepoint permission requires sudo
* free-cores `same-chiplet` is aggregated over 4 placements, so its summary stats are the mean across all placement x repeat runs
* busy-cores `same-chiplet` can also be aggregated over 4 chiplet affinities: `0-6`, `7-13`, `14-20`, `21-27`

## Core Choices Implemented
* chiplets used: `CCD0-CCD3`
    * `CCD0`: `0-6`
    * `CCD1`: `7-13`
    * `CCD2`: `14-20`
    * `CCD3`: `21-27`
* free-cores scenario noise threads
    * `CCD0`: `0,1,2,3` rate `500`
    * `CCD1`: `7,8,9,10` rate `220`
    * `CCD2`: `14,15,16,17` rate `150`
    * `CCD3`: `21,22,23,24` rate `50`
* busy-cores scenario noise threads
    * `CCD0`: `0-6` rate `500`
    * `CCD1`: `7-13` rate `220`
    * `CCD2`: `14-20` rate `150`
    * `CCD3`: `21-27` rate `50`
* free-cores optimal placement: `OMP_PLACES={4},{5},{6}`
* free-cores same-chiplet placement set:
    * `OMP_PLACES={4},{5},{6}`
    * `OMP_PLACES={11},{12},{13}`
    * `OMP_PLACES={18},{19},{20}`
    * `OMP_PLACES={25},{26},{27}`
* free-cores same-chiplet aggregate: mean across the 4 placements above
* busy-cores optimal affinity: `taskset -c 0-6`
* busy-cores same-chiplet affinity set:
    * `taskset -c 0-6`
    * `taskset -c 7-13`
    * `taskset -c 14-20`
    * `taskset -c 21-27`
* busy-cores same-chiplet aggregate: mean across the 4 chiplet affinities above

## Usage
```bash
python3 /home/seunghyun/exp3/exp1/run_exp1.py --dry-run
python3 /home/seunghyun/exp3/exp1/run_exp1.py
python3 /home/seunghyun/exp3/exp1/run_exp1.py --graph-scale 20
python3 /home/seunghyun/exp3/exp1/run_exp1.py --graph-scale 20 --perf-sched-sudo
python3 /home/seunghyun/exp3/exp1/run_exp1.py --benchmarks bfs --policies eevdf --scenarios free-cores --repeats 1
```

## Output
* result root: `exp1/results/<timestamp>/`
* per-run CSV: `exp1_results.csv`
    * includes `placement_tag`, `placement_detail`, `perf_sched_status`, `perf_sched_trace_path`, `perf_sched_cpus`
* per-thread residency CSV: `exp1_thread_domains.csv`
* aggregated per-thread residency CSV: `exp1_thread_domain_summary.csv`
* aggregated CSV: `exp1_summary_stats.csv`
* raw logs: `raw/*.log`
* perf sched traces when available: `raw/*__perf.script.log`
* perf sched timehist logs when available: `raw/*__perf.timehist.log`
* generated XML configs: `configs/*.xml`
* note: actual `perf sched` capture depends on tracepoint permission on the host
