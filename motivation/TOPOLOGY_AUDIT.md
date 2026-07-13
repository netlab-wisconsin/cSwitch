# NUMA and DIMM Topology Audit

## Conclusion

Among the paper experiments covered by this bundle, the only confirmed NPS4
experiment is **Figure 8d**. No paper point was found that requires NPS2.

Figures 8c, 8e, and 8f were not run under NPS4. Their exact raw noise logs all
start with:

```text
NUMA Available. Max Node ID: 1, Configured Nodes: 2
```

This is the normal paper host layout: NUMA0 is DIMM memory with all CPUs and
NUMA1 is CPU-less CXL.mem.

## Figure 8d: NPS4, Two DIMMs

The exact Figure 8d raw logs report:

```text
NUMA Available. Max Node ID: 4, Configured Nodes: 3
```

The runner binds traffic and victims to NUMA1 and NUMA3. The pre-run hugepage
record shows memory on nodes 1, 3, and 4. Combined with the experiment setup,
these are the two populated NPS4 DRAM domains plus CPU-less CXL.mem node 4.

Evidence:

- `reference/topology/fig8d_nps4_noise.log`
- `reference/topology/fig8d_hugepages_by_node.txt`
- `reference/exp3/fig8d/run_metadata.txt`

## Figure 8a/8b Mixed Historical Source

The base EEVDF/oracle data in the merged Figure 8a/8b result was collected in
the normal two-node NPS1+CXL layout. Later four-placement chiplet-affinity
replacement runs report 13 configured nodes: 12 L3/CCD NUMA domains plus
CXL.mem node 12.

This is **L3-as-NUMA**, not NPS2 or NPS4. The frozen full-run command reruns all
policies consistently under NPS1 and avoids mixing these two topologies.

Evidence:

- `reference/topology/fig8ab_base_nps1_noise.log`
- `reference/topology/fig8ab_replacement_l3_numa_noise.log`
- `original/exp3/general_manifest.md`

## Figure-by-Figure Classification

| Figure | Observed topology | Classification |
| --- | --- | --- |
| 2-6 AMD runs | March 13-17 host configuration, before later BIOS changes | NPS1; NUMA0 DIMM, NUMA1 CXL |
| 8a/8b base | max node 1, 2 configured nodes | NPS1+CXL |
| 8a/8b chiplet-affinity replacement | max node 12, 13 configured nodes | L3-as-NUMA+CXL; not NPS2/NPS4 |
| 8c | max node 1, 2 configured nodes | NPS1+CXL |
| 8d | max node 4, 3 configured memory nodes | NPS4, two DIMMs, plus CXL |
| 8e | max node 1, 2 configured nodes | NPS1+CXL |
| 8f graph | max node 1, 2 configured nodes | NPS1+CXL |
| 8f path-BW table | max node 1, 2 configured nodes | NPS1+CXL |

Do not infer an NPS mode from a `<numa>0</numa>` assignment alone. The raw
`Max Node ID` banner and CPU-bearing versus CPU-less node layout are the
decisive evidence used here.
