# External Dependencies

The author-provided AMD EPYC 9634 host is the canonical environment for fresh
Figures 10-13 runs. Large workload repositories, generated databases, graph
inputs, and models are intentionally not stored in this Git repository. The
release preflight verifies the required paths before a campaign starts.

The path defaults below describe the supplied host. Set the corresponding
`AE_*` variable when installing a dependency elsewhere; no harness edit is
required.

## Version Inventory

| Component | Upstream and pinned revision | License | Artifact patch |
| --- | --- | --- | --- |
| GAP Benchmark Suite (GAPBS) | <https://github.com/sbeamer/gapbs.git>, `b5e3e19c2845f22fb338f4a4bc4b1ccee861d026` | BSD-3-Clause | `third_party/patches/gapbs.patch` |
| Yahoo Cloud Serving Benchmark (YCSB) | <https://github.com/brianfrankcooper/YCSB.git>, `8b2ecaf9c876d930096637e539c9725b5c3ba950` | Apache-2.0 | `third_party/patches/ycsb.patch` |
| llama.cpp | <https://github.com/ggml-org/llama.cpp>, `b6c83aad55a4ce17ec96fced7770cd1be8758193` | MIT | none |
| Filebench | <https://github.com/filebench/filebench.git>, `22620e602cbbebad90c0bd041896ebccf70dbf5f` | CDDL-1.0 | `third_party/patches/filebench.patch` |
| node-replication | <https://github.com/vmware-archive/node-replication.git>, `57075c3ddaaab1098d3ec0c2b7d01cb3b57e1ac7` | MIT OR Apache-2.0 | `third_party/patches/node-replication.patch` |
| memory benchmark (`sched_bench`) | <https://github.com/netlab-wisconsin/membench.git>, `271a0a592c8b937b62db2e527e2f9df036623dee` | No upstream license file at the pinned revision | `third_party/patches/membench.patch` |

The artifact does not redistribute these source trees or their built binaries.
Their upstream licenses and notices remain authoritative. In particular, the
absence of a license in the pinned `membench` tree means that repository should
not be redistributed without separate permission.

## Common Patch Workflow

Clone each project at its exact revision and verify the patch before applying
it. The patch inventory records the same base revisions.

```sh
AE_ROOT=$HOME/ae
git clone <upstream-url> <destination>
git -C <destination> checkout <pinned-revision>
git -C <destination> apply --check "$AE_ROOT/third_party/patches/<name>.patch"
git -C <destination> apply "$AE_ROOT/third_party/patches/<name>.patch"
```

## Primary Workload Setup

### GAPBS

```sh
git clone https://github.com/sbeamer/gapbs.git "$HOME/gapbs/gapbs"
git -C "$HOME/gapbs/gapbs" checkout b5e3e19c2845f22fb338f4a4bc4b1ccee861d026
git -C "$HOME/gapbs/gapbs" apply "$HOME/ae/third_party/patches/gapbs.patch"
make -C "$HOME/gapbs/gapbs" -j"$(nproc)" pr bc converter
make -C "$HOME/gapbs/gapbs" bench-graphs
```

The Figure 13 PageRank sweep uses `benchmark/graphs/twitter.sg`. Figure 10's
GAPBS points and the Figure 11-12 PageRank points retain their generated
Kronecker scale-20 inputs. The Twitter graph recipe in the pinned
GAPBS `benchmark/bench.mk` downloads the compressed source from the ANLAB-KAIST
trace release and converts it. The generated graph occupies roughly 12 GB on
the supplied host. Override the checkout and graph directory with
`AE_GAPBS_ROOT` and `AE_GAPBS_GRAPH_ROOT`; override only the Figure 13 input
with `AE_FIG13_TWITTER_GRAPH` when needed.

### YCSB Workspace

The primary harness expects a workspace, not only the upstream YCSB checkout:

```text
$AE_YCSB_ROOT/
|-- YCSB/                       # patched upstream checkout
|-- scripts/
|-- chiplet_harness/
|-- workloads/
`-- benchmarks/
    |-- filebench/
    `-- llama.cpp/
```

The artifact preserves the author-written `scripts/`, `chiplet_harness/`, and
`workloads/` files under `motivation/original/ycsb/`. Primary runs execute the
artifact-local script and harness directly; the external workspace supplies
the patched upstream `YCSB/` checkout, generated workload files, benchmark
binaries, and models. For a new machine, install the three preserved
directories at the workspace root, clone YCSB into `YCSB/`, check out the
pinned revision, and apply `ycsb.patch`. YCSB uses Maven; build only the
required RocksDB, OrientDB, and Elasticsearch bindings or build the complete
distribution with the upstream command:

```sh
cd "$AE_YCSB_ROOT/YCSB"
mvn clean package
```

The harness creates fresh benchmark databases below its result directory.
Set `AE_YCSB_ROOT` to the installed workspace. Override `AE_YCSB_RUNNER` only
when intentionally testing another harness copy.

OrientDB 2.2.37 requires Java 8 on the supplied host; newer Java runtimes fail
while opening the embedded `plocal` engine. Install a Java 8 runtime and set:

```sh
export AE_ORIENTDB_JAVA_HOME=/usr/lib/jvm/java-8-openjdk-amd64/jre
```

### llama.cpp And Model

```sh
git clone https://github.com/ggml-org/llama.cpp "$AE_YCSB_ROOT/benchmarks/llama.cpp"
git -C "$AE_YCSB_ROOT/benchmarks/llama.cpp" checkout b6c83aad55a4ce17ec96fced7770cd1be8758193
cmake -S "$AE_YCSB_ROOT/benchmarks/llama.cpp" \
  -B "$AE_YCSB_ROOT/benchmarks/llama.cpp/build" -DGGML_NATIVE=ON
cmake --build "$AE_YCSB_ROOT/benchmarks/llama.cpp/build" -j"$(nproc)" --target llama-bench
```

Figure 10 uses `Meta-Llama-3.1-8B-Instruct-Q4_K_M.gguf` (approximately
4.6 GB). The model is not distributed by this artifact; obtain it under the
Meta Llama license and verify:

```text
SHA256  7b064f5842bf9532c91456deda288a1b672397a54fa729aa665952863033557c
```

Set `AE_LLAMA_ROOT` to the llama.cpp checkout and `AE_LLAMA_MODEL` to the GGUF
file. Users are responsible for accepting and complying with the model terms.

### Filebench

Clone Filebench into `$AE_YCSB_ROOT/benchmarks/filebench`, check out the pinned
revision, apply `filebench.patch`, then use its normal autotools build:

```sh
cd "$AE_YCSB_ROOT/benchmarks/filebench"
libtoolize
aclocal
autoheader
automake --add-missing
autoconf
./configure
make -j"$(nproc)"
```

The AE-local templates in `ae/templates/` define the short File Server, Web
Proxy, Web Server, and Varmail runs. The upstream source remains external.

### node-replication

Clone and patch node-replication at `$AE_EXP3_ROOT/node-replication`. The AE
harness points `AE_NODE_REPLICATION_ROOT` at its `node-replication/` package
and invokes `cargo bench --no-run` automatically when a required release bench
binary is missing. A manual build check is:

```sh
cd "$AE_EXP3_ROOT/node-replication/node-replication"
RUSTUP_TOOLCHAIN=stable RUSTC_BOOTSTRAP=1 cargo bench --bench lockfree --no-run
RUSTUP_TOOLCHAIN=stable RUSTC_BOOTSTRAP=1 \
  cargo bench --bench hashmap --no-run --features cmp
```

### Memory Traffic Generator

Clone `membench`, check out the pinned revision, apply `membench.patch`, and
build it with a C++20 compiler, CMake 3.15 or newer, and libnuma headers:

```sh
cmake -S <membench-checkout> -B <membench-checkout>/build
cmake --build <membench-checkout>/build -j"$(nproc)"
```

Set `AE_MEMORY_BENCHMARK` to the resulting `build/memory_benchmark` binary.
This traffic generator is required for loaded primary points but is not
redistributed because its pinned upstream tree has no license file.

## Scheduler Comparison Ports

The release host also contains the author-maintained EEVDF, ARCAS, and
Caladan+ scheduler ports used as contextual comparison series. ARCAS and
Caladan+ are built from the cSwitch source tree through feature-selected AE
variants. The EEVDF port is an author-modified tree based on sched_ext commit
`7298f797b83a105e74a8742355c58e3661f83091`; it is not claimed as an
independent reproduction of upstream Linux EEVDF or its paper. The supplied
host tree includes a rustland dispatch-liveness fix validated by the Figure 10
loaded File Server test; this comparison port remains a configured host input,
not part of the cSwitch source snapshot.

The public artifact's core claim is reproducible with the supplied host and
its pinned ports. For cSwitch-only development without the EEVDF tree, set:

```sh
AE_VARIANTS=paper-greedy ./reproduce.sh fig10 dry-run
```

## Host Toolchain

The supplied release was prepared with Rust/Cargo 1.84.1, Python 3.10.12, and
gnuplot 5.4.2. Primary runs additionally require Linux `sched_ext`, cgroup v2,
`taskset`, `timeout`, `flock`, passwordless `sudo -n`, and MSR access. Plotting
requires `gnuplot` and `pdftoppm`.

After installing or overriding dependencies, verify all paths before a run:

```sh
./reproduce.sh check
./reproduce.sh primary dry-run
```
