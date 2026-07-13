## things you need before reading this
* Chiplet topology in our system -> 7 cores per chiplet, 12 chiplets total
* NUMA per L3 Domain Enabled -> each chiplet is a NUMA node, so 12 NUMA nodes total
    * For memory, each chiplet has 96GB memory as a NUMA node. If unspecified, set up memory bandwidth benchmark to interleave across all NUMA nodes, so that the load is distributed across all chiplets.(But do not include numa#12, which is CXL.mem. If it is not possible to interleave with specific NUMA nodes, let me know and I'll reset the bios to disable NUMA per L3 domain)
* Currently SMT enabled, so please only use 0-83 cores for pinning (84-167 are SMT siblings) / or use lstopo to check the topology and pinning
* domains refers LLC domains, which is chiplet in our case.
* We want to say that we need to consider link capacity between compute chiplet to IO chiplet when we schedule jobs.
    * Compute chiplet to IO link capacity : ~35GB/s 
    * Each DIMM BW : ~35GB/s


## Tools you may need

* memory_benchmark : tool to generate a load. /home/seunghyun/sched_bench/build/memory_benchmark
    * It requires config xml, refer /home/seunghyun/gapbs/profiler for the sample use. 
    * To modify NUMA node interleaving, you may need to modify source code and recompile to support the interleaving across all NUMA nodes. By default, it may use only one NUMA node, which will generate load on only one chiplet.
    * Rate
    * 25% load : 4 threads on rate 500
    * 50% load : 4 threads on rate 220
    * 75% load : 4 threads on rate 150
    * 100% load : 4 threads on rate 50
* core-to-core-latency : /home/seunghyun/core-to-core-latency
    * exp3 primary benchmark will be added here as a new bandwidth bench.
* likwid-bench / likwid-perfctr
    * available locally
    * use only for validation / counter sanity, not as the primary exp3 metric
* GAPBS
    * available locally on /home/seunghyun/gapbs
* NPB 
    * available locally on /home/seunghyun/ycsb with harness. You may want to configure harness
    * Or you can just download and build NPB separately.

