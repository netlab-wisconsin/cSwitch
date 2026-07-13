#!/usr/bin/env python3

from __future__ import annotations

import argparse
import json
import os
import shlex
import shutil
import subprocess
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable


ROOT = Path(__file__).resolve().parent
ORIGINAL = ROOT / "original"
REFERENCE = ROOT / "reference"
PLOTS = ROOT / "plots"
OUT_ROOT = Path(os.environ.get("MOTIVATION_OUT_ROOT", ROOT / "results")).resolve()
PYTHON = os.environ.get("PYTHON_BIN", "python3")


@dataclass(frozen=True)
class Command:
    label: str
    argv: tuple[str, ...]
    cwd: Path
    topology: str = "nps1-cxl"
    env: tuple[tuple[str, str], ...] = ()
    note: str = ""


RUN_FIGURES = (
    "fig2a",
    "fig2b",
    "fig3",
    "fig4",
    "fig5",
    "fig6a",
    "fig6b",
    "fig6c",
    "fig6d",
    "fig8ab",
    "fig8c",
    "fig8d",
    "fig8e",
    "fig8f",
    "fig8f-bw",
)

GROUPS = {
    "fig2": ("fig2a", "fig2b"),
    "fig6": ("fig6a", "fig6b", "fig6c", "fig6d"),
    "fig8": ("fig8ab", "fig8c", "fig8d", "fig8e", "fig8f", "fig8f-bw"),
    "all": RUN_FIGURES,
}

PLOT_TARGETS = {
    "fig2a": (("fig2-6/stall_per_op/workingset/plot_wl.plt", "wl-Stall-rocksdb.pdf", "fig2a.pdf"),),
    "fig2b": (("fig2-6/stall_per_op/core/plot_wl.plt", "core-Stall-rocksdb.pdf", "fig2b.pdf"),),
    "fig3": (
        ("fig2-6/core_stripped/lat/plot_wl.plt", "core-stripped-lat.pdf", "fig3a.pdf"),
        ("fig2-6/core_stripped/bw/plot_wl.plt", "core-stripped-bw.pdf", "fig3b.pdf"),
    ),
    "fig4": (
        ("fig2-6/dimm-scaling/lat/plot_wl.plt", "dimm-scaling-lat.pdf", "fig4a.pdf"),
        ("fig2-6/dimm-scaling/stall/plot_wl.plt", "dimm-scaling-stall.pdf", "fig4b.pdf"),
    ),
    "fig5": (
        ("fig2-6/per_chiplet/bw/DIMM/plot_wl.plt", "one-chiplet-per-core-bw-dimm.pdf", "fig5a.pdf"),
        ("fig2-6/per_chiplet/bw/CXL/plot_wl.plt", "one-chiplet-per-core-bw-cxl.pdf", "fig5b.pdf"),
    ),
    "fig6a": (("fig2-6/bw-ineq/cc-io/plot_wl.plt", "bw-ineq-cc-io.pdf", "fig6a.pdf"),),
    "fig6b": (("fig2-6/bw-ineq/intra-io/plot_wl.plt", "bw-ineq-intra-io.pdf", "fig6b.pdf"),),
    "fig6c": (("fig2-6/bw-ineq/io-dimm/plot_wl.plt", "bw-ineq-io-dimm.pdf", "fig6c.pdf"),),
    "fig6d": (("fig2-6/bw-ineq/io-cxl/plot_wl.plt", "bw-ineq-io-cxl.pdf", "fig6d.pdf"),),
    "fig8ab": (
        ("fig8/ne-cores/free-merged/plot.plt", "free-merged.pdf", "fig8a.pdf"),
        ("fig8/ne-cores/busy-merged/plot.plt", "busy-merged.pdf", "fig8b.pdf"),
    ),
    "fig8c": (("fig8/case2/plot.plt", "chara-case-2.pdf", "fig8c.pdf"),),
    "fig8d": (("fig8/case3/plot.plt", "chara-case-3.pdf", "fig8d.pdf"),),
    "fig8e": (("fig8/case4-1/plot.plt", "chara-case-4-1.pdf", "fig8e.pdf"),),
    "fig8f": (("fig8/case4-2/plot.plt", "chara-case-4-2.pdf", "fig8f.pdf"),),
}


def expand_figure(value: str) -> tuple[str, ...]:
    if value in GROUPS:
        return GROUPS[value]
    if value in RUN_FIGURES:
        return (value,)
    choices = ", ".join((*RUN_FIGURES, *GROUPS))
    raise SystemExit(f"unknown figure {value!r}; choose one of: {choices}")


def sudo_prefix() -> tuple[str, ...]:
    if os.environ.get("MOTIVATION_USE_SUDO", "1") == "0":
        return ()
    return ("sudo", "-n", "-E")


def ycsb_worktree() -> Path:
    return OUT_ROOT / "worktrees" / "ycsb"


def prepare_ycsb_worktree() -> Path:
    source = ORIGINAL / "ycsb"
    target = ycsb_worktree()
    target.parent.mkdir(parents=True, exist_ok=True)
    shutil.copytree(source, target, dirs_exist_ok=True)

    dimm_config = target / "config_old" / "numa0_prefix_duckdb_tpch_sf1_single_thread.json"
    shutil.copy2(dimm_config, target / "configs" / dimm_config.name)

    memory_benchmark = Path(
        os.environ.get("MOTIVATION_MEMORY_BENCHMARK", "/home/seunghyun/ycsb/memory_benchmark")
    ).resolve()
    staged_binary = target / "memory_benchmark"
    if staged_binary.is_symlink() and staged_binary.resolve() != memory_benchmark:
        staged_binary.unlink()
    if not staged_binary.exists():
        staged_binary.symlink_to(memory_benchmark)
    return target


def materialize_ycsb_config(source: Path, name: str, *, result_suffix: str | None = None) -> Path:
    generated = OUT_ROOT / "generated-configs"
    generated.mkdir(parents=True, exist_ok=True)
    payload = json.loads(source.read_text(encoding="utf-8"))
    payload["result_root"] = str(ycsb_worktree() / "results")
    if result_suffix:
        payload["result_prefix"] = f"{payload['result_prefix']}_{result_suffix}"
    target = generated / name
    target.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
    return target


def ycsb_env() -> tuple[tuple[str, str], ...]:
    worktree = ycsb_worktree()
    return tuple(
        sorted(
            {
                "YCSB_ROOT": str(worktree),
                "YCSB_HOME": os.environ.get("MOTIVATION_YCSB_HOME", "/home/seunghyun/ycsb/YCSB"),
                "WORKLOAD_DIR": str(worktree / "workloads"),
                "RESULTS_DIR": str(worktree / "results"),
                "PYTHON_BIN": PYTHON,
            }.items()
        )
    )


def current_vendor() -> str:
    override = os.environ.get("MOTIVATION_CPU_VENDOR")
    if override:
        if override not in {"amd", "intel"}:
            raise SystemExit("MOTIVATION_CPU_VENDOR must be amd or intel")
        return override
    output = subprocess.run(["lscpu"], check=True, capture_output=True, text=True).stdout
    if "AuthenticAMD" in output:
        return "amd"
    if "GenuineIntel" in output:
        return "intel"
    raise SystemExit("could not detect an AMD or Intel CPU vendor")


def run_dir(figure: str) -> Path:
    return OUT_ROOT / "runs" / figure


def command_plan(figure: str) -> tuple[Command, ...]:
    yroot = ycsb_worktree()
    exp = ORIGINAL / "exp3"
    output = run_dir(figure)
    env = ycsb_env()
    sudo = sudo_prefix()

    if figure == "fig2a":
        vendor = current_vendor()
        return (
            Command(
                label=f"Figure 2a {vendor.upper()} RocksDB working-set sweep",
                argv=("bash", str(yroot / "script_old" / f"run_{vendor}_stall_sweep.sh")),
                cwd=yroot,
                topology="vendor-host",
                env=env + (("SIZES_MB", "4 16 32 64 128 256 512"),),
                note="Run once on the AMD host and once on the Intel host.",
            ),
        )

    if figure == "fig2b":
        vendor = current_vendor()
        return (
            Command(
                label=f"Figure 2b {vendor.upper()} OrientDB/Elasticsearch load sweep",
                argv=(
                    "bash",
                    str(yroot / "script_old" / "run_multi_instance_noise_thread_sweep.sh"),
                    "--config",
                    str(yroot / "configs" / "fig2b_noise_template.xml"),
                    "--memory-benchmark",
                    str(yroot / "memory_benchmark"),
                    "--noise-thread-counts",
                    "1 2 3 4 5 6 7",
                    "--backends",
                    "orientdb elasticsearch",
                    "--instance-counts",
                    "1",
                    "--multi-instance-size-mb",
                    "128",
                    "--record-bytes",
                    "64",
                    "--field-length",
                    "54",
                    "--output-dir",
                    str(output / vendor),
                ),
                cwd=yroot,
                topology="vendor-host",
                env=env,
                note="Run once on each paper host; nthreads 1..7 maps to 0..100% load.",
            ),
        )

    if figure == "fig3":
        return (
            Command(
                label="Figure 3 RocksDB/OrientDB/Elasticsearch density sweep",
                argv=(*sudo, PYTHON, str(yroot / "scripts" / "run_chiplet_ycsb_harness.py"), "--config", str(OUT_ROOT / "generated-configs" / "fig3_ycsb.json")),
                cwd=yroot,
                env=env,
            ),
            Command(
                label="Figure 3 DuckDB Q21 density sweep",
                argv=(*sudo, PYTHON, str(yroot / "scripts" / "run_chiplet_ycsb_harness.py"), "--config", str(OUT_ROOT / "generated-configs" / "fig3_duckdb.json")),
                cwd=yroot,
                env=env,
            ),
        )

    if figure == "fig4":
        dimms = os.environ.get("MOTIVATION_DIMMS", "<set-MOTIVATION_DIMMS>")
        return (
            Command(
                label=f"Figure 4 DuckDB DIMM scaling point ({dimms} DIMMs)",
                argv=(*sudo, PYTHON, str(yroot / "scripts" / "run_chiplet_ycsb_harness.py"), "--config", str(OUT_ROOT / "generated-configs" / f"fig4_dimm{dimms}.json")),
                cwd=yroot,
                env=env,
                note="Physically configure exactly 1, 2, 4, or 12 DIMMs before each invocation.",
            ),
        )

    if figure == "fig5":
        configs = (
            "llamacpp.example.json",
            "chiplet_fixed_1perchiplet_npb_ft_class_b_single_thread.json",
            "chiplet_fixed_1perchiplet_npb_cg_class_b_single_thread.json",
            "chiplet_fixed_1perchiplet_npb_mg_class_b_single_thread.json",
        )
        config_args = tuple(value for name in configs for value in ("--config", str(yroot / "configs" / name)))
        return (
            Command(
                label="Figure 5 DIMM/CXL per-core bandwidth scaling",
                argv=(*sudo, PYTHON, str(yroot / "scripts" / "run_smt14_numa_backend_sweep.py"), *map(str, range(1, 15)), *config_args, "--numa", "0", "--numa", "1"),
                cwd=yroot,
                env=env,
            ),
        )

    if figure in {"fig6a", "fig6b", "fig6c", "fig6d"}:
        script = {
            "fig6a": "run_bw_ineq_cc_io.py",
            "fig6b": "run_bw_ineq_intra_io.py",
            "fig6c": "run_bw_ineq_io_dimm.py",
            "fig6d": "run_bw_ineq_io_cxl.py",
        }[figure]
        configs = ("llamacpp.example.json", "duckdb_tpch_sf1_single_thread_chiplet_density_q21.json")
        config_args = tuple(value for name in configs for value in ("--config", str(yroot / "configs" / name)))
        return (
            Command(
                label=f"Figure {figure[3:]} bandwidth-inequality sweep",
                argv=(*sudo, PYTHON, str(yroot / "scripts" / script), *config_args),
                cwd=yroot,
                env=env,
            ),
        )

    if figure == "fig8ab":
        return (
            Command(
                label="Figure 8a/b PageRank free/busy chiplet placement",
                argv=(PYTHON, str(exp / "exp1" / "run_exp1.py"), "--scenarios", "free-cores", "busy-cores", "--policies", "eevdf", "optimal", "same-chiplet", "--benchmarks", "pr", "--repeats", "5", "--graph-scale", "20", "--perf-sched-mode", "required", "--perf-sched-sudo", "--output-dir", str(output)),
                cwd=exp,
            ),
        )

    if figure == "fig8c":
        return (
            Command(
                label="Figure 8c PageRank placement versus I/O traffic",
                argv=(PYTHON, str(exp / "exp4" / "run_exp4.py"), "--benchmarks", "pr", "--placements", "same-core", "same-llc", "optimal", "--load-pcts", "0", "20", "40", "60", "80", "100", "--load-rate-overrides", "20:600", "40:300", "60:150", "80:100", "100:0", "--repeats", "1", "--graph-scale", "20", "--output-dir", str(output)),
                cwd=exp,
            ),
        )

    if figure == "fig8d":
        return (
            Command(
                label="Figure 8d PageRank memory-channel culprit experiment",
                argv=(PYTHON, str(exp / "exp_case3" / "run_exp_case3.py"), "--benchmarks", "pr", "--cases", "culprit", "related-inter", "irrelevant-within", "irrelevant-inter", "--load-pcts", "0", "25", "50", "75", "100", "--repeats", "5", "--graph-scale", "20", "--pr-iterations", "100", "--output-dir", str(output)),
                cwd=exp,
                topology="nps4-two-dimm-cxl",
                note="The harness also runs constant core21 -> NUMA1 traffic at rate 0.",
            ),
        )

    if figure == "fig8e":
        twitter = os.environ.get("MOTIVATION_TWITTER_GRAPH", "/home/seunghyun/gapbs/gapbs/benchmark/graphs/twitter.sg")
        return (
            Command(
                label="Figure 8e PageRank traffic-skew experiment",
                argv=(PYTHON, str(exp / "exp_case4-1" / "run_exp_case4_1.py"), "--benchmarks", "pr", "--policies", "eevdf", "optimal", "same-chiplet", "--skewnesses", "0", "25", "50", "75", "100", "--rotations", "0", "1", "2", "3", "--fixed-rotation", "2", "--repeats", "1", "--graph-file", twitter, "--pr-iterations", "100", "--noise-memory-nodes", "0", "--optimal-strict-pinning", "--same-chiplet-single-combo", "--output-dir", str(output)),
                cwd=exp,
            ),
        )

    if figure == "fig8f":
        return (
            Command(
                label="Figure 8f node-replication skiplist traffic-skew experiment",
                argv=(PYTHON, str(exp / "exp_case4-2" / "run_exp_case4_2.py"), "--benchmarks", "skiplist-rw50", "--policies", "eevdf", "optimal-pinned", "same-chiplet", "--skewnesses", "0", "25", "50", "75", "100", "--rotations", "0", "1", "2", "3", "--fixed-rotation", "2", "--repeats", "1", "--noise-memory-nodes", "0", "--same-chiplet-single-combo", "--nr-benchmark-duration-seconds", "5", "--node-replication-timeout-seconds", "45", "--nr-skiplist-initial-capacity", "4194304", "--nr-skiplist-key-space", "5000000", "--nr-skiplist-ops", "2500000", "--nr-skiplist-log-counts", "1", "--output-dir", str(output)),
                cwd=exp,
            ),
        )

    if figure == "fig8f-bw":
        return (
            Command(
                label="Figure 8f path-bandwidth table",
                argv=(PYTHON, str(exp / "exp3" / "run_exp3.py"), "--pairs", "intra", "inter", "--traffic-types", "none", "l3", "io", "full-load", "--iterations", "256", "--samples", "1", "--bench-id", "4", "--bandwidth-repeats", "10", "--fixed-payload-bytes", "262144", "--l3-threads", "5", "--l3-worker-memory-mb", "4", "--output-dir", str(output)),
                cwd=exp,
            ),
        )

    raise AssertionError(figure)


def prepare_configs(figure: str) -> None:
    if figure == "fig3":
        materialize_ycsb_config(REFERENCE / "ycsb" / "fig3_ycsb_resolved_config.json", "fig3_ycsb.json")
        materialize_ycsb_config(REFERENCE / "ycsb" / "fig3_duckdb_resolved_config.json", "fig3_duckdb.json")
    elif figure == "fig4":
        dimms = os.environ.get("MOTIVATION_DIMMS")
        if dimms not in {"1", "2", "4", "12"}:
            raise SystemExit("Figure 4 full run requires MOTIVATION_DIMMS=1, 2, 4, or 12")
        materialize_ycsb_config(
            ORIGINAL / "ycsb" / "config_old" / "numa0_prefix_duckdb_tpch_sf1_single_thread.json",
            f"fig4_dimm{dimms}.json",
            result_suffix=f"dimm{dimms}",
        )


def parse_numa_hardware() -> dict[int, dict[str, object]]:
    output = subprocess.run(["numactl", "-H"], check=True, capture_output=True, text=True).stdout
    nodes: dict[int, dict[str, object]] = {}
    for raw in output.splitlines():
        fields = raw.strip().replace(":", "").split()
        if len(fields) >= 3 and fields[0] == "node" and fields[2] == "cpus":
            node = int(fields[1])
            nodes.setdefault(node, {})["cpus"] = tuple(int(value) for value in fields[3:])
        elif len(fields) >= 4 and fields[0] == "node" and fields[2] == "size":
            node = int(fields[1])
            nodes.setdefault(node, {})["size_mb"] = int(fields[3])
    return nodes


def topology_summary(nodes: dict[int, dict[str, object]]) -> str:
    parts = []
    for node, values in sorted(nodes.items()):
        cpus = values.get("cpus", ())
        size = values.get("size_mb", 0)
        parts.append(f"node{node}:cpus={len(cpus)},memory={size}MB")
    return "; ".join(parts)


def validate_topology(profile: str) -> tuple[bool, str]:
    if profile in {"vendor-host", "any"}:
        return True, "vendor-specific topology"
    nodes = parse_numa_hardware()
    summary = topology_summary(nodes)
    if profile == "nps1-cxl":
        ok = (
            set(nodes) == {0, 1}
            and bool(nodes[0].get("cpus"))
            and not bool(nodes[1].get("cpus"))
            and int(nodes[0].get("size_mb", 0)) > 0
            and int(nodes[1].get("size_mb", 0)) > 0
        )
        return ok, summary
    if profile == "nps4-two-dimm-cxl":
        ok = (
            set(nodes) == {0, 1, 2, 3, 4}
            and all(bool(nodes[node].get("cpus")) for node in range(4))
            and not bool(nodes[4].get("cpus"))
            and int(nodes[1].get("size_mb", 0)) > 0
            and int(nodes[3].get("size_mb", 0)) > 0
            and int(nodes[4].get("size_mb", 0)) > 0
        )
        return ok, summary
    raise ValueError(f"unknown topology profile: {profile}")


def format_command(command: Command) -> str:
    env = " ".join(f"{key}={shlex.quote(value)}" for key, value in command.env)
    argv = shlex.join(command.argv)
    return f"{env} {argv}".strip()


def show_plan(figures: Iterable[str]) -> None:
    for figure in figures:
        print(f"\n[{figure}]")
        for command in command_plan(figure):
            print(f"  {command.label}")
            print(f"  topology: {command.topology}")
            print(f"  cwd: {command.cwd}")
            print(f"  command: {format_command(command)}")
            if command.note:
                print(f"  note: {command.note}")


def run_command(command: Command) -> None:
    ok, detail = validate_topology(command.topology)
    if not ok:
        raise SystemExit(f"topology check failed for {command.label}: expected {command.topology}; observed {detail}")
    command.cwd.mkdir(parents=True, exist_ok=True)
    env = os.environ.copy()
    env.update(dict(command.env))
    print(f"==> {command.label}")
    print(f"    topology: {detail}")
    print(f"    command: {format_command(command)}")
    subprocess.run(command.argv, cwd=command.cwd, env=env, check=True)


def write_fig8d_sidecar_config(path: Path, output_path: Path) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        "\n".join(
            (
                '<?xml version="1.0"?>',
                "<benchmark>",
                "  <nthreads>1</nthreads>",
                "  <memory>128m</memory>",
                "  <worker_memory_mb>256</worker_memory_mb>",
                f"  <output>{output_path}</output>",
                "  <bandwidth>true</bandwidth>",
                "  <time>86400</time>",
                "  <clock>2.25</clock>",
                "  <silent>true</silent>",
                "  <core>21</core>",
                "  <numa>1</numa>",
                "  <worker_numa_policy>bind</worker_numa_policy>",
                "  <worker_numa_interleave_nodes></worker_numa_interleave_nodes>",
                "  <rate>0</rate>",
                "  <Mode>0</Mode>",
                "  <thread_alloc>false</thread_alloc>",
                "  <alloc_core>987</alloc_core>",
                "  <alloc_numa>0</alloc_numa>",
                "  <alloc_rate>0</alloc_rate>",
                "  <alloc_Mode>3</alloc_Mode>",
                "</benchmark>",
                "",
            )
        ),
        encoding="utf-8",
    )


def run_fig8d(command: Command) -> None:
    ok, detail = validate_topology(command.topology)
    if not ok:
        raise SystemExit(f"topology check failed for Figure 8d: expected {command.topology}; observed {detail}")
    sidecar_dir = run_dir("fig8d") / "extra_constant_traffic"
    config = sidecar_dir / "core21_to_numa1_rate0.xml"
    output = sidecar_dir / "core21_to_numa1_rate0.txt"
    log_path = sidecar_dir / "core21_to_numa1_rate0.log"
    write_fig8d_sidecar_config(config, output)
    memory_benchmark = Path(
        os.environ.get("MOTIVATION_MEMORY_BENCHMARK", "/home/seunghyun/sched_bench/build/memory_benchmark")
    )
    with log_path.open("w", encoding="utf-8") as log_handle:
        sidecar = subprocess.Popen(
            [str(memory_benchmark), "--config", str(config)],
            stdout=log_handle,
            stderr=subprocess.STDOUT,
            text=True,
        )
        try:
            time.sleep(2)
            if sidecar.poll() is not None:
                raise RuntimeError(f"Figure 8d constant-traffic sidecar exited with {sidecar.returncode}")
            run_command(command)
        finally:
            sidecar.terminate()
            try:
                sidecar.wait(timeout=10)
            except subprocess.TimeoutExpired:
                sidecar.kill()
                sidecar.wait(timeout=5)


def run_full(figures: tuple[str, ...]) -> None:
    profiles = {command.topology for figure in figures for command in command_plan(figure)}
    hardware_profiles = profiles - {"vendor-host", "any"}
    if len(hardware_profiles) > 1:
        raise SystemExit(
            "the selected full run spans incompatible topology profiles; run NPS1 figures and Figure 8d separately"
        )
    if any(figure.startswith(("fig2", "fig3", "fig4", "fig5", "fig6")) for figure in figures):
        prepare_ycsb_worktree()
    for figure in figures:
        prepare_configs(figure)
        for command in command_plan(figure):
            if figure == "fig8d":
                run_fig8d(command)
            else:
                run_command(command)


def render_plots(figures: tuple[str, ...]) -> None:
    selected = [(figure, target) for figure in figures for target in PLOT_TARGETS.get(figure, ())]
    if not selected:
        raise SystemExit("no plot is defined for the selected figure")
    if shutil.which("gnuplot") is None:
        raise SystemExit("gnuplot is required")
    output_dir = OUT_ROOT / "figures"
    output_dir.mkdir(parents=True, exist_ok=True)
    work_parent = OUT_ROOT / "plot-work"
    work_parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="motivation-", dir=work_parent) as temp_raw:
        work = Path(temp_raw)
        shutil.copytree(PLOTS, work / "plots")
        for figure, (script_rel, generated_name, final_name) in selected:
            script = work / "plots" / script_rel
            print(f"==> render {figure}: {script_rel}")
            subprocess.run(["gnuplot", script.name], cwd=script.parent, check=True)
            generated = script.parent / generated_name
            if not generated.is_file() or generated.stat().st_size == 0:
                raise RuntimeError(f"gnuplot did not create {generated}")
            shutil.copy2(generated, output_dir / final_name)
            print(f"    {output_dir / final_name}")


def required_local_paths() -> tuple[Path, ...]:
    return (
        ORIGINAL / "ycsb" / "script_old" / "common.sh",
        ORIGINAL / "ycsb" / "scripts" / "run_chiplet_ycsb_harness.py",
        ORIGINAL / "ycsb" / "chiplet_harness" / "runner.py",
        ORIGINAL / "exp3" / "experiment_utils.py",
        ORIGINAL / "exp3" / "exp1" / "run_exp1.py",
        ORIGINAL / "exp3" / "exp_case3" / "run_exp_case3.py",
        REFERENCE / "ycsb" / "fig3_ycsb_resolved_config.json",
        REFERENCE / "exp3" / "fig8d" / "run_metadata.txt",
        PLOTS / "fig8" / "case2" / "data",
    )


def check_bundle(figures: tuple[str, ...]) -> int:
    failed = False
    for path in required_local_paths():
        ok = path.exists()
        print(f"{'ok' if ok else 'MISSING'} local {path.relative_to(ROOT)}")
        failed |= not ok
    for binary in ("python3", "gnuplot", "numactl", "lscpu"):
        path = shutil.which(binary)
        print(f"{'ok' if path else 'MISSING'} binary {binary}: {path or '-'}")
        failed |= path is None

    profiles = sorted({command.topology for figure in figures for command in command_plan(figure)})
    for profile in profiles:
        ok, detail = validate_topology(profile)
        print(f"{'ok' if ok else 'MISMATCH'} topology {profile}: {detail}")
        if len(profiles) == 1:
            failed |= not ok

    checksum_file = ROOT / "SHA256SUMS"
    if checksum_file.exists():
        result = subprocess.run(
            ["sha256sum", "--check", str(checksum_file)],
            cwd=ROOT,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
        )
        print("ok checksums" if result.returncode == 0 else result.stdout.rstrip())
        failed |= result.returncode != 0
    return 1 if failed else 0


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Reproduce cSwitch motivation Figures 2-6 and 8.")
    parser.add_argument("figure", help="Figure id, group (fig2/fig6/fig8), or all")
    parser.add_argument("action", choices=("list", "check", "dry-run", "plot", "full"))
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    figures = expand_figure(args.figure)
    if args.action == "list":
        for figure in figures:
            print(figure)
        return 0
    if args.action == "check":
        return check_bundle(figures)
    if args.action == "dry-run":
        show_plan(figures)
        return 0
    if args.action == "plot":
        render_plots(figures)
        return 0
    run_full(figures)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
