# Licensing And Third-Party Material

## Project Material

The cSwitch scheduler, author-written artifact harnesses and plotting scripts,
and author-generated measurement/plot data are distributed under
GPL-2.0-only, represented by the root `LICENSE`, unless an individual file or
directory states otherwise.

## Third-Party Material

Third-party software, models, and datasets are not relicensed by this
repository. Their upstream terms continue to apply. The artifact does not
redistribute the Llama 3.1 model, the 12 GB Twitter graph, benchmark databases,
or external workload binaries. Exact versions and source links are listed in
`docs/EXTERNAL_DEPENDENCIES.md`.

The source snapshot under `vendor/scx_rustland_core/` includes its upstream
GPL-2.0-only license. Files under `motivation/original/` that were derived from
third-party projects retain the applicable upstream notices and terms; the
author-written glue around them is GPL-2.0-only.

The patch files under `third_party/patches/` contain modifications against
third-party source trees. Applying a patch does not change the upstream
project's license.
