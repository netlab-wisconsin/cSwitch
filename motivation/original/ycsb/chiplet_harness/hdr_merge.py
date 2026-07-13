from __future__ import annotations

import os
import subprocess
from pathlib import Path


def _find_hdrhistogram_jar(ycsb_root: Path) -> Path:
    matches = sorted((ycsb_root / "core" / "target" / "dependency").glob("HdrHistogram-*.jar"))
    if not matches:
        raise FileNotFoundError(
            "HdrHistogram jar not found under YCSB/core/target/dependency; build YCSB core first"
        )
    return matches[0]


def _ensure_compiled(harness_root: Path, hdr_jar: Path) -> Path:
    source_file = harness_root / "chiplet_harness" / "hdr" / "HdrHistogramMerger.java"
    build_dir = harness_root / "chiplet_harness" / "hdr" / "build"
    class_file = build_dir / "HdrHistogramMerger.class"
    build_dir.mkdir(parents=True, exist_ok=True)
    if class_file.exists() and class_file.stat().st_mtime >= source_file.stat().st_mtime:
        return build_dir
    subprocess.run(
        ["javac", "-cp", str(hdr_jar), "-d", str(build_dir), str(source_file)],
        check=True,
    )
    return build_dir


def merge_histograms(
    harness_root: Path,
    ycsb_root: Path,
    hdr_files: list[Path],
    percentiles: tuple[float, ...],
) -> dict[str, float]:
    if not hdr_files:
        raise ValueError("merge_histograms requires at least one HDR file")
    hdr_jar = _find_hdrhistogram_jar(ycsb_root)
    build_dir = _ensure_compiled(harness_root, hdr_jar)
    classpath = os.pathsep.join([str(build_dir), str(hdr_jar)])
    cmd = [
        "java",
        "-cp",
        classpath,
        "HdrHistogramMerger",
        "--percentiles",
        ",".join(str(percentile) for percentile in percentiles),
    ]
    cmd.extend(str(path) for path in hdr_files)
    completed = subprocess.run(
        cmd,
        check=True,
        capture_output=True,
        text=True,
    )
    metrics: dict[str, float] = {}
    for line in completed.stdout.splitlines():
        if "=" not in line:
            continue
        key, value = line.split("=", 1)
        metrics[key.strip()] = float(value.strip())
    return metrics
