# Third-Party Patch Inventory

These patches record the author-local changes required by the artifact's
external workload trees. They do not include or relicense the upstream source.

| Patch | Apply at revision |
| --- | --- |
| `gapbs.patch` | GAPBS `b5e3e19c2845f22fb338f4a4bc4b1ccee861d026` |
| `ycsb.patch` | YCSB `8b2ecaf9c876d930096637e539c9725b5c3ba950` |
| `filebench.patch` | Filebench `22620e602cbbebad90c0bd041896ebccf70dbf5f` |
| `node-replication.patch` | node-replication `57075c3ddaaab1098d3ec0c2b7d01cb3b57e1ac7` |
| `membench.patch` | membench `271a0a592c8b937b62db2e527e2f9df036623dee` |

From a clean checkout of a listed base revision:

```sh
git apply --check /path/to/ae/third_party/patches/<name>.patch
git apply /path/to/ae/third_party/patches/<name>.patch
```

See `docs/EXTERNAL_DEPENDENCIES.md` for upstream URLs, licenses, build
commands, large input provenance, and the environment variables used to point
the harness at a reconstructed installation.
