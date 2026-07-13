Refer `../general_manifest.md` for the general information about the experiment.

Using 4 chiplets selected from the first socket: `ccd0`, `ccd1`, `ccd3`, and `ccd4`
(`0-34`, skipping `ccd2` because it lacks CCM mapping in the current host setup).

1. Definition of skewness
* It is qualitatively defined by the difference of BW load on the compute chiplets.
* 25 : 25%,50%,75%,100%, 
* 0 : 50% load on each chiplet, so no skewness.
* 100 : 100% load on two chiplet, so max skewness.
* 50: 75% load on two chiplet, 25% on the others, so moderate skewness.

2. benchmark
* use gapbs bfs and pr on synthetic kronecker scale 20
* use 9 threads for benchmark
* use 3 thread per each chiplet for load generation, so 0,1,2 thread for each chiplet can generate 0,25,50,75% load on each chiplet. load percentage is defined by the rate. 

3. policy notes
* The harness supports the legacy `eevdf` taskset-only baseline and pinned-placement policies (`optimal`, `same-chiplet`).
* It also supports scheduler-managed dynamic policies:
  * `eevdf-emulated`: the rustland-based emulated EEVDF scheduler.
  * `la-default`: the current default scheduler from `scx_rustland_la`.
* These scheduler-managed policies run the benchmark under `sched_ext` while keeping the synthetic noise workload outside the scheduler.
* Scheduler-managed runs inject workload environment variables after `sudo`, so settings such as `OMP_NUM_THREADS=9` survive the privilege boundary and reach the benchmark process.
