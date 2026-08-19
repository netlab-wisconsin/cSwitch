from __future__ import annotations

import json
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any


def _resolve_path(base_dir: Path, raw: str | None, default: str | None = None) -> Path:
    value = raw if raw is not None else default
    if value is None:
        raise ValueError("missing required path value")
    path = Path(value)
    if not path.is_absolute():
        path = (base_dir / path).resolve()
    return path


def _parse_int_list(raw: Any, field_name: str) -> tuple[int, ...]:
    if raw is None:
        return tuple()
    if isinstance(raw, int):
        return (raw,)
    if isinstance(raw, str):
        text = raw.replace(",", " ")
        values = [chunk for chunk in text.split() if chunk]
        if not values:
            return tuple()
        return tuple(int(value) for value in values)
    if isinstance(raw, list):
        return tuple(int(value) for value in raw)
    raise ValueError(f"{field_name} must be an int, string, or list")


def _parse_float_list(raw: Any, field_name: str) -> tuple[float, ...]:
    if raw is None:
        return tuple()
    if isinstance(raw, (int, float)):
        return (float(raw),)
    if isinstance(raw, str):
        text = raw.replace(",", " ")
        values = [chunk for chunk in text.split() if chunk]
        if not values:
            return tuple()
        return tuple(float(value) for value in values)
    if isinstance(raw, list):
        return tuple(float(value) for value in raw)
    raise ValueError(f"{field_name} must be a float, string, or list")


def _parse_str_map(raw: Any, field_name: str) -> dict[str, str]:
    if raw is None:
        return {}
    if not isinstance(raw, dict):
        raise ValueError(f"{field_name} must be a JSON object")
    return {str(key): str(value) for key, value in raw.items()}


@dataclass(frozen=True)
class WorkloadConfig:
    file: Path
    working_set_mb_per_instance: int
    primary_operation: str = "READ"
    hdr_percentiles: tuple[float, ...] = (50.0, 95.0, 99.0, 99.9, 99.99)


@dataclass(frozen=True)
class Assignment:
    label: str
    cores: tuple[int, ...]
    numas: tuple[int, ...]
    apply_numa_binding: bool
    metadata: dict[str, Any] = field(default_factory=dict)


@dataclass(frozen=True)
class AssignmentGenerator:
    kind: str
    counts: tuple[int, ...]
    chiplets: int = 12
    cores_per_chiplet: int = 7
    base_core: int = 0
    label_prefix: str = "chiplet_x"
    bind_numa: int | None = None


@dataclass(frozen=True)
class MonitoringConfig:
    event_name: str = "NO_RETIRED_INST_CYCLES"
    perf_event: str = "cpu/event=0xc0,cmask=1,inv=1/"
    perf_enabled: bool = True
    df_enabled: bool = True
    df_resource_family: str = "CCM"
    df_resource_ids: tuple[int, ...] = (0, 1, 2, 3, 4, 5, 6, 7)
    df_sample_slot_ms: int = 20


@dataclass(frozen=True)
class BackgroundNoiseConfig:
    kind: str
    binary: Path
    startup_seconds: int = 2
    apply_during_load: bool = False
    memory: str = "128m"
    worker_memory_mb: int = 256
    bandwidth: bool = True
    time_seconds: int = 86400
    clock_ghz: float = 2.25
    silent: bool = True
    rate: int = 0
    mode: int = 0
    thread_alloc: bool = False
    alloc_core: int = 0
    alloc_numa: int = 0
    alloc_rate: int = 0
    alloc_mode: int = 3


@dataclass(frozen=True)
class BackendConfig:
    name: str
    operationcount: int | None = None
    threads_per_instance: int = 1
    java_active_processor_count: int = 1
    load_threads: int | None = None
    java_opts: str = ""
    options: dict[str, str] = field(default_factory=dict)
    env: dict[str, str] = field(default_factory=dict)
    load_props: dict[str, str] = field(default_factory=dict)
    run_props: dict[str, str] = field(default_factory=dict)


@dataclass(frozen=True)
class HarnessConfig:
    config_path: Path
    ycsb_root: Path
    result_root: Path
    result_prefix: str
    keep_result_db: bool
    fail_fast: bool
    workload: WorkloadConfig | None
    monitoring: MonitoringConfig
    setup_cgroup: Path | None
    workload_cgroup: Path | None
    background_noise: BackgroundNoiseConfig | None
    backends: tuple[BackendConfig, ...]
    assignments: tuple[Assignment, ...]


def _parse_assignment(raw: dict[str, Any]) -> Assignment:
    label = str(raw["label"])
    cores = _parse_int_list(raw.get("cores"), f"assignments[{label}].cores")
    if not cores:
        raise ValueError(f"assignment {label} has no cores")

    raw_numas = raw.get("numas")
    apply_numa_binding = raw_numas is not None
    numas = _parse_int_list(raw_numas, f"assignments[{label}].numas")
    if not numas:
        numas = (0,) * len(cores)
        apply_numa_binding = False
    elif len(numas) == 1 and len(cores) > 1:
        numas = numas * len(cores)
    elif len(numas) != len(cores):
        raise ValueError(
            f"assignment {label} has {len(cores)} cores but {len(numas)} NUMA entries"
        )

    metadata = raw.get("metadata") if isinstance(raw.get("metadata"), dict) else {}
    return Assignment(
        label=label,
        cores=tuple(cores),
        numas=tuple(numas),
        apply_numa_binding=apply_numa_binding,
        metadata=dict(metadata),
    )


def _generate_chiplet_density_assignments(generator: AssignmentGenerator) -> tuple[Assignment, ...]:
    assignments: list[Assignment] = []
    for count in generator.counts:
        if count <= 0:
            raise ValueError("assignment_generator.counts entries must be > 0")
        if count > generator.cores_per_chiplet:
            raise ValueError(
                f"chiplet density {count} exceeds cores_per_chiplet={generator.cores_per_chiplet}"
            )

        cores: list[int] = []
        for chiplet in range(generator.chiplets):
            chiplet_base = generator.base_core + chiplet * generator.cores_per_chiplet
            for offset in range(count):
                cores.append(chiplet_base + offset)

        if generator.bind_numa is None:
            numas = tuple(0 for _ in cores)
            apply_numa_binding = False
        else:
            numas = tuple(generator.bind_numa for _ in cores)
            apply_numa_binding = True

        assignments.append(
            Assignment(
                label=f"{generator.label_prefix}{count}",
                cores=tuple(cores),
                numas=numas,
                apply_numa_binding=apply_numa_binding,
                metadata={"instances_per_chiplet": count},
            )
        )
    return tuple(assignments)


def _generate_prefix_density_assignments(generator: AssignmentGenerator) -> tuple[Assignment, ...]:
    assignments: list[Assignment] = []
    for count in generator.counts:
        if count <= 0:
            raise ValueError("assignment_generator.counts entries must be > 0")

        cores = tuple(range(generator.base_core, generator.base_core + count))
        if generator.bind_numa is None:
            numas = tuple(0 for _ in cores)
            apply_numa_binding = False
        else:
            numas = tuple(generator.bind_numa for _ in cores)
            apply_numa_binding = True

        assignments.append(
            Assignment(
                label=f"{generator.label_prefix}{count}",
                cores=cores,
                numas=numas,
                apply_numa_binding=apply_numa_binding,
                metadata={"prefix_count": count},
            )
        )
    return tuple(assignments)


def _generate_assignments(generator: AssignmentGenerator) -> tuple[Assignment, ...]:
    if generator.kind == "chiplet_density":
        return _generate_chiplet_density_assignments(generator)
    if generator.kind == "prefix_density":
        return _generate_prefix_density_assignments(generator)
    raise ValueError(f"unsupported assignment_generator.kind: {generator.kind}")


def _parse_assignment_generator(raw: dict[str, Any] | None) -> AssignmentGenerator | None:
    if raw is None:
        return None
    return AssignmentGenerator(
        kind=str(raw.get("kind", "chiplet_density")),
        counts=_parse_int_list(raw.get("counts"), "assignment_generator.counts"),
        chiplets=int(raw.get("chiplets", 12)),
        cores_per_chiplet=int(raw.get("cores_per_chiplet", 7)),
        base_core=int(raw.get("base_core", 0)),
        label_prefix=str(raw.get("label_prefix", "chiplet_x")),
        bind_numa=(
            None if raw.get("bind_numa") is None else int(raw.get("bind_numa"))
        ),
    )


def _parse_background_noise(base_dir: Path, raw: dict[str, Any] | None) -> BackgroundNoiseConfig | None:
    if raw is None:
        return None

    kind = str(raw.get("kind", "memory_benchmark"))
    if kind != "memory_benchmark":
        raise ValueError(f"unsupported background_noise.kind: {kind}")

    return BackgroundNoiseConfig(
        kind=kind,
        binary=_resolve_path(base_dir, raw.get("binary"), "../memory_benchmark"),
        startup_seconds=int(raw.get("startup_seconds", 2)),
        apply_during_load=bool(raw.get("apply_during_load", False)),
        memory=str(raw.get("memory", "128m")),
        worker_memory_mb=int(raw.get("worker_memory_mb", 256)),
        bandwidth=bool(raw.get("bandwidth", True)),
        time_seconds=int(raw.get("time_seconds", 86400)),
        clock_ghz=float(raw.get("clock_ghz", 2.25)),
        silent=bool(raw.get("silent", True)),
        rate=int(raw.get("rate", 0)),
        mode=int(raw.get("mode", 0)),
        thread_alloc=bool(raw.get("thread_alloc", False)),
        alloc_core=int(raw.get("alloc_core", 0)),
        alloc_numa=int(raw.get("alloc_numa", 0)),
        alloc_rate=int(raw.get("alloc_rate", 0)),
        alloc_mode=int(raw.get("alloc_mode", 3)),
    )


def load_config(config_path: Path) -> HarnessConfig:
    raw = json.loads(config_path.read_text(encoding="utf-8"))
    base_dir = config_path.resolve().parent

    ycsb_root = _resolve_path(base_dir, raw.get("ycsb_root"), "../YCSB")
    result_root = _resolve_path(base_dir, raw.get("result_root"), "../results")

    workload_raw = raw.get("workload")
    workload: WorkloadConfig | None
    if workload_raw is None:
        workload = None
    else:
        if not isinstance(workload_raw, dict):
            raise ValueError("config.workload must be a JSON object when present")
        workload = WorkloadConfig(
            file=_resolve_path(base_dir, workload_raw.get("file")),
            working_set_mb_per_instance=int(workload_raw["working_set_mb_per_instance"]),
            primary_operation=str(workload_raw.get("primary_operation", "READ")),
            hdr_percentiles=_parse_float_list(
                workload_raw.get("hdr_percentiles"), "workload.hdr_percentiles"
            )
            or (50.0, 95.0, 99.0, 99.9, 99.99),
        )

    monitoring_raw = raw.get("monitoring") if isinstance(raw.get("monitoring"), dict) else {}
    df_resource_family = str(monitoring_raw.get("df_resource_family", "CCM")).upper()
    df_resource_ids_raw = monitoring_raw.get("df_resource_ids")
    if df_resource_ids_raw is None and "ccm_ids" in monitoring_raw:
        df_resource_ids_raw = monitoring_raw.get("ccm_ids")
    default_df_resource_ids = (
        (0, 1, 2, 3, 4, 5, 6, 7)
        if df_resource_family == "CCM"
        else (0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11)
        if df_resource_family == "CS"
        else (0, 1, 2, 3)
        if df_resource_family == "IOM"
        else tuple()
    )
    monitoring = MonitoringConfig(
        event_name=str(monitoring_raw.get("event_name", "NO_RETIRED_INST_CYCLES")),
        perf_event=str(
            monitoring_raw.get("perf_event", "cpu/event=0xc0,cmask=1,inv=1/")
        ),
        perf_enabled=bool(monitoring_raw.get("perf_enabled", True)),
        df_enabled=bool(monitoring_raw.get("df_enabled", True)),
        df_resource_family=df_resource_family,
        df_resource_ids=_parse_int_list(
            df_resource_ids_raw,
            "monitoring.df_resource_ids",
        )
        or default_df_resource_ids,
        df_sample_slot_ms=int(monitoring_raw.get("df_sample_slot_ms", 20)),
    )
    execution_cgroups_raw = (
        raw.get("execution_cgroups") if isinstance(raw.get("execution_cgroups"), dict) else {}
    )
    setup_cgroup = (
        None
        if execution_cgroups_raw.get("setup") is None
        else _resolve_path(base_dir, execution_cgroups_raw.get("setup"))
    )
    workload_cgroup = (
        None
        if execution_cgroups_raw.get("workload") is None
        else _resolve_path(base_dir, execution_cgroups_raw.get("workload"))
    )
    background_noise = _parse_background_noise(
        base_dir,
        raw.get("background_noise") if isinstance(raw.get("background_noise"), dict) else None,
    )

    backends_raw = raw.get("backends")
    if not isinstance(backends_raw, list) or not backends_raw:
        raise ValueError("config.backends must be a non-empty list")
    backends = tuple(
        BackendConfig(
            name=str(entry["name"]),
            operationcount=(
                None if entry.get("operationcount") is None else int(entry.get("operationcount"))
            ),
            threads_per_instance=int(entry.get("threads_per_instance", 1)),
            java_active_processor_count=int(entry.get("java_active_processor_count", 1)),
            load_threads=(
                None if entry.get("load_threads") is None else int(entry.get("load_threads"))
            ),
            java_opts=str(entry.get("java_opts", "")),
            options=_parse_str_map(entry.get("options"), f"backends[{entry['name']}].options"),
            env=_parse_str_map(entry.get("env"), f"backends[{entry['name']}].env"),
            load_props=_parse_str_map(entry.get("load_props"), f"backends[{entry['name']}].load_props"),
            run_props=_parse_str_map(entry.get("run_props"), f"backends[{entry['name']}].run_props"),
        )
        for entry in backends_raw
    )

    assignments: list[Assignment] = []
    generator = _parse_assignment_generator(
        raw.get("assignment_generator") if isinstance(raw.get("assignment_generator"), dict) else None
    )
    if generator is not None:
        assignments.extend(_generate_assignments(generator))

    explicit_assignments = raw.get("assignments")
    if explicit_assignments is not None:
        if not isinstance(explicit_assignments, list):
            raise ValueError("config.assignments must be a list when present")
        assignments.extend(_parse_assignment(entry) for entry in explicit_assignments)

    if not assignments:
        raise ValueError("config must define assignment_generator and/or assignments")

    labels = [assignment.label for assignment in assignments]
    if len(labels) != len(set(labels)):
        raise ValueError("assignment labels must be unique")

    return HarnessConfig(
        config_path=config_path.resolve(),
        ycsb_root=ycsb_root,
        result_root=result_root,
        result_prefix=str(raw.get("result_prefix", "chiplet_ycsb")),
        keep_result_db=bool(raw.get("keep_result_db", False)),
        fail_fast=bool(raw.get("fail_fast", True)),
        workload=workload,
        monitoring=monitoring,
        setup_cgroup=setup_cgroup,
        workload_cgroup=workload_cgroup,
        background_noise=background_noise,
        backends=backends,
        assignments=tuple(assignments),
    )


def config_to_dict(config: HarnessConfig) -> dict[str, Any]:
    return {
        "config_path": str(config.config_path),
        "ycsb_root": str(config.ycsb_root),
        "result_root": str(config.result_root),
        "result_prefix": config.result_prefix,
        "keep_result_db": config.keep_result_db,
        "fail_fast": config.fail_fast,
        "workload": (
            None
            if config.workload is None
            else {
                "file": str(config.workload.file),
                "working_set_mb_per_instance": config.workload.working_set_mb_per_instance,
                "primary_operation": config.workload.primary_operation,
                "hdr_percentiles": list(config.workload.hdr_percentiles),
            }
        ),
        "monitoring": {
            "event_name": config.monitoring.event_name,
            "perf_event": config.monitoring.perf_event,
            "perf_enabled": config.monitoring.perf_enabled,
            "df_enabled": config.monitoring.df_enabled,
            "df_resource_family": config.monitoring.df_resource_family,
            "df_resource_ids": list(config.monitoring.df_resource_ids),
            "df_sample_slot_ms": config.monitoring.df_sample_slot_ms,
        },
        "execution_cgroups": {
            "setup": None if config.setup_cgroup is None else str(config.setup_cgroup),
            "workload": None if config.workload_cgroup is None else str(config.workload_cgroup),
        },
        "background_noise": (
            None
            if config.background_noise is None
            else {
                "kind": config.background_noise.kind,
                "binary": str(config.background_noise.binary),
                "startup_seconds": config.background_noise.startup_seconds,
                "apply_during_load": config.background_noise.apply_during_load,
                "memory": config.background_noise.memory,
                "worker_memory_mb": config.background_noise.worker_memory_mb,
                "bandwidth": config.background_noise.bandwidth,
                "time_seconds": config.background_noise.time_seconds,
                "clock_ghz": config.background_noise.clock_ghz,
                "silent": config.background_noise.silent,
                "rate": config.background_noise.rate,
                "mode": config.background_noise.mode,
                "thread_alloc": config.background_noise.thread_alloc,
                "alloc_core": config.background_noise.alloc_core,
                "alloc_numa": config.background_noise.alloc_numa,
                "alloc_rate": config.background_noise.alloc_rate,
                "alloc_mode": config.background_noise.alloc_mode,
            }
        ),
        "backends": [
            {
                "name": backend.name,
                "operationcount": backend.operationcount,
                "threads_per_instance": backend.threads_per_instance,
                "java_active_processor_count": backend.java_active_processor_count,
                "load_threads": backend.load_threads,
                "java_opts": backend.java_opts,
                "options": dict(backend.options),
                "env": dict(backend.env),
                "load_props": dict(backend.load_props),
                "run_props": dict(backend.run_props),
            }
            for backend in config.backends
        ],
        "assignments": [
            {
                "label": assignment.label,
                "cores": list(assignment.cores),
                "numas": list(assignment.numas),
                "apply_numa_binding": assignment.apply_numa_binding,
                "metadata": dict(assignment.metadata),
            }
            for assignment in config.assignments
        ],
    }
