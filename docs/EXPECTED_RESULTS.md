# Expected Runtime And Results

These estimates describe one successful repeat (`AE_REPEATS=1`) on the
author-provided AMD EPYC 9634 host. They include all four configured variants
and allow for normal setup/build overhead, but not three complete failed
attempts at every point. Hardware state, filesystem cache state, and retries
can change wall time substantially.

## Resource Estimates

| Target | Paper points | Expected wall time | Recommended free space | Expected result |
| :----- | -----------: | -----------------: | ---------------------: | :-------------- |
| `check` | 0 | under 2 minutes | under 10 MB | All required host, source, lock, and workload checks pass |
| `primary dry-run` | 256 | under 2 minutes | under 100 MB | Exact command plan is recorded; no scheduler is launched |
| `fig10 smoke` | 1 | 5-15 minutes | 5 GB | One cSwitch workload point completes and writes `status.json` |
| `fig10` | 96 | 2-4 hours | 10 GB | cSwitch comparison across the 12-workload clean/loaded matrix |
| `fig11` | 24 | 0.5-1 hour | 5 GB | Benefit under heterogeneous free-core and busy-core noise |
| `fig12a` | 28 | 0.5-1 hour | 5 GB | Sensitivity to compute-I/O link load |
| `fig12b` | 28 | 0.5-1 hour | 5 GB | Sensitivity to overall I/O-chiplet load |
| `fig12` | 56 | 1-2 hours | 5 GB | Both Figure 12 load sweeps |
| `fig13` | 80 | 1-2 hours | 5 GB | PageRank scaling across thread counts |
| `primary` | 256 | 5-9 hours | 20 GB | Figures 10-13, summaries, plot data, PDFs, and PNGs |
| `optional` | 0 fresh points | under 2 minutes | under 100 MB | Frozen Figures 2-6 and 8 pass checksums and render |

The retained primary run directories observed during author testing occupied
less than 50 MB, with approximately 25 MB of copied scheduler binaries. The
larger free-space recommendations cover transient YCSB databases, build
scratch space, failed attempts, and future dependency builds. The harness
prunes regenerated heavy workload directories unless
`AE_KEEP_HEAVY_WORKLOAD_ARTIFACTS=1` is set.

`AE_MAX_ATTEMPTS=3` is a retry ceiling, not a promise that runtime stays within
the table. A machine or harness problem that causes every point to consume all
three attempts can approach three times the listed wall time.

## Success Criteria

A fresh target is functionally successful when:

1. `./reproduce.sh check` ends with `AE preflight passed.`
2. Each requested logical point has at least one `ok` or `salvaged` attempt and
   the aggregate table reports `success_count >= 1`.
3. The target writes `raw_results.tsv`, `aggregate_results.tsv`, and
   `summary.md` below
   `ae/results/campaigns/<campaign-id>/<figure>/results/`.
4. The plotting stage creates the requested PDF and PNG under
   the campaign's `figures/` directory and records unavailable comparison points in
   `missing_values.tsv` instead of fabricating values.

`salvaged` means the primary metric was parsed successfully but wrapper cleanup
returned nonzero. The logs and status record must be inspected, but the point
counts as a successful repeat by design.

## Expected Trends

Exact performance values are not pass/fail thresholds. They are sensitive to
firmware, temperature, background activity, workload versions, and the
available DIMM/CXL topology. Evaluators should compare qualitative trends:

- **Figure 10:** the AE graph normalizes each workload/panel to cSwitch, so the
  cSwitch bar is 1.0. Under I/O-chiplet load, cSwitch should remain competitive
  across the workload matrix and generally separate more clearly from EEVDF
  and Caladan+ than in the unloaded panel.
- **Figure 11:** cSwitch should retain an advantage under asymmetric free-core
  and busy-core noise. Missing EEVDF measurements remain blank and do not
  invalidate successful cSwitch points.
- **Figure 12:** each scheduler's no-noise (`rate=0`) point is 100%. As traffic
  increases from left to right, cSwitch should show a flatter degradation curve
  because it can migrate work away from congested paths.
- **Figure 13:** each scheduler is normalized to its own single-thread result.
  cSwitch should preserve the paper's scaling trend, with the clearest relative
  benefit in the loaded panel.

The artifact's EEVDF, ARCAS, and Caladan+ ports provide comparison context.
Their values are not independent reproduction claims for the original systems.

## Inspecting Failures

Start with:

```sh
find ae/results/latest -name status.json -print
find ae/results/latest -name status.json -exec grep -H '"status": "failed"' {} +
```

Each attempt directory preserves `command.txt`, `config.json`, launcher logs,
workload logs, and `status.json`. Zero, negative, and non-finite primary metrics
do not count as successful attempts. A new top-level command starts a fresh
campaign by default; use the printed `AE_OUT_ROOT=...` command to resume a
specific campaign and consume later attempt slots for failed points.
