# Scheduler Source Snapshot

The default `main` branch contains only scheduler source. The
`artifact-evaluation` branch starts from the paper scheduler commit and adds
`ae/` and `motivation/` without relocating the scheduler source.

The evaluator release is the immutable tag named in `ae/ARTIFACT_RELEASE`
(`sosp26-ae-v6`). `./reproduce.sh check` resolves that tag and requires the
current `HEAD` to match its commit before a fresh campaign starts. This release
check is separate from the scheduler-source check below.

`ae/SOURCE_COMMIT` records the scheduler-only commit used by this release.
Because the following evaluation commit changes the branch HEAD, the wrappers
compare scheduler-owned paths to that source commit rather than requiring
`HEAD` to equal it. `ae/SOURCE_SHA256SUMS` provides the same verification for
GitHub zip/tar source archives without Git history.

That scheduler commit is based on paper source commit
`c9560e5024b0d0fdfb2b0ad1f6731f1ffb1c5de6` and adds the default-enabled
`--adopt-workload-descendants` launch-control option. Its default preserves the
paper scheduler behavior. The AE harness disables automatic descendant
adoption only for partial-cgroup YCSB runs, then explicitly admits each
measured run-phase Java process after database setup. This does not change the
scheduler placement policy.

To use another scheduler clone, check out the pinned release source commit and
provide its path:

```sh
git -C /path/to/scx_rustland_la checkout "$(cat ae/SOURCE_COMMIT)"
AE_SCHEDULER_ROOT=/path/to/scx_rustland_la ae/run.sh fig13 dry-run
```

Set `AE_ALLOW_SOURCE_MISMATCH=1` only for an intentional development run using
modified scheduler source.
