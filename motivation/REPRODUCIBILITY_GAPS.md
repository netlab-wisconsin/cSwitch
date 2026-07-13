# Reproducibility Gaps and Manual Steps

## Figure 2

- Figure 2a preserves both launchers and the final plot data, but the complete
  original AMD and Intel raw result directories are no longer present.
- Figure 2b preserves exact extracted Intel TSVs. The exact AMD raw run is not
  present, so the final paper plot input is the authoritative frozen value.
- Both panels require separate AMD and Intel hosts.

## Figure 3

The exact resolved configs and raw group/instance summaries are preserved in
`reference/ycsb/fig3_*`. The script that transformed those summaries into the
final `core_stripped` latency and total-bandwidth tables was not found. The
final paper tables are therefore frozen directly under `plots/fig2-6`.

## Figure 4

The harness and combined `dimm_exp.tsv` are preserved. Physical DIMM
installation/removal was manual and was not encoded in the original runner,
so a fresh run is outside the AE scope. Evaluators reproduce the panel from
the frozen paper input.

## Figures 5 and 6

The source harnesses, exact selection manifests, merged summaries, and final
plot inputs are preserved. The active defaults in several Figure 6 preset
files were later changed to a small RocksDB test; `motivation/harness.py`
therefore passes the paper's llama.cpp and DuckDB configs explicitly.

## Figure 8

- Figure 8a/8b's historical merged result mixes NPS1 base data with later
  L3-as-NUMA chiplet-affinity replacement runs. A clean rerun should collect
  every policy under the same NPS1 topology.
- Figure 8c's workspace plot data was overwritten after the paper. The frozen
  input was recovered from git commit `bdcd9cd`.
- A fresh Figure 8d run is outside the AE scope because it requires NPS4 and
  two DIMMs. Evaluators reproduce the panel from the frozen paper input.
- Figure 8e uses the external Twitter graph file, and Figure 8f uses the
  external node-replication source/binary. These large benchmark inputs are
  intentionally not duplicated in this source snapshot.

Every preserved reference result names its original author-workspace path in
`SOURCE_PATHS.tsv`. `SHA256SUMS` detects accidental edits to the snapshot.
