# Scheduler Source Snapshot

The default `main` branch contains only scheduler source. The
`artifact-evaluation` branch starts from the paper scheduler commit and adds
`ae/` and `motivation/` without relocating the scheduler source.

The evaluator release is the immutable tag named in `ae/ARTIFACT_RELEASE`
(`sosp26-ae-v1`). `./reproduce.sh check` resolves that tag and requires the
current `HEAD` to match its commit before a fresh campaign starts. This release
check is separate from the scheduler-source check below.

`ae/SOURCE_COMMIT` records the paper scheduler base commit. Because evaluation
commits change the branch HEAD, the wrappers compare scheduler-owned paths to
that base rather than requiring `HEAD` to equal it. `ae/SOURCE_SHA256SUMS`
provides the same verification for GitHub zip/tar source archives without Git
history.

To use another scheduler clone, check out the paper source commit and provide
its path:

```sh
git -C /path/to/scx_rustland_la checkout "$(cat ae/SOURCE_COMMIT)"
AE_SCHEDULER_ROOT=/path/to/scx_rustland_la ae/run.sh fig13 dry-run
```

Set `AE_ALLOW_SOURCE_MISMATCH=1` only for an intentional development run using
modified scheduler source.
