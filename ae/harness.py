#!/usr/bin/env python3

from __future__ import annotations

import argparse
import csv
import json
import os
import re
import shlex
import shutil
import signal
import statistics
import subprocess
import sys
import time
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


AE_ROOT = Path(__file__).resolve().parent
PACKAGE_ROOT = AE_ROOT.parent
SCHEDULER_ROOT = Path(
    os.environ.get("AE_SCHEDULER_ROOT", str(PACKAGE_ROOT))
).expanduser().resolve()
DEFAULT_OUT_ROOT = AE_ROOT / "results"
DEFAULT_CCM_MAPPING = AE_ROOT / "ccm_mapping.txt"


def environment_path(name: str, default: str | Path) -> Path:
    return Path(os.environ.get(name, str(default))).expanduser().resolve()


GAPBS_ROOT = environment_path("AE_GAPBS_ROOT", "/home/seunghyun/gapbs/gapbs")
GAPBS_GRAPH_ROOT = environment_path(
    "AE_GAPBS_GRAPH_ROOT", GAPBS_ROOT / "benchmark" / "graphs"
)
YCSB_ROOT = environment_path("AE_YCSB_ROOT", "/home/seunghyun/ycsb")
YCSB_RUNNER = environment_path(
    "AE_YCSB_RUNNER", YCSB_ROOT / "scripts" / "run_chiplet_ycsb_harness.py"
)
AE_EXTERNAL_RUNNER = AE_ROOT / "external_workload.py"
EEVDF_ROOT = environment_path("AE_EEVDF_ROOT", "/home/seunghyun/scx_rustland_eevdf")
EXP3_ROOT = environment_path("AE_EXP3_ROOT", "/home/seunghyun/exp3")
NODE_REPLICATION_ROOT = environment_path(
    "AE_NODE_REPLICATION_ROOT", EXP3_ROOT / "node-replication" / "node-replication"
)
LLAMA_ROOT = environment_path("AE_LLAMA_ROOT", YCSB_ROOT / "benchmarks" / "llama.cpp")
LLAMA_MODEL = environment_path(
    "AE_LLAMA_MODEL",
    "/home/seunghyun/llama.cpp/models/Meta-Llama-3.1-8B-Instruct-Q4_K_M.gguf",
)
BUILD_SCRATCH_ROOT = Path("/dev/shm/scx-ae-build")
DEFAULT_WORKLOAD_CPUS = "0-13,21-34"
DEFAULT_NOISE_CPUS = "0-3,21-24"
DEFAULT_FIG10_LOADED_WORKLOAD_CPUS = "0-4,7-11,21-25,28-32"
DEFAULT_FIG10_NOISE_CPUS = "5-6,12-13,26-27,33-34"
DEFAULT_FIG10_NOISE_RATE = 50
DEFAULT_FIG10_NOISE_DURATION_SEC = 86400
DEFAULT_PAPER_NOISE_DURATION_SEC = 86400
DEFAULT_FIG12_NOISE_DURATION_SEC = 3600
FIG11_WORKLOAD_CPUS = "0-13,21-34"
FIG12A_WORKLOAD_START_CPUS = "0-6"
FIG12A_WORKLOAD_FULL_CPUS = "0-13,21-34"
FIG12A_NOISE_CPUS = "0-4"
FIG12B_WORKLOAD_FULL_CPUS = "0-13,21-34,42-55,63-76"
FIG12B_WORKLOAD_START_CPUS = ("0-6", "7-13", "21-27", "28-34")
FIG12B_MANAGED_NOISE_CPUS = ("0-4", "7-11", "21-25", "28-32")
FIG12B_EXTERNAL_NOISE_CPUS = "14-15,35-36,56-57,77-78"
FIG12B_EXTERNAL_NOISE_RATE = 50
FIG13_WORKLOAD_CPUS = "0-13,21-34,42-55,63-76"
SUCCESS_STATUSES = {"ok", "salvaged"}
TRIAL_TIME_RE = re.compile(r"Trial Time:\s*([0-9]+(?:\.[0-9]+)?)")
AVERAGE_TIME_RE = re.compile(r"Average Time:\s*([0-9]+(?:\.[0-9]+)?)")
DF_CS_IDS = tuple(range(12))
FIG11_FREE_NOISE_CPUS = (
    0, 1, 2, 3,
    7, 8, 9, 10,
    21, 22, 23, 24,
    28, 29, 30, 31,
)
FIG11_FREE_NOISE_RATES = (
    50, 50, 50, 50,
    200, 200, 200, 200,
    2000, 2000, 2000, 2000,
    10000, 10000, 10000, 10000,
)
FIG11_BUSY_NOISE_CPUS = tuple(range(0, 7)) + tuple(range(7, 14)) + tuple(range(21, 28)) + tuple(range(28, 35))
FIG11_BUSY_NOISE_RATES = (
    50, 50, 50, 50, 50, 50, 50,
    200, 200, 200, 200, 200, 200, 200,
    2000, 2000, 2000, 2000, 2000, 2000, 2000,
    10000, 10000, 10000, 10000, 10000, 10000, 10000,
)
FIG13_LOADED_NOISE_CPUS = (
    tuple(range(0, 4))
    + tuple(range(7, 11))
    + tuple(range(21, 25))
    + tuple(range(28, 32))
    + (42, 49, 63, 70)
)
FIG13_LOADED_NOISE_RATES = tuple([50 for _ in range(16)] + [10000 for _ in range(4)])


@dataclass(frozen=True)
class VariantSpec:
    key: str
    label: str
    repo_root: Path | None
    binary_name: str | None
    feature: str | None = None
    runtime_args: tuple[str, ...] = ()


@dataclass(frozen=True)
class WorkloadSpec:
    key: str
    label: str
    kind: str
    metric_family: str
    metric_name: str
    higher_is_better: bool = False
    binary: Path | None = None
    backend: str = ""
    style: str = "28x1"
    ycsb_root: Path = YCSB_ROOT / "YCSB"
    workload_file: Path | None = None
    working_set_mb_per_instance: int | None = None
    primary_operation: str = "READ"
    operationcount: int | None = None
    load_threads: int | None = None
    java_opts: str = ""
    monitoring_family: str = "CS"
    monitoring_ids: tuple[int, ...] = DF_CS_IDS
    keep_result_db: bool = False
    backend_options: dict[str, str] = field(default_factory=dict)
    external_options: dict[str, Any] = field(default_factory=dict)


@dataclass(frozen=True)
class RunSpec:
    figure: str
    benchmark: str
    case: str
    variant: str
    threads: int
    repeat: int
    noise_rate: int | None = None

    @property
    def point_key(self) -> tuple[str, str, str, int, int | None]:
        return (self.benchmark, self.case, self.variant, self.threads, self.noise_rate)

    @property
    def run_id(self) -> str:
        parts = [
            self.benchmark,
            self.case,
            self.variant,
            f"t{self.threads:02d}",
        ]
        if self.noise_rate is not None:
            parts.append(f"rate{self.noise_rate:05d}")
        parts.append(f"r{self.repeat:02d}")
        return "__".join(parts)


@dataclass(frozen=True)
class NoiseProfile:
    label: str
    cpus: tuple[int, ...]
    rates: tuple[int, ...]
    duration_sec: int


VARIANTS: dict[str, VariantSpec] = {
    "la-default": VariantSpec(
        key="la-default",
        label="cSwitch legacy default",
        repo_root=SCHEDULER_ROOT,
        binary_name="scx-rustland-la",
    ),
    "paper-greedy": VariantSpec(
        key="paper-greedy",
        label="cSwitch",
        repo_root=SCHEDULER_ROOT,
        binary_name="scx-rustland-la",
        feature="scheduler-paper-greedy",
    ),
    "arcas": VariantSpec(
        key="arcas",
        label="ARCAS",
        repo_root=SCHEDULER_ROOT,
        binary_name="scx-rustland-la",
        feature="scheduler-arcas",
    ),
    "nsdi-delay-range": VariantSpec(
        key="nsdi-delay-range",
        label="Caladan+",
        repo_root=SCHEDULER_ROOT,
        binary_name="scx-rustland-la",
        feature="scheduler-nsdi",
        runtime_args=("--nsdi-policy", "delay-range"),
    ),
    "eevdf": VariantSpec(
        key="eevdf",
        label="Linux EEVDF",
        repo_root=EEVDF_ROOT,
        binary_name="scx-rustland-eevdf",
        runtime_args=("--base-slice-us", "700"),
    ),
    "cfs": VariantSpec(
        key="cfs",
        label="CFS",
        repo_root=None,
        binary_name=None,
    ),
}

VARIANT_ALIASES = {
    "cswitch": "paper-greedy",
    "la": "paper-greedy",
    "nsdi": "nsdi-delay-range",
    "caladan": "nsdi-delay-range",
    "caladan+": "nsdi-delay-range",
}

RESULT_VARIANT_LABELS = {
    "paper-greedy": "cSwitch",
    "la-default": "cSwitch (legacy)",
    "arcas": "ARCAS",
    "nsdi-delay-range": "Caladan+",
    "eevdf": "EEVDF",
    "cfs": "Linux CFS",
}


def ycsb_workload(name: str) -> Path:
    return YCSB_ROOT / "workloads" / name


def filebench_options(benchmark_class: str, *, template: Path | None = None, **overrides: str) -> dict[str, str]:
    options = {
        "filebench_root": str(YCSB_ROOT / "benchmarks" / "filebench"),
        "runtime": "10",
        "nthreads": "1",
        "nfiles": "1000",
        "meandirwidth": "20",
        "iosize": "1m",
        "meanappendsize": "16k",
        "benchmark_class": benchmark_class,
    }
    if template is not None:
        options["template"] = str(template)
    options.update(overrides)
    return options


def gapbs_external_options(kind: str, binary_name: str, *, trial_count: int = 16, iterations: int | None = None) -> dict[str, Any]:
    options: dict[str, Any] = {
        "family": "gapbs",
        "kind": kind,
        "root": str(GAPBS_ROOT),
        "binary_path": str(GAPBS_ROOT / binary_name),
        "trial_count": trial_count,
        "style_graphs": {
            "28x1": {"mode": "generated", "generator": "kronecker", "scale": 20, "class": "kronecker20"},
            "1x28": {"mode": "file", "path": str(GAPBS_GRAPH_ROOT / "twitter.sg"), "class": "twitter"},
        },
    }
    if iterations is not None:
        options["iterations"] = iterations
    return options


def node_replication_options(kind: str) -> dict[str, Any]:
    common = {
        "family": "node-replication",
        "kind": kind,
        "package_root": str(NODE_REPLICATION_ROOT),
        "manifest_path": str(NODE_REPLICATION_ROOT / "Cargo.toml"),
        "duration_seconds": 5,
        "timeout_seconds": 300,
        "initial_capacity": 1 << 22,
        "key_space": 5_000_000,
        "ops": 2_500_000,
    }
    if kind == "skiplist-rw50":
        return {
            **common,
            "cargo_bench_name": "lockfree",
            "binary_prefix": "lockfree",
            "log_counts": "1",
        }
    return {
        **common,
        "cargo_bench_name": "hashmap",
        "cargo_extra_args": ["--features", "cmp"],
        "binary_prefix": "hashmap",
    }


WORKLOADS: dict[str, WorkloadSpec] = {
    "ycsb_rocksdb_256mib": WorkloadSpec(
        key="ycsb_rocksdb_256mib",
        label="YCSB RocksDB 256MiB",
        kind="chiplet-harness",
        metric_family="throughput",
        metric_name="throughput_ops_per_sec",
        higher_is_better=True,
        backend="rocksdb",
        workload_file=ycsb_workload("working_set_256mb.properties"),
        working_set_mb_per_instance=256,
        operationcount=1_000_000,
        keep_result_db=True,
    ),
    "ycsb_orientdb_256mib": WorkloadSpec(
        key="ycsb_orientdb_256mib",
        label="YCSB OrientDB 256MiB",
        kind="chiplet-harness",
        metric_family="throughput",
        metric_name="throughput_ops_per_sec",
        higher_is_better=True,
        backend="orientdb",
        workload_file=ycsb_workload("working_set_256mb.properties"),
        working_set_mb_per_instance=256,
        operationcount=10_000,
    ),
    "ycsb_elasticsearch_256mib_lvalue": WorkloadSpec(
        key="ycsb_elasticsearch_256mib_lvalue",
        label="YCSB Elasticsearch 256MiB LValue",
        kind="chiplet-harness",
        metric_family="throughput",
        metric_name="throughput_ops_per_sec",
        higher_is_better=True,
        backend="elasticsearch",
        workload_file=ycsb_workload("working_set_256mb_large_value.properties"),
        working_set_mb_per_instance=256,
        operationcount=100,
        load_threads=1,
        java_opts="-Xms2g -Xmx2g",
        monitoring_family="CCM",
        monitoring_ids=(0,),
    ),
    "gapbs_bc_kron20_twitter": WorkloadSpec(
        key="gapbs_bc_kron20_twitter",
        label="GAPBS BC kron20/twitter",
        kind="external",
        metric_family="time",
        metric_name="benchmark_time_s",
        backend="gapbs_bc",
        binary=GAPBS_ROOT / "bc",
        external_options=gapbs_external_options("bc", "bc", trial_count=16, iterations=4),
    ),
    "gapbs_pr_kron20": WorkloadSpec(
        key="gapbs_pr_kron20",
        label="GAPBS PR kron20",
        kind="direct-gapbs-pr",
        metric_family="time",
        metric_name="benchmark_time_s",
        binary=GAPBS_ROOT / "pr",
        backend="gapbs_pr",
        external_options=gapbs_external_options("pr", "pr", trial_count=16, iterations=4),
    ),
    "llamacpp_llama31_8b": WorkloadSpec(
        key="llamacpp_llama31_8b",
        label="llama.cpp Llama 3.1 8B",
        kind="chiplet-harness",
        metric_family="rate",
        metric_name="gen_tokens_per_s",
        higher_is_better=True,
        backend="llamacpp",
        monitoring_family="CCM",
        monitoring_ids=(0,),
        backend_options={
            "llama_root": str(LLAMA_ROOT),
            "model": str(LLAMA_MODEL),
            "n_prompt": "8",
            "n_gen": "8",
            "batch_size": "128",
            "ubatch_size": "128",
            "repetitions": "1",
            "n_gpu_layers": "0",
            "numa": "numactl",
            "benchmark_class": "Meta-Llama-3.1-8B-Instruct-Q4_K_M.gguf",
        },
    ),
    "node_replication_skiplist_rw50": WorkloadSpec(
        key="node_replication_skiplist_rw50",
        label="node-replication skiplist rw50",
        kind="external",
        metric_family="rate",
        metric_name="benchmark_rate",
        higher_is_better=True,
        backend="node_replication_skiplist_rw50",
        style="1x28",
        external_options=node_replication_options("skiplist-rw50"),
    ),
    "node_replication_rwlock_rw50": WorkloadSpec(
        key="node_replication_rwlock_rw50",
        label="node-replication rwlock rw50",
        kind="external",
        metric_family="rate",
        metric_name="benchmark_rate",
        higher_is_better=True,
        backend="node_replication_rwlock_rw50",
        style="1x28",
        external_options=node_replication_options("rwlock-rw50"),
    ),
    "filebench_fileserver": WorkloadSpec(
        key="filebench_fileserver",
        label="Filebench fileserver",
        kind="chiplet-harness",
        metric_family="throughput",
        metric_name="throughput_ops_per_sec",
        higher_is_better=True,
        backend="filebench_fileserver",
        ycsb_root=YCSB_ROOT,
        backend_options=filebench_options("fileserver"),
    ),
    "filebench_webproxy": WorkloadSpec(
        key="filebench_webproxy",
        label="Filebench webproxy",
        kind="chiplet-harness",
        metric_family="throughput",
        metric_name="throughput_ops_per_sec",
        higher_is_better=True,
        backend="filebench_fileserver",
        ycsb_root=YCSB_ROOT,
        backend_options=filebench_options(
            "webproxy",
            template=AE_ROOT / "templates" / "filebench_webproxy_10s.f",
            nfiles="10000",
            meandirwidth="1000000",
            filesize="16k",
        ),
    ),
    "filebench_webserver": WorkloadSpec(
        key="filebench_webserver",
        label="Filebench webserver",
        kind="chiplet-harness",
        metric_family="throughput",
        metric_name="throughput_ops_per_sec",
        higher_is_better=True,
        backend="filebench_fileserver",
        ycsb_root=YCSB_ROOT,
        backend_options=filebench_options(
            "webserver",
            template=AE_ROOT / "templates" / "filebench_webserver_10s.f",
        ),
    ),
    "filebench_varmail": WorkloadSpec(
        key="filebench_varmail",
        label="Filebench varmail",
        kind="chiplet-harness",
        metric_family="throughput",
        metric_name="throughput_ops_per_sec",
        higher_is_better=True,
        backend="filebench_fileserver",
        ycsb_root=YCSB_ROOT,
        backend_options=filebench_options(
            "varmail",
            template=AE_ROOT / "templates" / "filebench_varmail_10s.f",
            meandirwidth="1000000",
        ),
    ),
}

FIG10_WORKLOADS = [
    "ycsb_rocksdb_256mib",
    "ycsb_orientdb_256mib",
    "ycsb_elasticsearch_256mib_lvalue",
    "gapbs_bc_kron20_twitter",
    "gapbs_pr_kron20",
    "llamacpp_llama31_8b",
    "node_replication_skiplist_rw50",
    "node_replication_rwlock_rw50",
    "filebench_fileserver",
    "filebench_webproxy",
    "filebench_webserver",
    "filebench_varmail",
]
FIG11_WORKLOADS = [
    "llamacpp_llama31_8b",
    "gapbs_pr_kron20",
    "filebench_fileserver",
]
FIG12_WORKLOADS = ["gapbs_pr_kron20"]
FIG13_WORKLOADS = ["gapbs_pr_kron20"]
PAPER_VARIANTS = ["paper-greedy", "arcas", "eevdf", "nsdi-delay-range"]
# Zero is an AE sentinel for a true no-noise baseline. memory_benchmark itself
# interprets rate=0 as maximum traffic, so the zero point must not launch it.
FIG12_NOISE_RATES = [0, 50, 100, 500, 1000, 5000, 10000]
FIG13_THREAD_COUNTS = [1, 2, 4, 6, 8, 10, 12, 14, 16, 18]


def log(message: str) -> None:
    stamp = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    print(f"[{stamp}] {message}", flush=True)


def now_utc_iso() -> str:
    return datetime.now(timezone.utc).isoformat()


def ensure_dir(path: Path) -> Path:
    path.mkdir(parents=True, exist_ok=True)
    return path


def write_json(path: Path, payload: MappingLike) -> None:
    ensure_dir(path.parent)
    path.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8")


MappingLike = dict[str, Any]


def read_json(path: Path) -> MappingLike:
    return json.loads(path.read_text(encoding="utf-8"))


def fmt_command(command: list[str]) -> str:
    return shlex.join(str(item) for item in command)


def parse_csv_words(value: str | None, default: list[str]) -> list[str]:
    if value is None or value.strip() == "":
        return list(default)
    value = value.replace(",", " ")
    return [item for item in value.split() if item]


def parse_int_words(value: str | None, default: list[int]) -> list[int]:
    return [int(item) for item in parse_csv_words(value, [str(item) for item in default])]


def normalize_variant_key(key: str) -> str:
    normalized = VARIANT_ALIASES.get(key, key)
    if normalized not in VARIANTS:
        raise SystemExit(f"unknown variant {key}; choices: {', '.join(sorted(VARIANTS))}")
    return normalized


def result_variant_label(key: Any) -> str:
    value = str(key or "")
    return RESULT_VARIANT_LABELS.get(value, value)


def parse_cpu_list(text: str) -> list[int]:
    cpus: list[int] = []
    for part in text.split(","):
        part = part.strip()
        if not part:
            continue
        if "-" in part:
            start_text, end_text = part.split("-", 1)
            cpus.extend(range(int(start_text), int(end_text) + 1))
        else:
            cpus.append(int(part))
    return sorted(dict.fromkeys(cpus))


def format_cpu_list(cpus: list[int]) -> str:
    if not cpus:
        return ""
    items = sorted(dict.fromkeys(cpus))
    ranges: list[str] = []
    start = prev = items[0]
    for cpu in items[1:]:
        if cpu == prev + 1:
            prev = cpu
            continue
        ranges.append(f"{start}-{prev}" if start != prev else str(start))
        start = prev = cpu
    ranges.append(f"{start}-{prev}" if start != prev else str(start))
    return ",".join(ranges)


def cpus_from_mask(mask: str) -> tuple[int, ...]:
    return tuple(parse_cpu_list(mask))


def format_cpu_tuple(cpus: tuple[int, ...]) -> str:
    return format_cpu_list(list(cpus))


def workload_cpu_text(spec: RunSpec, args: argparse.Namespace) -> str:
    if spec.figure == "fig10" and spec.case != "clean":
        return args.fig10_loaded_workload_cpus
    if spec.figure == "fig11":
        return FIG11_WORKLOAD_CPUS
    if spec.figure == "fig12a":
        return FIG12A_WORKLOAD_FULL_CPUS
    if spec.figure == "fig12b":
        return FIG12B_WORKLOAD_FULL_CPUS
    if spec.figure == "fig13":
        return FIG13_WORKLOAD_CPUS
    return args.workload_cpus


def workload_cpus(spec: RunSpec, args: argparse.Namespace) -> list[int]:
    return parse_cpu_list(workload_cpu_text(spec, args))


def generic_noise_profile(spec: RunSpec, args: argparse.Namespace) -> NoiseProfile | None:
    if spec.figure == "fig10" and spec.case != "clean":
        cpus = cpus_from_mask(args.fig10_noise_cpus)
        return NoiseProfile(
            label="fig10-load75-sidecar",
            cpus=cpus,
            rates=tuple(args.fig10_noise_rate for _ in cpus),
            duration_sec=args.fig10_noise_duration_sec,
        )
    if spec.figure == "fig11":
        if spec.case == "free-cores":
            return NoiseProfile(
                label="fig11-free-cores-asymmetric-sidecar",
                cpus=FIG11_FREE_NOISE_CPUS,
                rates=FIG11_FREE_NOISE_RATES,
                duration_sec=args.paper_noise_duration_sec,
            )
        if spec.case == "busy-cores":
            return NoiseProfile(
                label="fig11-busy-cores-asymmetric-sidecar",
                cpus=FIG11_BUSY_NOISE_CPUS,
                rates=FIG11_BUSY_NOISE_RATES,
                duration_sec=args.paper_noise_duration_sec,
            )
    if spec.figure == "fig13" and spec.case == "loaded":
        return NoiseProfile(
            label="fig13-loaded-sidecar",
            cpus=FIG13_LOADED_NOISE_CPUS,
            rates=FIG13_LOADED_NOISE_RATES,
            duration_sec=args.paper_noise_duration_sec,
        )
    return None


def fig12a_noise_profile(spec: RunSpec, args: argparse.Namespace) -> NoiseProfile:
    cpus = cpus_from_mask(FIG12A_NOISE_CPUS)
    rate = spec.noise_rate if spec.noise_rate is not None else args.default_noise_rate
    return NoiseProfile(
        label="fig12a-cc-io-managed-noise",
        cpus=cpus,
        rates=tuple(rate for _ in cpus),
        duration_sec=args.fig12_noise_duration_sec,
    )


def fig12b_managed_noise_profiles(spec: RunSpec, args: argparse.Namespace) -> list[NoiseProfile]:
    rate = spec.noise_rate if spec.noise_rate is not None else args.default_noise_rate
    profiles: list[NoiseProfile] = []
    for index, mask in enumerate(FIG12B_MANAGED_NOISE_CPUS):
        cpus = cpus_from_mask(mask)
        profiles.append(
            NoiseProfile(
                label=f"fig12b-managed-noise-ccd{index}",
                cpus=cpus,
                rates=tuple(rate for _ in cpus),
                duration_sec=args.fig12_noise_duration_sec,
            )
        )
    return profiles


def fig12b_external_noise_profile(args: argparse.Namespace) -> NoiseProfile:
    cpus = cpus_from_mask(FIG12B_EXTERNAL_NOISE_CPUS)
    return NoiseProfile(
        label="fig12b-persistent-external-sidecar",
        cpus=cpus,
        rates=tuple(FIG12B_EXTERNAL_NOISE_RATE for _ in cpus),
        duration_sec=args.paper_noise_duration_sec,
    )


def has_noise(spec: RunSpec, args: argparse.Namespace) -> bool:
    if generic_noise_profile(spec, args) is not None:
        return True
    if spec.figure in {"fig12a", "fig12b"}:
        return spec.noise_rate != 0
    return False


def noise_cpu_text(spec: RunSpec, args: argparse.Namespace) -> str:
    profile = generic_noise_profile(spec, args)
    if profile is not None:
        return format_cpu_tuple(profile.cpus)
    if spec.figure == "fig12a":
        return FIG12A_NOISE_CPUS
    if spec.figure == "fig12b":
        managed = ";".join(FIG12B_MANAGED_NOISE_CPUS)
        return f"managed:{managed} external:{FIG12B_EXTERNAL_NOISE_CPUS}"
    return ""


def noise_rates_text(spec: RunSpec, args: argparse.Namespace) -> str:
    profile = generic_noise_profile(spec, args)
    if profile is not None:
        return ",".join(str(rate) for rate in profile.rates)
    if spec.figure == "fig12a":
        return ",".join(str(rate) for rate in fig12a_noise_profile(spec, args).rates)
    if spec.figure == "fig12b":
        managed = ";".join(",".join(str(rate) for rate in profile.rates) for profile in fig12b_managed_noise_profiles(spec, args))
        external = ",".join(str(rate) for rate in fig12b_external_noise_profile(args).rates)
        return f"managed:{managed} external:{external}"
    return ""


def noise_duration_sec(spec: RunSpec, args: argparse.Namespace) -> int:
    profile = generic_noise_profile(spec, args)
    if profile is not None:
        return profile.duration_sec
    if spec.figure in {"fig12a", "fig12b"}:
        return args.fig12_noise_duration_sec
    return args.noise_duration_sec


def default_noise_rate(spec: RunSpec, args: argparse.Namespace) -> int:
    if spec.figure == "fig10" and spec.case != "clean":
        return args.fig10_noise_rate
    return args.default_noise_rate


def paper_condition_label(spec: RunSpec) -> str:
    if spec.figure == "fig10":
        return "I/O chiplet unloaded" if spec.case == "clean" else "I/O chiplet loaded"
    if spec.figure == "fig11":
        return "heterogeneous free cores" if spec.case == "free-cores" else "heterogeneous busy cores"
    if spec.figure == "fig12a":
        return "varying compute-I/O chiplet link load"
    if spec.figure == "fig12b":
        return "varying overall I/O chiplet load"
    if spec.figure == "fig13":
        return "PageRank scaling without chiplet load" if spec.case == "clean" else "PageRank scaling with chiplet load"
    return spec.case


def workload_style(spec: RunSpec) -> str:
    return WORKLOADS[spec.benchmark].style


def workload_effective_style(spec: RunSpec, args: argparse.Namespace) -> str:
    count = len(workload_cpus(spec, args))
    if workload_style(spec) == "1x28":
        return f"1x{count}"
    return f"{count}x1"


def first_existing(paths: list[Path]) -> Path:
    for path in paths:
        if path.exists():
            return path
    return paths[0]


def memory_benchmark_path() -> Path:
    env_path = os.environ.get("AE_MEMORY_BENCHMARK")
    if env_path:
        return Path(env_path).expanduser().resolve()
    candidates = [
        YCSB_ROOT / "memory_benchmark",
        Path("/home/seunghyun/sched_bench/build/memory_benchmark"),
    ]
    return first_existing(candidates)


def output_profile(profile: str, *, light_build: bool, variant: VariantSpec) -> str:
    if light_build and variant.repo_root == SCHEDULER_ROOT:
        return f"{profile}-light"
    return profile


def built_binary_path(out_root: Path, variant_key: str, profile: str, *, light_build: bool) -> Path:
    variant = VARIANTS[variant_key]
    if variant.binary_name is None:
        raise ValueError(f"variant {variant_key} has no scheduler binary")
    return out_root / "build" / variant_key / output_profile(profile, light_build=light_build, variant=variant) / variant.binary_name


def is_local_scheduler_variant(variant_key: str) -> bool:
    return VARIANTS[variant_key].repo_root == SCHEDULER_ROOT


def build_variant(variant_key: str, args: argparse.Namespace) -> Path | None:
    variant = VARIANTS[variant_key]
    if variant.binary_name is None:
        return None
    assert variant.repo_root is not None
    output_path = built_binary_path(args.out_root, variant_key, args.profile, light_build=args.light_build)
    if args.mode == "dry-run":
        return output_path
    if args.skip_build:
        if output_path.exists():
            return output_path
        raise RuntimeError(f"--skip-build requested but missing {output_path}")

    scratch_dir = BUILD_SCRATCH_ROOT / variant_key / output_path.parent.name
    if scratch_dir.exists():
        shutil.rmtree(scratch_dir)
    ensure_dir(scratch_dir)
    command = ["cargo", "build", "--bin", variant.binary_name, "--target-dir", str(scratch_dir)]
    if args.profile == "release":
        command.append("--release")
    if args.light_build and variant.repo_root == SCHEDULER_ROOT:
        features = ["tick-resched"]
        if variant.feature:
            features.append(variant.feature)
        command.append("--no-default-features")
        command.extend(["--features", ",".join(features)])
    elif variant.feature:
        command.extend(["--features", variant.feature])

    build_log = args.out_root / f"build_{variant_key}.log"
    log(f"building {variant_key}: {fmt_command(command)}")
    result = run_capture(command, cwd=variant.repo_root, stdout_path=build_log, stderr_path=build_log, timeout_sec=args.build_timeout_sec)
    if result["returncode"] != 0:
        raise RuntimeError(f"build failed for {variant_key}; see {build_log}")

    profile_dir = "release" if args.profile == "release" else "debug"
    built = scratch_dir / profile_dir / variant.binary_name
    if not built.exists():
        raise RuntimeError(f"expected built binary at {built}")
    ensure_dir(output_path.parent)
    shutil.copy2(built, output_path)
    return output_path


def terminate_process_group(proc: subprocess.Popen[Any]) -> None:
    try:
        os.killpg(proc.pid, signal.SIGTERM)
    except ProcessLookupError:
        return
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        try:
            os.killpg(proc.pid, signal.SIGKILL)
        except ProcessLookupError:
            return
        proc.wait(timeout=5)


def run_capture(
    command: list[str],
    *,
    cwd: Path,
    stdout_path: Path,
    stderr_path: Path,
    timeout_sec: int,
) -> MappingLike:
    ensure_dir(stdout_path.parent)
    same_log = stdout_path == stderr_path
    start = time.monotonic()
    with stdout_path.open("ab") as stdout_handle:
        stderr_handle_obj = stdout_handle if same_log else stderr_path.open("ab")
        try:
            proc = subprocess.Popen(
                command,
                cwd=cwd,
                stdout=stdout_handle,
                stderr=stderr_handle_obj,
                start_new_session=True,
            )
            child_pgid = proc.pid
            timed_out = False
            try:
                returncode = proc.wait(timeout=timeout_sec)
            except subprocess.TimeoutExpired:
                timed_out = True
                terminate_process_group(proc)
                returncode = None
            except KeyboardInterrupt:
                terminate_process_group(proc)
                raise
        finally:
            if not same_log:
                stderr_handle_obj.close()
    return {
        "returncode": returncode,
        "timed_out": timed_out,
        "wall_s": round(time.monotonic() - start, 6),
        "child_pgid": child_pgid,
    }


def cleanup_processes_for_run(run_dir: Path, run_id: str, *, use_sudo: bool) -> None:
    cleanup_tokens = (
        "scx-rustland-la",
        "scx-rustland-eevdf",
        "run_chiplet_ycsb_harness.py",
        "external_workload.py",
        "memory_benchmark",
        "site.ycsb.Client",
        "gapbs/gapbs",
        "filebench",
        "llama-bench",
        "node-replication",
        "ycsb.sh",
        "/usr/bin/java",
    )
    result = subprocess.run(
        ["ps", "-eo", "pid,pgid,cmd"],
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        check=False,
    )
    own_pid = os.getpid()
    own_pgid = os.getpgrp()
    pgids: set[int] = set()
    run_dir_text = str(run_dir)
    for line in result.stdout.splitlines()[1:]:
        fields = line.strip().split(None, 2)
        if len(fields) < 3:
            continue
        try:
            pid = int(fields[0])
            pgid = int(fields[1])
        except ValueError:
            continue
        command_text = fields[2]
        if pid == own_pid or pgid == own_pgid:
            continue
        if not any(token in command_text for token in cleanup_tokens):
            continue
        if run_dir_text not in command_text and run_id not in command_text:
            continue
        pgids.add(pgid)
    if not pgids:
        return
    log(f"cleanup lingering processes for {run_id}: pgids={sorted(pgids)}")
    for signal_name in ("TERM", "KILL"):
        for pgid in sorted(pgids):
            command = ["kill", f"-{signal_name}", "--", f"-{pgid}"]
            if use_sudo:
                command = ["sudo", "-n", *command]
            subprocess.run(command, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False)
        if signal_name == "TERM":
            time.sleep(2)


def remove_tree_best_effort(path: Path, *, use_sudo: bool) -> bool:
    shutil.rmtree(path, ignore_errors=True)
    if path.exists() and use_sudo:
        subprocess.run(["sudo", "-n", "rm", "-rf", str(path)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    return not path.exists()


def prune_heavy_workload_artifacts(run_dir: Path, *, keep: bool, use_sudo: bool) -> list[str]:
    if keep:
        return []
    removed: list[str] = []
    harness_root = run_dir / "harness_results"
    if not harness_root.exists():
        return removed
    for path in sorted(harness_root.glob("*/runs/*/workload")):
        if not path.is_dir():
            continue
        if remove_tree_best_effort(path, use_sudo=use_sudo):
            removed.append(str(path))
        else:
            log(f"warning: could not prune workload artifacts for {run_dir.name}: {path}")
    if removed:
        log(f"pruned reproducible workload artifacts for {run_dir.name}: count={len(removed)}")
    return removed


def write_memory_benchmark_config(path: Path, *, cpus: list[int], rates: list[int], duration_sec: int, output_path: Path) -> None:
    if len(cpus) != len(rates):
        raise ValueError("memory_benchmark cpus/rates length mismatch")
    cores = ",".join(str(cpu) for cpu in cpus)
    zeros = ",".join("0" for _ in cpus)
    rate_text = ",".join(str(rate) for rate in rates)
    text = f"""<?xml version="1.0"?>
<benchmark>
  <nthreads>{len(cpus)}</nthreads>
  <memory>128m</memory>
  <worker_memory_mb>256</worker_memory_mb>
  <output>{output_path}</output>
  <bandwidth>true</bandwidth>
  <time>{duration_sec}</time>
  <clock>2.25</clock>
  <silent>true</silent>
  <core>{cores}</core>
  <numa>{zeros}</numa>
  <rate>{rate_text}</rate>
  <Mode>{zeros}</Mode>
  <thread_alloc>false</thread_alloc>
  <alloc_core>{cpus[0] if cpus else 0}</alloc_core>
  <alloc_numa>0</alloc_numa>
  <alloc_rate>0</alloc_rate>
  <alloc_Mode>3</alloc_Mode>
</benchmark>
"""
    ensure_dir(path.parent)
    path.write_text(text, encoding="utf-8")


def write_chiplet_harness_config(spec: RunSpec, args: argparse.Namespace, run_dir: Path) -> Path:
    workload = WORKLOADS[spec.benchmark]
    cpus = workload_cpus(spec, args)
    backend_options = dict(workload.backend_options)
    threads_per_instance = 1
    java_active_processor_count = 1
    if spec.figure == "fig11":
        if workload.backend == "llamacpp":
            threads_per_instance = spec.threads
        elif workload.backend == "filebench_fileserver":
            backend_options["nthreads"] = str(spec.threads)
    assignment_metadata: MappingLike = {}
    if spec.figure == "fig11":
        assignment_metadata = {
            "instance_count": 1,
            "shared_cpu_selector": workload_cpu_text(spec, args),
        }
    backend: MappingLike = {
        "name": workload.backend,
        "operationcount": workload.operationcount,
        "threads_per_instance": threads_per_instance,
        "java_active_processor_count": java_active_processor_count,
        "load_threads": workload.load_threads,
        "java_opts": workload.java_opts,
        "options": backend_options,
        "env": {},
        "load_props": {},
        "run_props": {},
    }
    payload: MappingLike = {
        "ycsb_root": str(workload.ycsb_root),
        "result_root": str(run_dir / "harness_results"),
        "result_prefix": spec.run_id,
        "keep_result_db": workload.keep_result_db,
        "fail_fast": True,
        "workload": None,
        "monitoring": {
            "event_name": "NO_RETIRED_INST_CYCLES",
            "perf_event": "cpu/event=0xc0,cmask=1,inv=1/",
            "df_enabled": True,
            "df_resource_family": workload.monitoring_family,
            "df_resource_ids": list(workload.monitoring_ids),
            "df_sample_slot_ms": 20,
        },
        "background_noise": None,
        "backends": [backend],
        "assignments": [
            {
                "label": "workload",
                "cores": cpus,
                "numas": [0 for _ in cpus],
                "metadata": assignment_metadata,
            }
        ],
    }
    if workload.workload_file is not None:
        payload["workload"] = {
            "file": str(workload.workload_file),
            "working_set_mb_per_instance": workload.working_set_mb_per_instance,
            "primary_operation": workload.primary_operation,
            "hdr_percentiles": [50, 95, 99, 99.9, 99.99],
        }
    config_path = run_dir / "workload_config.json"
    write_json(config_path, payload)
    return config_path


def write_external_workload_config(spec: RunSpec, args: argparse.Namespace, run_dir: Path) -> Path:
    workload = WORKLOADS[spec.benchmark]
    cpus = workload_cpus(spec, args)
    style = workload_style(spec)
    single_instance = style == "1x28"
    payload: MappingLike = {
        "runner_kind": "external",
        "benchmark": spec.benchmark,
        "benchmark_display": workload.label,
        "backend": workload.backend,
        "metric_family": workload.metric_family,
        "style": style,
        "effective_style": workload_effective_style(spec, args),
        "state": "load75" if spec.case == "loaded" else spec.case,
        "single_instance": single_instance,
        "instance_count": 1 if single_instance else len(cpus),
        "effective_threads": len(cpus) if single_instance else 1,
        "workload_cpus": cpus,
        "workload_cpu_mask": format_cpu_list(cpus),
        "shared_workload_cpu_mask": format_cpu_list(cpus) if single_instance else None,
        "result_root": str(run_dir / "harness_results"),
        "result_prefix": spec.run_id,
        "runner_options": dict(workload.external_options),
    }
    if spec.benchmark == "gapbs_pr_kron20":
        payload["runner_kind"] = "external"
    config_path = run_dir / "workload_config.json"
    write_json(config_path, payload)
    return config_path


def direct_gapbs_pr_args(spec: RunSpec, args: argparse.Namespace) -> list[str]:
    if spec.figure == "fig11":
        return ["-g", "20", "-i", "100", "-n", "1"]
    if spec.figure in {"fig12a", "fig12b"}:
        return ["-g", "20"]
    return ["-g", "20", "-n", str(args.gapbs_iterations)]


def workload_command(spec: RunSpec, args: argparse.Namespace, run_dir: Path) -> list[str]:
    workload = WORKLOADS[spec.benchmark]
    if spec.benchmark == "gapbs_pr_kron20" and spec.figure != "fig10":
        assert workload.binary is not None
        return [
            "env",
            f"OMP_NUM_THREADS={spec.threads}",
            "taskset",
            "-c",
            workload_cpu_text(spec, args),
            str(workload.binary),
            *direct_gapbs_pr_args(spec, args),
        ]
    if workload.kind == "chiplet-harness":
        config_path = write_chiplet_harness_config(spec, args, run_dir)
        return [sys.executable, str(YCSB_RUNNER), "--config", str(config_path)]
    if workload.kind == "external" or spec.benchmark == "gapbs_pr_kron20":
        config_path = write_external_workload_config(spec, args, run_dir)
        return [sys.executable, str(AE_EXTERNAL_RUNNER), "--config", str(config_path)]
    raise ValueError(f"unsupported workload {spec.benchmark}")


def payload_shell(spec: RunSpec, args: argparse.Namespace, run_dir: Path, *, noise_enabled: bool) -> str:
    if spec.figure == "fig12a":
        return fig12a_payload_shell(spec, args, run_dir)
    if spec.figure == "fig12b":
        return fig12b_payload_shell(spec, args, run_dir)
    stdout_path = run_dir / "workload.stdout.log"
    stderr_path = run_dir / "workload.stderr.log"
    settle = f"sleep {args.noise_settle_sec:g}; " if noise_enabled and args.noise_settle_sec > 0 else ""
    exec_prefix = "" if noise_enabled else "exec "
    return f"{settle}{exec_prefix}{fmt_command(workload_command(spec, args, run_dir))} > {shlex.quote(str(stdout_path))} 2> {shlex.quote(str(stderr_path))}"


def fig12a_payload_shell(spec: RunSpec, args: argparse.Namespace, run_dir: Path) -> str:
    noise_launch = ""
    if has_noise(spec, args):
        profile = fig12a_noise_profile(spec, args)
        noise_config = run_dir / "memory_benchmark.xml"
        noise_shell_text = memory_benchmark_shell(
            profile,
            noise_config,
            run_dir / "noise.output.txt",
            run_dir / "noise.log",
        )
        noise_launch = f"{noise_shell_text} &\nnoise_pid=$!"
    workload_stdout = run_dir / "workload.stdout.log"
    workload_stderr = run_dir / "workload.stderr.log"
    assert WORKLOADS["gapbs_pr_kron20"].binary is not None
    workload_command_text = fmt_command([str(WORKLOADS["gapbs_pr_kron20"].binary), *direct_gapbs_pr_args(spec, args)])
    return f"""
set -euo pipefail
workload_pid=
noise_pid=
cleanup() {{
  if [[ -n "${{noise_pid:-}}" ]]; then kill -TERM "$noise_pid" 2>/dev/null || true; wait "$noise_pid" 2>/dev/null || true; fi
  if [[ -n "${{workload_pid:-}}" ]]; then kill -TERM "$workload_pid" 2>/dev/null || true; wait "$workload_pid" 2>/dev/null || true; fi
}}
trap cleanup EXIT
: > {shlex.quote(str(workload_stdout))}
: > {shlex.quote(str(workload_stderr))}
env OMP_NUM_THREADS=1 taskset -c {shlex.quote(FIG12A_WORKLOAD_START_CPUS)} {workload_command_text} > {shlex.quote(str(workload_stdout))} 2> {shlex.quote(str(workload_stderr))} &
workload_pid=$!
sleep {args.fig12_scope_widen_delay_sec:g}
taskset -pc {shlex.quote(FIG12A_WORKLOAD_FULL_CPUS)} "$workload_pid" >/dev/null 2>&1 || true
{noise_launch}
wait "$workload_pid"
status=$?
cleanup
trap - EXIT
exit "$status"
""".strip()


def fig12b_payload_shell(spec: RunSpec, args: argparse.Namespace, run_dir: Path) -> str:
    assert WORKLOADS["gapbs_pr_kron20"].binary is not None
    workload_command_text = fmt_command([str(WORKLOADS["gapbs_pr_kron20"].binary), *direct_gapbs_pr_args(spec, args)])
    lines = [
        "set -euo pipefail",
        "workload_pids=()",
        "noise_pids=()",
        "cleanup() {",
        "  for pid in \"${noise_pids[@]:-}\"; do kill -TERM \"$pid\" 2>/dev/null || true; wait \"$pid\" 2>/dev/null || true; done",
        "  for pid in \"${workload_pids[@]:-}\"; do kill -TERM \"$pid\" 2>/dev/null || true; wait \"$pid\" 2>/dev/null || true; done",
        "}",
        "trap cleanup EXIT",
    ]
    for index, start_mask in enumerate(FIG12B_WORKLOAD_START_CPUS):
        stdout_path = run_dir / f"pagerank-pr-{index}.stdout.log"
        stderr_path = run_dir / f"pagerank-pr-{index}.stderr.log"
        lines.extend(
            [
                f": > {shlex.quote(str(stdout_path))}",
                f": > {shlex.quote(str(stderr_path))}",
                (
                    f"env OMP_NUM_THREADS=1 taskset -c {shlex.quote(start_mask)} {workload_command_text} "
                    f"> {shlex.quote(str(stdout_path))} 2> {shlex.quote(str(stderr_path))} &"
                ),
                "workload_pids+=(\"$!\")",
            ]
        )
    lines.append(f"sleep {args.fig12_scope_widen_delay_sec:g}")
    lines.extend(
        [
            "for pid in \"${workload_pids[@]}\"; do",
            f"  taskset -pc {shlex.quote(FIG12B_WORKLOAD_FULL_CPUS)} \"$pid\" >/dev/null 2>&1 || true",
            "done",
        ]
    )
    if has_noise(spec, args):
        for index, profile in enumerate(fig12b_managed_noise_profiles(spec, args)):
            noise_shell_text = memory_benchmark_shell(
                profile,
                run_dir / f"managed-noise-{index}.xml",
                run_dir / f"managed-noise-{index}.output.txt",
                run_dir / f"managed-noise-{index}.log",
            )
            lines.extend([f"{noise_shell_text} &", "noise_pids+=(\"$!\")"])
    lines.extend(
        [
            "status=0",
            "for pid in \"${workload_pids[@]}\"; do",
            "  if ! wait \"$pid\"; then status=1; fi",
            "done",
            "cleanup",
            "trap - EXIT",
            "exit \"$status\"",
        ]
    )
    return "\n".join(lines)


def memory_benchmark_shell(profile: NoiseProfile, config_path: Path, output_path: Path, log_path: Path) -> str:
    write_memory_benchmark_config(
        config_path,
        cpus=list(profile.cpus),
        rates=list(profile.rates),
        duration_sec=profile.duration_sec,
        output_path=output_path,
    )
    command = [
        "timeout",
        "--signal=TERM",
        "--kill-after=5s",
        f"{profile.duration_sec + 30}s",
        str(memory_benchmark_path()),
        "--config",
        str(config_path),
    ]
    return f"{fmt_command(command)} > {shlex.quote(str(log_path))} 2>&1"


def noise_shell(spec: RunSpec, run_dir: Path, args: argparse.Namespace) -> tuple[str, Path]:
    profile = generic_noise_profile(spec, args)
    if profile is None:
        raise ValueError(f"{spec.figure}/{spec.case} does not use generic spawn-shell noise")
    config_path = run_dir / "memory_benchmark.xml"
    output_path = run_dir / "noise.output.txt"
    log_path = run_dir / "noise.log"
    shell = memory_benchmark_shell(profile, config_path, output_path, log_path)
    return shell, config_path


def cfs_wrapper_shell(payload: str, noise: str | None) -> str:
    if noise is None:
        return payload
    return (
        "set -euo pipefail; "
        f"{noise} & noise_pid=$!; "
        "cleanup() { kill -TERM \"$noise_pid\" 2>/dev/null || true; wait \"$noise_pid\" 2>/dev/null || true; }; "
        "trap cleanup EXIT; "
        f"{payload}; status=$?; cleanup; trap - EXIT; exit $status"
    )


def wrap_command_with_external_noise(command: list[str], spec: RunSpec, args: argparse.Namespace, run_dir: Path) -> tuple[list[str], str]:
    profile = fig12b_external_noise_profile(args)
    config_path = run_dir / "external-memory-benchmark.xml"
    shell = memory_benchmark_shell(
        profile,
        config_path,
        run_dir / "external-noise.output.txt",
        run_dir / "external-noise.log",
    )
    wrapped = (
        "set -uo pipefail; "
        f"{shell} & external_noise_pid=$!; "
        "cleanup() { kill -TERM \"$external_noise_pid\" 2>/dev/null || true; wait \"$external_noise_pid\" 2>/dev/null || true; }; "
        "trap cleanup EXIT; "
        f"{fmt_command(command)}; status=$?; "
        "cleanup; trap - EXIT; exit $status"
    )
    return ["/bin/bash", "--noprofile", "--norc", "-c", wrapped], str(config_path)


def scheduler_command(
    spec: RunSpec,
    args: argparse.Namespace,
    run_dir: Path,
    binary_paths: dict[str, Path | None],
) -> tuple[list[str], str | None]:
    variant = VARIANTS[spec.variant]
    noise_enabled = has_noise(spec, args)
    noise_cmd: str | None = None
    noise_config_path: Path | None = None
    generic_profile = generic_noise_profile(spec, args)
    if generic_profile is not None:
        noise_cmd, noise_config_path = noise_shell(spec, run_dir, args)
    elif spec.figure == "fig12a" and noise_enabled:
        noise_config_path = run_dir / "memory_benchmark.xml"
    elif spec.figure == "fig12b" and noise_enabled:
        noise_config_path = run_dir / "external-memory-benchmark.xml"
    payload = payload_shell(spec, args, run_dir, noise_enabled=noise_enabled)
    if noise_cmd is not None:
        payload = cfs_wrapper_shell(payload, noise_cmd)
        noise_cmd = None

    if spec.variant == "cfs":
        command = ["/bin/bash", "--noprofile", "--norc", "-c", cfs_wrapper_shell(payload, noise_cmd)]
        if spec.figure == "fig12b" and noise_enabled:
            command, external_config_path = wrap_command_with_external_noise(command, spec, args, run_dir)
            return command, f"external:{external_config_path}"
        return command, str(noise_config_path or "")

    binary = binary_paths[spec.variant]
    if binary is None:
        raise RuntimeError(f"missing scheduler binary for {spec.variant}")
    command: list[str] = [str(binary)]
    if spec.variant == "eevdf":
        command.extend(["--ccm-mapping-path", str(args.ccm_mapping_path), "--restrict-mapped-cpus", "true"])
    else:
        cgroup_name = "scx-ae-" + re.sub(r"[^a-zA-Z0-9_.-]+", "-", spec.run_id)[:180]
        command.extend(
            [
                "--cgroup-path",
                str(Path("/sys/fs/cgroup") / cgroup_name),
                "--ccm-mapping-path",
                str(args.ccm_mapping_path),
                "--restrict-mapped-cpus",
                "true",
                "--guard-clean-cgroup",
                "false",
                "--managed-cpu-max",
                str(args.managed_cpu_max),
                "--primary-smt-only",
                "true",
            ]
        )
    command.extend(variant.runtime_args)
    if is_local_scheduler_variant(spec.variant):
        command.extend(
            [
                "--signature-snapshots",
                "true",
                "--cs-villain-throttle",
                args.cs_villain_throttle,
                "--tick-reeval-every",
                str(args.tick_reeval_every),
                "--tick-defer-max",
                str(args.tick_defer_max),
                "--cs-villain-reslice-us",
                str(args.cs_villain_reslice_us),
                "--cs-villain-refill-divisor",
                str(args.cs_villain_refill_divisor),
                "--cs-villain-settle-ms",
                str(args.cs_villain_settle_ms),
                "--cs-villain-release-samples",
                str(args.cs_villain_release_samples),
            ]
        )
    if args.scheduler_logs:
        command.extend(["--decision-log-path", str(run_dir / "decision.log")])
        command.extend(["--runtime-log-path", str(run_dir / "runtime.log")])
        if args.monitor_sec > 0:
            command.extend(["--monitor", f"{args.monitor_sec:g}"])
    if noise_cmd and spec.variant != "cfs":
        command.extend(["--spawn-shell", noise_cmd])
    command.extend(["--", "/bin/bash", "--noprofile", "--norc", "-c", payload])
    if args.use_sudo:
        command = ["sudo", "-n", *command]
    if spec.figure == "fig12b" and noise_enabled:
        command, external_config_path = wrap_command_with_external_noise(command, spec, args, run_dir)
        return command, f"external:{external_config_path}"
    return command, str(noise_config_path or "")


def optional_float(value: Any) -> float | None:
    if value in (None, ""):
        return None
    try:
        return float(value)
    except (TypeError, ValueError):
        return None


def read_first_tsv_row(path: Path | None) -> dict[str, str] | None:
    if path is None or not path.exists():
        return None
    with path.open(newline="", encoding="utf-8") as handle:
        rows = list(csv.DictReader(handle, delimiter="\t"))
    if not rows:
        return None
    return rows[0]


def find_group_summary(run_dir: Path) -> Path | None:
    matches = sorted((run_dir / "harness_results").rglob("group_summary.tsv"))
    if not matches:
        return None
    return matches[-1]


def parse_instance_summary_fallback(group_summary_path: Path | None) -> MappingLike:
    if group_summary_path is None:
        return {}
    instance_path = group_summary_path.with_name("instance_summary.tsv")
    if not instance_path.exists():
        return {}
    with instance_path.open(newline="", encoding="utf-8") as handle:
        rows = list(csv.DictReader(handle, delimiter="\t"))
    benchmark_times: list[float] = []
    benchmark_rates: list[float] = []
    throughput_values: list[float] = []
    benchmark_rate_unit = ""
    for row in rows:
        if (value := optional_float(row.get("benchmark_time_s"))) is not None:
            benchmark_times.append(value)
        if (value := optional_float(row.get("benchmark_rate"))) is not None:
            benchmark_rates.append(value)
        if (value := optional_float(row.get("throughput_ops_per_sec"))) is not None:
            throughput_values.append(value)
        if row.get("benchmark_rate_unit"):
            benchmark_rate_unit = row["benchmark_rate_unit"]
    return {
        "fallback_benchmark_time_s": max(benchmark_times) if benchmark_times else None,
        "fallback_benchmark_rate": sum(benchmark_rates) if benchmark_rates else None,
        "fallback_benchmark_rate_unit": benchmark_rate_unit,
        "fallback_throughput_ops_per_sec": sum(throughput_values) if throughput_values else None,
    }


def parse_direct_gapbs_metric(stdout_path: Path) -> MappingLike:
    if not stdout_path.exists():
        return {"metric_value": None, "metric_error": "missing workload stdout"}
    text = stdout_path.read_text(encoding="utf-8", errors="replace")
    average_matches = AVERAGE_TIME_RE.findall(text)
    if average_matches:
        return {"metric_value": float(average_matches[-1]), "metric_error": "", "metric_unit": "s"}
    trial_matches = TRIAL_TIME_RE.findall(text)
    if trial_matches:
        return {"metric_value": float(trial_matches[-1]), "metric_error": "", "metric_unit": "s"}
    return {"metric_value": None, "metric_error": "could not parse Trial Time/Average Time"}


def parse_fig12b_gapbs_metric(run_dir: Path) -> MappingLike:
    values: list[float] = []
    paths = sorted(run_dir.glob("pagerank-pr-*.stdout.log"))
    for path in paths:
        parsed = parse_direct_gapbs_metric(path)
        value = optional_float(parsed.get("metric_value"))
        if value is not None:
            values.append(value)
    if not values:
        return {"metric_value": None, "metric_error": "could not parse Fig12b PageRank instance logs"}
    return {
        "metric_value": statistics.fmean(values),
        "metric_error": "",
        "metric_unit": "s",
        "fig12b_instance_time_aggregation": "mean",
        "fig12b_instance_times_s": values,
        "fig12b_instance_logs": [str(path) for path in paths],
    }


def parse_llama_bench_output(output: str) -> dict[str, float | None]:
    prompt_tokens_per_s: float | None = None
    gen_tokens_per_s: float | None = None
    total_tokens = 0
    total_time_ns = 0.0
    for raw_line in output.splitlines():
        line = raw_line.strip()
        if not line.startswith("{") or not line.endswith("}"):
            continue
        try:
            payload = json.loads(line)
        except json.JSONDecodeError:
            continue
        if not isinstance(payload, dict):
            continue
        try:
            avg_ns = float(payload.get("avg_ns"))
            avg_ts = float(payload.get("avg_ts"))
            n_prompt = int(payload.get("n_prompt") or 0)
            n_gen = int(payload.get("n_gen") or 0)
        except (TypeError, ValueError):
            continue
        total_tokens += n_prompt + n_gen
        total_time_ns += avg_ns
        if n_prompt > 0 and n_gen == 0:
            prompt_tokens_per_s = avg_ts
        elif n_gen > 0 and n_prompt == 0:
            gen_tokens_per_s = avg_ts
    if total_tokens <= 0 or total_time_ns <= 0.0:
        return {"prompt_tokens_per_s": None, "gen_tokens_per_s": None, "total_tokens_per_s": None}
    return {
        "prompt_tokens_per_s": prompt_tokens_per_s,
        "gen_tokens_per_s": gen_tokens_per_s,
        "total_tokens_per_s": float(total_tokens) / (total_time_ns / 1_000_000_000.0),
    }


def parse_llama_metric(run_dir: Path) -> MappingLike:
    prompt_values: list[float] = []
    gen_values: list[float] = []
    total_values: list[float] = []
    log_paths = sorted((run_dir / "harness_results").rglob("run.stdout.log"))
    for path in log_paths:
        parsed = parse_llama_bench_output(path.read_text(encoding="utf-8", errors="replace"))
        if (value := optional_float(parsed.get("prompt_tokens_per_s"))) is not None:
            prompt_values.append(value)
        if (value := optional_float(parsed.get("gen_tokens_per_s"))) is not None:
            gen_values.append(value)
        if (value := optional_float(parsed.get("total_tokens_per_s"))) is not None:
            total_values.append(value)
    if gen_values:
        return {
            "metric_value": sum(gen_values),
            "metric_unit": "tok/s",
            "metric_error": "",
            "prompt_tokens_per_s": sum(prompt_values) if prompt_values else None,
            "gen_tokens_per_s": sum(gen_values),
            "total_tokens_per_s": sum(total_values) if total_values else None,
            "fallback_log_paths": [str(path) for path in log_paths],
        }
    return {"metric_value": None, "metric_error": "could not parse llama-bench generation token rows"}


def parse_metric(run_dir: Path, spec: RunSpec) -> MappingLike:
    workload = WORKLOADS[spec.benchmark]
    if spec.figure == "fig12b" and spec.benchmark == "gapbs_pr_kron20":
        return parse_fig12b_gapbs_metric(run_dir)
    if spec.benchmark == "gapbs_pr_kron20" and spec.figure != "fig10":
        return parse_direct_gapbs_metric(run_dir / "workload.stdout.log")

    group_summary_path = find_group_summary(run_dir)
    group_row = read_first_tsv_row(group_summary_path)
    harness_payload: MappingLike = {
        "group_summary_path": "" if group_summary_path is None else str(group_summary_path),
        "harness_result_dir": "" if group_summary_path is None else str(group_summary_path.parent),
    }
    if spec.benchmark.startswith("llamacpp_"):
        llama_payload = parse_llama_metric(run_dir)
        llama_payload.update(harness_payload)
        if llama_payload.get("metric_value") is not None:
            return llama_payload

    if group_row is None:
        return {**harness_payload, "metric_value": None, "metric_error": "group_summary.tsv not found or empty"}

    fallback = parse_instance_summary_fallback(group_summary_path)
    benchmark_time = optional_float(group_row.get("benchmark_time_s"))
    if benchmark_time is None:
        benchmark_time = optional_float(fallback.get("fallback_benchmark_time_s"))
    benchmark_rate = optional_float(group_row.get("benchmark_rate"))
    if benchmark_rate is None:
        benchmark_rate = optional_float(fallback.get("fallback_benchmark_rate"))
    throughput = optional_float(group_row.get("aggregate_wall_throughput_ops_per_sec"))
    if throughput is None:
        throughput = optional_float(group_row.get("throughput_ops_per_sec"))
    if throughput is None:
        throughput = optional_float(fallback.get("fallback_throughput_ops_per_sec"))

    value: float | None
    unit: str
    if workload.metric_family == "throughput":
        value = throughput if throughput is not None else benchmark_rate
        unit = "ops/s"
    elif workload.metric_family == "rate":
        value = benchmark_rate if benchmark_rate is not None else throughput
        unit = group_row.get("benchmark_rate_unit") or str(fallback.get("fallback_benchmark_rate_unit") or "ops/s")
    else:
        value = benchmark_time
        unit = "s"

    if value is None:
        return {
            **harness_payload,
            "metric_value": None,
            "metric_error": "group_summary.tsv did not contain the selected primary metric",
            "group_status": group_row.get("status", ""),
        }
    return {
        **harness_payload,
        "metric_value": value,
        "metric_unit": unit,
        "metric_error": "",
        "benchmark_time_s": benchmark_time,
        "benchmark_rate": benchmark_rate,
        "benchmark_rate_unit": group_row.get("benchmark_rate_unit", ""),
        "throughput_ops_per_sec": throughput,
        "group_status": group_row.get("status", ""),
    }


def default_variants(figure: str, mode: str, light: bool) -> list[str]:
    if mode == "smoke":
        return ["paper-greedy"]
    if figure in {"fig10", "fig11", "fig12a", "fig12b", "fig13"}:
        return list(PAPER_VARIANTS)
    return list(PAPER_VARIANTS)


def default_benchmarks(figure: str, mode: str) -> list[str]:
    if figure == "fig10" and mode != "smoke":
        return list(FIG10_WORKLOADS)
    if figure == "fig11" and mode != "smoke":
        return list(FIG11_WORKLOADS)
    if figure in {"fig12a", "fig12b"}:
        return list(FIG12_WORKLOADS)
    if figure == "fig13":
        return list(FIG13_WORKLOADS)
    return ["gapbs_pr_kron20"]


def selected_values(args: argparse.Namespace, name: str, default: list[str]) -> list[str]:
    return parse_csv_words(getattr(args, name), default)


def attempt_numbers(args: argparse.Namespace) -> list[int]:
    if args.mode == "dry-run":
        return [1]
    return list(range(1, args.max_attempts + 1))


def target_successes(args: argparse.Namespace) -> int:
    if args.mode in {"dry-run", "smoke"}:
        return 1
    return args.repeats


def point_label(spec: RunSpec) -> str:
    rate = "" if spec.noise_rate is None else f" rate={spec.noise_rate}"
    return (
        f"{spec.figure} benchmark={spec.benchmark} case={spec.case} "
        f"variant={spec.variant} threads={spec.threads}{rate}"
    )


def run_specs_for(args: argparse.Namespace, figure: str) -> list[RunSpec]:
    light = args.light
    benchmarks = selected_values(args, "benchmarks", default_benchmarks(figure, args.mode))
    for bench in benchmarks:
        if bench not in WORKLOADS:
            raise SystemExit(f"unsupported AE workload {bench}; choices: {', '.join(sorted(WORKLOADS))}")
    variants = [normalize_variant_key(item) for item in selected_values(args, "variants", default_variants(figure, args.mode, light))]
    attempts = attempt_numbers(args)
    specs: list[RunSpec] = []

    if figure == "fig10":
        cases = selected_values(args, "cases", ["clean"] if args.mode == "smoke" else ["clean", "loaded"])
        for bench in benchmarks:
            for case in cases:
                for variant in variants:
                    for attempt in attempts:
                        specs.append(RunSpec(figure, bench, case, variant, args.fig10_threads, attempt))
    elif figure == "fig11":
        cases = selected_values(args, "cases", ["free-cores"] if args.mode == "smoke" else ["free-cores", "busy-cores"])
        for bench in benchmarks:
            for case in cases:
                if case not in {"free-cores", "busy-cores"}:
                    raise SystemExit("fig11 cases must be free-cores and/or busy-cores")
                for variant in variants:
                    for attempt in attempts:
                        specs.append(RunSpec(figure, bench, case, variant, args.fig11_threads, attempt))
    elif figure in {"fig12a", "fig12b"}:
        default_case = "cc-io-load" if figure == "fig12a" else "io-chiplet-load"
        cases = selected_values(args, "cases", [default_case])
        rates = parse_int_words(args.noise_rates, [1000] if args.mode == "smoke" else FIG12_NOISE_RATES)
        for bench in benchmarks:
            for case in cases:
                for rate in rates:
                    for variant in variants:
                        for attempt in attempts:
                            specs.append(RunSpec(figure, bench, case, variant, args.fig12_threads, attempt, noise_rate=rate))
    elif figure == "fig13":
        cases = selected_values(args, "cases", ["clean"] if args.mode == "smoke" else ["clean", "loaded"])
        cores = parse_int_words(args.cores, [1] if args.mode == "smoke" else FIG13_THREAD_COUNTS)
        for bench in benchmarks:
            for case in cases:
                for cores_value in cores:
                    for variant in variants:
                        for attempt in attempts:
                            specs.append(RunSpec(figure, bench, case, variant, cores_value, attempt))
    else:
        raise SystemExit(f"unknown figure {figure}")
    return specs


def execute_spec(spec: RunSpec, args: argparse.Namespace, binary_paths: dict[str, Path | None]) -> MappingLike:
    out_dir = args.out_root / spec.figure
    run_dir = out_dir / "runs" / spec.run_id
    run_dir_existed = run_dir.exists()
    if args.force and run_dir.exists():
        shutil.rmtree(run_dir)
    ensure_dir(run_dir)
    status_path = run_dir / "status.json"
    if run_dir_existed and not status_path.exists():
        cleanup_processes_for_run(run_dir, spec.run_id, use_sudo=args.use_sudo)
    if status_path.exists() and not args.force:
        status = read_json(status_path)
        existing_status = str(status.get("status", ""))
        if args.mode == "dry-run":
            log(f"skip existing {spec.figure} {spec.run_id}")
            return status
        if existing_status in SUCCESS_STATUSES:
            log(f"skip existing {spec.figure} {spec.run_id}")
            return status
        if existing_status != "dry-run":
            log(f"preserve existing failed attempt {spec.figure} {spec.run_id}")
            cleanup_processes_for_run(run_dir, spec.run_id, use_sudo=args.use_sudo)
            return status

    command, noise_config_path = scheduler_command(spec, args, run_dir, binary_paths)
    command_path = run_dir / "command.txt"
    command_path.write_text(fmt_command(command) + "\n", encoding="utf-8")
    config_payload: MappingLike = {
        "figure": spec.figure,
        "benchmark": spec.benchmark,
        "benchmark_label": WORKLOADS[spec.benchmark].label,
        "case": spec.case,
        "variant": spec.variant,
        "threads": spec.threads,
        "style": workload_style(spec),
        "effective_style": workload_effective_style(spec, args),
        "repeat": spec.repeat,
        "attempt": spec.repeat,
        "target_successes": target_successes(args),
        "max_attempts": args.max_attempts,
        "noise_rate": spec.noise_rate,
        "command": command,
        "command_path": str(command_path),
        "workload_config_path": str(run_dir / "workload_config.json") if (run_dir / "workload_config.json").exists() else "",
        "workload_cpus": workload_cpu_text(spec, args),
        "noise_cpus": noise_cpu_text(spec, args) if has_noise(spec, args) else "",
        "noise_rates": noise_rates_text(spec, args) if has_noise(spec, args) else "",
        "noise_config_path": noise_config_path,
        "paper_condition": paper_condition_label(spec),
        "scheduler_logs": args.scheduler_logs,
        "light_build": args.light_build,
        "cs_villain_throttle": args.cs_villain_throttle,
        "tick_reeval_every": args.tick_reeval_every,
        "tick_defer_max": args.tick_defer_max,
        "cs_villain_reslice_us": args.cs_villain_reslice_us,
        "cs_villain_refill_divisor": args.cs_villain_refill_divisor,
        "cs_villain_settle_ms": args.cs_villain_settle_ms,
        "cs_villain_release_samples": args.cs_villain_release_samples,
        "generated_at": now_utc_iso(),
    }
    write_json(run_dir / "config.json", config_payload)

    if args.mode == "dry-run":
        payload = {
            **config_payload,
            "status": "dry-run",
            "run_dir": str(run_dir),
        }
        write_json(status_path, payload)
        log(f"dry-run {spec.figure} {spec.run_id}: {fmt_command(command)}")
        return payload

    log(f"run {spec.figure} {spec.run_id}")
    result = run_capture(
        command,
        cwd=PACKAGE_ROOT,
        stdout_path=run_dir / "launcher.stdout.log",
        stderr_path=run_dir / "launcher.stderr.log",
        timeout_sec=args.timeout_sec,
    )
    metric_payload = parse_metric(run_dir, spec)
    metric_value = optional_float(metric_payload.get("metric_value"))
    metric_error = str(metric_payload.get("metric_error", ""))
    if result["returncode"] == 0 and not result["timed_out"] and metric_value is not None:
        status = "ok"
    elif not result["timed_out"] and metric_value is not None:
        status = "salvaged"
    else:
        status = "failed"
    payload = {
        **config_payload,
        **metric_payload,
        "status": status,
        "returncode": result["returncode"],
        "timed_out": result["timed_out"],
        "wall_s": result["wall_s"],
        "child_pgid": result.get("child_pgid"),
        "metric_name": WORKLOADS[spec.benchmark].metric_name,
        "metric_value": metric_value,
        "higher_is_better": WORKLOADS[spec.benchmark].higher_is_better,
        "metric_error": metric_error,
        "run_dir": str(run_dir),
        "workload_stdout_log": str(run_dir / "workload.stdout.log"),
        "workload_stderr_log": str(run_dir / "workload.stderr.log"),
        "launcher_stdout_log": str(run_dir / "launcher.stdout.log"),
        "launcher_stderr_log": str(run_dir / "launcher.stderr.log"),
        "cleanup_complete": False,
        "pruned_workload_artifacts": [],
        "finished_at": now_utc_iso(),
    }
    write_json(status_path, payload)
    cleanup_processes_for_run(run_dir, spec.run_id, use_sudo=args.use_sudo)
    payload["pruned_workload_artifacts"] = prune_heavy_workload_artifacts(
        run_dir,
        keep=args.keep_heavy_workload_artifacts,
        use_sudo=args.use_sudo,
    )
    payload["cleanup_complete"] = True
    payload["cleanup_finished_at"] = now_utc_iso()
    write_json(status_path, payload)
    return payload


def load_statuses(out_dir: Path) -> list[MappingLike]:
    statuses: list[MappingLike] = []
    for path in sorted((out_dir / "runs").glob("*/status.json")):
        try:
            payload = read_json(path)
        except json.JSONDecodeError:
            continue
        payload["_status_path"] = str(path)
        statuses.append(payload)
    return statuses


def median(values: list[float]) -> float | None:
    if not values:
        return None
    return float(statistics.median(values))


def summarize_figure(figure: str, args: argparse.Namespace) -> None:
    out_dir = args.out_root / figure
    results_dir = ensure_dir(out_dir / "results")
    statuses = load_statuses(out_dir)
    raw_fields = [
        "figure",
        "benchmark",
        "benchmark_label",
        "case",
        "variant",
        "style",
        "effective_style",
        "threads",
        "noise_rate",
        "noise_rates",
        "paper_condition",
        "repeat",
        "attempt",
        "target_successes",
        "max_attempts",
        "status",
        "metric_name",
        "metric_value",
        "metric_unit",
        "higher_is_better",
        "group_status",
        "group_summary_path",
        "harness_result_dir",
        "wall_s",
        "run_dir",
        "_status_path",
    ]
    with (results_dir / "raw_results.tsv").open("w", encoding="utf-8", newline="") as handle:
        writer = csv.DictWriter(handle, fieldnames=raw_fields, delimiter="\t", extrasaction="ignore")
        writer.writeheader()
        for status in statuses:
            output_row = dict(status)
            output_row["variant"] = result_variant_label(status.get("variant"))
            writer.writerow(output_row)

    grouped: dict[tuple[str, str, str, int, int], list[MappingLike]] = {}
    for status in statuses:
        if str(status.get("status", "")) not in SUCCESS_STATUSES:
            continue
        rate = int(status.get("noise_rate") or 0)
        key = (
            str(status.get("benchmark", "")),
            str(status.get("case", "")),
            str(status.get("variant", "")),
            int(status.get("threads", 0)),
            rate,
        )
        grouped.setdefault(key, []).append(status)

    baseline_by_point: dict[tuple[str, str, int, int], float] = {}
    for key, items in grouped.items():
        bench, case, variant, threads, rate = key
        if variant != "eevdf":
            continue
        values = [float(item["metric_value"]) for item in items if isinstance(item.get("metric_value"), (int, float))]
        value = median(values)
        if value is not None:
            baseline_by_point[(bench, case, threads, rate)] = value

    aggregate_rows: list[MappingLike] = []
    for key in sorted(grouped):
        bench, case, variant, threads, rate = key
        items = grouped[key]
        values = [float(item["metric_value"]) for item in items if isinstance(item.get("metric_value"), (int, float))]
        med = median(values)
        baseline = baseline_by_point.get((bench, case, threads, rate))
        higher_is_better = WORKLOADS.get(bench, WorkloadSpec(bench, bench, "unknown", "time", "metric")).higher_is_better
        if baseline is None or med in (None, 0.0) or baseline == 0.0:
            speedup = None
        elif higher_is_better:
            speedup = med / baseline
        else:
            speedup = baseline / med
        aggregate_rows.append(
            {
                "figure": figure,
                "benchmark": bench,
                "benchmark_label": WORKLOADS[bench].label if bench in WORKLOADS else bench,
                "case": case,
                "variant": result_variant_label(variant),
                "threads": threads,
                "noise_rate": rate if figure in {"fig12a", "fig12b"} else ("" if rate == 0 else rate),
                "success_count": len(items),
                "median_metric_value": "" if med is None else f"{med:.6f}",
                "speedup_vs_eevdf": "" if speedup is None else f"{speedup:.6f}",
            }
        )

    agg_fields = [
        "figure",
        "benchmark",
        "benchmark_label",
        "case",
        "variant",
        "threads",
        "noise_rate",
        "success_count",
        "median_metric_value",
        "speedup_vs_eevdf",
    ]
    with (results_dir / "aggregate_results.tsv").open("w", encoding="utf-8", newline="") as handle:
        writer = csv.DictWriter(handle, fieldnames=agg_fields, delimiter="\t")
        writer.writeheader()
        writer.writerows(aggregate_rows)
    if figure == "fig10":
        with (results_dir / "results_matrix.csv").open("w", encoding="utf-8", newline="") as handle:
            writer = csv.DictWriter(handle, fieldnames=agg_fields)
            writer.writeheader()
            writer.writerows(aggregate_rows)
    write_summary_md(results_dir / "summary.md", figure, aggregate_rows, statuses)


def write_summary_md(path: Path, figure: str, aggregate_rows: list[MappingLike], statuses: list[MappingLike]) -> None:
    lines = [
        f"# {figure} AE Harness Summary",
        "",
        "- harness: ae/harness.py",
        "- eval/exp* runners: not used",
        "- scheduler decision/runtime logs: off unless --scheduler-logs was passed",
        f"- status files: {len(statuses)}",
        "",
        "| benchmark | case | threads | rate | variant | successes | median time (s) | speedup vs EEVDF |",
        "| :-------- | :--- | ------: | ---: | :------ | --------: | --------------: | ---------------: |",
    ]
    for row in aggregate_rows:
        lines.append(
            "| {benchmark} | {case} | {threads} | {noise_rate} | {variant} | {success_count} | {median_metric_value} | {speedup_vs_eevdf} |".format(
                **row
            )
        )
    path.write_text("\n".join(lines).rstrip() + "\n", encoding="utf-8")


def run_figure(figure: str, args: argparse.Namespace) -> None:
    specs = run_specs_for(args, figure)
    ensure_dir(args.out_root / figure)
    selected_variants = sorted({spec.variant for spec in specs if spec.variant != "cfs"})
    binary_paths = {variant: build_variant(variant, args) for variant in selected_variants}
    required_successes = target_successes(args)
    grouped: dict[tuple[str, str, str, int, int | None], list[RunSpec]] = {}
    for spec in specs:
        grouped.setdefault(spec.point_key, []).append(spec)
    log(
        f"prepared {len(specs)} AE attempt slots for {figure} "
        f"({len(grouped)} points, target_successes={required_successes}, max_attempts={len(attempt_numbers(args))})"
    )
    for point_specs in grouped.values():
        successes = 0
        for spec in point_specs:
            if successes >= required_successes:
                log(f"skip remaining attempts for {point_label(spec)}; target successes reached")
                break
            status = execute_spec(spec, args, binary_paths)
            if str(status.get("status", "")) in SUCCESS_STATUSES:
                successes += 1
        if args.mode != "dry-run" and successes < required_successes:
            log(
                f"incomplete point {point_label(point_specs[0])}: "
                f"{successes}/{required_successes} successes after {len(point_specs)} attempt slots"
            )
    summarize_figure(figure, args)


def check(args: argparse.Namespace) -> int:
    status = 0
    checks = [
        ("cargo", shutil.which("cargo") is not None),
        ("sudo", shutil.which("sudo") is not None),
        ("taskset", shutil.which("taskset") is not None),
        ("timeout", shutil.which("timeout") is not None),
        ("ccm_mapping", args.ccm_mapping_path.exists()),
        ("scheduler_manifest", (SCHEDULER_ROOT / "Cargo.toml").exists()),
        ("scheduler_source", (SCHEDULER_ROOT / "src" / "main.rs").exists()),
        ("ae_external_runner", AE_EXTERNAL_RUNNER.exists()),
        ("ycsb_runner", YCSB_RUNNER.exists()),
        ("gapbs_pr", bool(WORKLOADS["gapbs_pr_kron20"].binary and WORKLOADS["gapbs_pr_kron20"].binary.exists())),
        ("gapbs_bc", bool(WORKLOADS["gapbs_bc_kron20_twitter"].binary and WORKLOADS["gapbs_bc_kron20_twitter"].binary.exists())),
        ("gapbs_twitter_graph", (GAPBS_GRAPH_ROOT / "twitter.sg").exists()),
        ("node_replication_manifest", (NODE_REPLICATION_ROOT / "Cargo.toml").exists()),
        ("llama_model", Path(WORKLOADS["llamacpp_llama31_8b"].backend_options["model"]).exists()),
        ("filebench_webserver_template", (AE_ROOT / "templates" / "filebench_webserver_10s.f").exists()),
        ("filebench_webproxy_template", (AE_ROOT / "templates" / "filebench_webproxy_10s.f").exists()),
        ("filebench_varmail_template", (AE_ROOT / "templates" / "filebench_varmail_10s.f").exists()),
        ("ycsb_256m_workload", ycsb_workload("working_set_256mb.properties").exists()),
        ("ycsb_256m_large_value_workload", ycsb_workload("working_set_256mb_large_value.properties").exists()),
        ("memory_benchmark", memory_benchmark_path().exists()),
    ]
    for name, ok in checks:
        print(f"[{'ok' if ok else 'fail'}] {name}")
        if not ok:
            status = 1
    if args.use_sudo:
        sudo = subprocess.run(["sudo", "-n", "true"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False)
        print(f"[{'ok' if sudo.returncode == 0 else 'warn'}] passwordless sudo")
    return status


def add_common_args(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("--out-root", type=Path, default=Path(os.environ.get("AE_OUT_ROOT", DEFAULT_OUT_ROOT)))
    parser.add_argument("--profile", choices=("debug", "release"), default=os.environ.get("AE_PROFILE", "release"))
    parser.add_argument("--repeats", type=int, default=int(os.environ.get("AE_REPEATS", "1")))
    parser.add_argument("--max-attempts", type=int, default=int(os.environ.get("AE_MAX_ATTEMPTS", "1")))
    parser.add_argument("--timeout-sec", type=int, default=int(os.environ.get("AE_TIMEOUT_SEC", "900")))
    parser.add_argument("--build-timeout-sec", type=int, default=int(os.environ.get("AE_BUILD_TIMEOUT_SEC", "1200")))
    parser.add_argument("--skip-build", action="store_true", default=os.environ.get("AE_SKIP_BUILD", "0") == "1")
    parser.add_argument("--force", action="store_true")
    parser.add_argument("--light", dest="light", action="store_true", default=os.environ.get("AE_LIGHT", "1") != "0")
    parser.add_argument("--paper-fidelity", dest="light", action="store_false")
    parser.add_argument("--light-build", dest="light_build", action="store_true", default=os.environ.get("AE_LIGHT_BUILD", "1") != "0")
    parser.add_argument("--default-build", dest="light_build", action="store_false")
    parser.add_argument(
        "--keep-heavy-workload-artifacts",
        action="store_true",
        default=os.environ.get("AE_KEEP_HEAVY_WORKLOAD_ARTIFACTS", "0") == "1",
        help="Keep large regenerated workload databases under harness_results instead of pruning them after status is recorded.",
    )
    parser.add_argument("--sudo", dest="use_sudo", action="store_true", default=os.environ.get("AE_USE_SUDO", "1") != "0")
    parser.add_argument("--no-sudo", dest="use_sudo", action="store_false")
    parser.add_argument("--scheduler-logs", action="store_true", default=os.environ.get("AE_SCHEDULER_LOGS", "0") == "1")
    parser.add_argument("--monitor-sec", type=float, default=float(os.environ.get("AE_MONITOR_SEC", "0")))
    parser.add_argument("--ccm-mapping-path", type=Path, default=Path(os.environ.get("AE_CCM_MAPPING_PATH", DEFAULT_CCM_MAPPING)))
    parser.add_argument("--workload-cpus", default=os.environ.get("AE_WORKLOAD_CPUS", DEFAULT_WORKLOAD_CPUS))
    parser.add_argument("--noise-cpus", default=os.environ.get("AE_NOISE_CPUS", DEFAULT_NOISE_CPUS))
    parser.add_argument("--noise-duration-sec", type=int, default=int(os.environ.get("AE_NOISE_DURATION_SEC", "120")))
    parser.add_argument("--noise-settle-sec", type=float, default=float(os.environ.get("AE_NOISE_SETTLE_SEC", "2")))
    parser.add_argument("--default-noise-rate", type=int, default=int(os.environ.get("AE_DEFAULT_NOISE_RATE", "1000")))
    parser.add_argument("--paper-noise-duration-sec", type=int, default=int(os.environ.get("AE_PAPER_NOISE_DURATION_SEC", str(DEFAULT_PAPER_NOISE_DURATION_SEC))))
    parser.add_argument("--fig10-loaded-workload-cpus", default=os.environ.get("AE_FIG10_LOADED_WORKLOAD_CPUS", DEFAULT_FIG10_LOADED_WORKLOAD_CPUS))
    parser.add_argument("--fig10-noise-cpus", default=os.environ.get("AE_FIG10_NOISE_CPUS", DEFAULT_FIG10_NOISE_CPUS))
    parser.add_argument("--fig10-noise-rate", type=int, default=int(os.environ.get("AE_FIG10_NOISE_RATE", str(DEFAULT_FIG10_NOISE_RATE))))
    parser.add_argument("--fig10-noise-duration-sec", type=int, default=int(os.environ.get("AE_FIG10_NOISE_DURATION_SEC", str(DEFAULT_FIG10_NOISE_DURATION_SEC))))
    parser.add_argument("--fig12-noise-duration-sec", type=int, default=int(os.environ.get("AE_FIG12_NOISE_DURATION_SEC", str(DEFAULT_FIG12_NOISE_DURATION_SEC))))
    parser.add_argument("--fig12-scope-widen-delay-sec", type=float, default=float(os.environ.get("AE_FIG12_SCOPE_WIDEN_DELAY_SEC", "0")))
    parser.add_argument("--managed-cpu-max", type=int, default=int(os.environ.get("AE_MANAGED_CPU_MAX", "83")))
    parser.add_argument("--gapbs-iterations", type=int, default=int(os.environ.get("AE_GAPBS_ITERATIONS", "16")))
    parser.add_argument("--cs-villain-throttle", choices=("true", "false"), default=os.environ.get("AE_CS_VILLAIN_THROTTLE", "true"))
    parser.add_argument("--tick-reeval-every", type=int, default=int(os.environ.get("AE_TICK_REEVAL_EVERY", "1")))
    parser.add_argument("--tick-defer-max", type=int, default=int(os.environ.get("AE_TICK_DEFER_MAX", "1")))
    parser.add_argument("--cs-villain-reslice-us", type=int, default=int(os.environ.get("AE_CS_VILLAIN_RESLICE_US", "1000")))
    parser.add_argument("--cs-villain-refill-divisor", type=int, default=int(os.environ.get("AE_CS_VILLAIN_REFILL_DIVISOR", "4")))
    parser.add_argument("--cs-villain-settle-ms", type=int, default=int(os.environ.get("AE_CS_VILLAIN_SETTLE_MS", "40")))
    parser.add_argument("--cs-villain-release-samples", type=int, default=int(os.environ.get("AE_CS_VILLAIN_RELEASE_SAMPLES", "2")))
    parser.add_argument("--benchmarks", default=os.environ.get("AE_BENCHMARKS"))
    parser.add_argument("--variants", default=os.environ.get("AE_VARIANTS"))
    parser.add_argument("--cases", default=os.environ.get("AE_CASES"))
    parser.add_argument("--cores", default=os.environ.get("AE_CORES"))
    parser.add_argument("--noise-rates", default=os.environ.get("AE_NOISE_RATES"))
    parser.add_argument("--fig10-threads", type=int, default=int(os.environ.get("AE_FIG10_THREADS", "28")))
    parser.add_argument("--fig11-threads", type=int, default=int(os.environ.get("AE_FIG11_THREADS", "3")))
    parser.add_argument("--fig12-threads", type=int, default=int(os.environ.get("AE_FIG12_THREADS", "1")))


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Standalone AE harness for cSwitch paper figures.")
    sub = parser.add_subparsers(dest="command", required=True)
    run = sub.add_parser("run", help="Run or dry-run an AE figure without eval/exp* runners.")
    add_common_args(run)
    run.add_argument("--figure", required=True, choices=("fig10", "fig11", "fig12a", "fig12b", "fig13", "all"))
    run.add_argument("--mode", default="dry-run", choices=("dry-run", "smoke", "full"))

    summarize = sub.add_parser("summarize", help="Refresh AE summary files.")
    add_common_args(summarize)
    summarize.add_argument("--figure", default="all", choices=("fig10", "fig11", "fig12a", "fig12b", "fig13", "all"))

    check_parser = sub.add_parser("check", help="Check AE harness prerequisites.")
    add_common_args(check_parser)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    args.out_root = args.out_root.expanduser().resolve()
    args.ccm_mapping_path = args.ccm_mapping_path.expanduser().resolve()
    if args.repeats <= 0:
        raise SystemExit("--repeats must be positive")
    if args.max_attempts < args.repeats:
        raise SystemExit("--max-attempts must be >= --repeats")
    if args.fig10_noise_rate < 0:
        raise SystemExit("--fig10-noise-rate must be non-negative")
    if args.fig10_noise_duration_sec <= 0:
        raise SystemExit("--fig10-noise-duration-sec must be positive")
    if args.paper_noise_duration_sec <= 0:
        raise SystemExit("--paper-noise-duration-sec must be positive")
    if args.fig12_noise_duration_sec <= 0:
        raise SystemExit("--fig12-noise-duration-sec must be positive")
    if args.fig12_scope_widen_delay_sec < 0:
        raise SystemExit("--fig12-scope-widen-delay-sec must be non-negative")
    if args.tick_reeval_every <= 0:
        raise SystemExit("--tick-reeval-every must be positive")
    if args.tick_defer_max <= 0:
        raise SystemExit("--tick-defer-max must be positive")
    if args.cs_villain_reslice_us <= 0:
        raise SystemExit("--cs-villain-reslice-us must be positive")
    if args.cs_villain_refill_divisor <= 0:
        raise SystemExit("--cs-villain-refill-divisor must be positive")
    if args.cs_villain_settle_ms <= 0:
        raise SystemExit("--cs-villain-settle-ms must be positive")
    if args.cs_villain_release_samples <= 0:
        raise SystemExit("--cs-villain-release-samples must be positive")
    if args.command == "check":
        return check(args)
    figures = ["fig10", "fig11", "fig12a", "fig12b", "fig13"] if args.figure == "all" else [args.figure]
    if args.command == "summarize":
        for figure in figures:
            summarize_figure(figure, args)
        return 0
    for figure in figures:
        run_figure(figure, args)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
