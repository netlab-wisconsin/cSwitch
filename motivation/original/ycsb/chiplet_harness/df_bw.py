from __future__ import annotations

import os
import struct
import threading
import time
from dataclasses import dataclass


MASK_48 = (1 << 48) - 1
MSR_DF_PERF_CTL_BASE = 0xC0010240
MSR_DF_PERF_CTR_BASE = 0xC0010241


@dataclass(frozen=True)
class DfResourceSpec:
    family: str
    resource_id: int
    instance_id: int
    event_code: int
    read_umask: int
    write_umask: int
    read_beat_bytes: int
    write_beat_bytes: int

    @property
    def event_sel(self) -> int:
        return (self.instance_id << 6) | (self.event_code & 0x3F)


@dataclass(frozen=True)
class DfResourceBandwidth:
    read_mib_s: float
    write_mib_s: float

    @property
    def total_mib_s(self) -> float:
        return self.read_mib_s + self.write_mib_s


@dataclass(frozen=True)
class DfBandwidthSummary:
    duration_s: float
    resource_family: str
    per_resource: dict[int, DfResourceBandwidth]

    @property
    def total_read_mib_s(self) -> float:
        return sum(metric.read_mib_s for metric in self.per_resource.values())

    @property
    def total_write_mib_s(self) -> float:
        return sum(metric.write_mib_s for metric in self.per_resource.values())

    @property
    def total_mib_s(self) -> float:
        return self.total_read_mib_s + self.total_write_mib_s


def _pack_df_perf_ctl(event_sel: int, unit_mask: int, enabled: int = 1) -> int:
    return (
        (((event_sel >> 12) & 0x3) << 36)
        | (((event_sel >> 8) & 0xF) << 32)
        | (((unit_mask >> 8) & 0xF) << 24)
        | ((enabled & 0x1) << 22)
        | ((unit_mask & 0xFF) << 8)
        | (event_sel & 0xFF)
    )


def _wrmsr(fd: int, msr: int, value: int) -> None:
    os.lseek(fd, msr, os.SEEK_SET)
    os.write(fd, struct.pack("Q", value))


def _rdmsr(fd: int, msr: int) -> int:
    os.lseek(fd, msr, os.SEEK_SET)
    return struct.unpack("Q", os.read(fd, 8))[0]


def _ctl_msr(counter_index: int) -> int:
    return MSR_DF_PERF_CTL_BASE + (2 * counter_index)


def _ctr_msr(counter_index: int) -> int:
    return MSR_DF_PERF_CTR_BASE + (2 * counter_index)


def _disable_all_counters(fd: int, counter_count: int = 16) -> None:
    for counter_index in range(counter_count):
        _wrmsr(fd, _ctl_msr(counter_index), 0)


def _resource_spec(family: str, resource_id: int) -> DfResourceSpec:
    normalized_family = family.upper()
    if normalized_family == "CCM":
        return DfResourceSpec(
            family=normalized_family,
            resource_id=resource_id,
            instance_id=0x10 + resource_id,
            event_code=0x1E,
            read_umask=0xFFE,
            write_umask=0xFFF,
            read_beat_bytes=32,
            write_beat_bytes=64,
        )
    if normalized_family == "CS":
        return DfResourceSpec(
            family=normalized_family,
            resource_id=resource_id,
            instance_id=resource_id,
            event_code=0x1F,
            read_umask=0xFFE,
            write_umask=0xFFF,
            read_beat_bytes=64,
            write_beat_bytes=64,
        )
    if normalized_family == "IOM":
        return DfResourceSpec(
            family=normalized_family,
            resource_id=resource_id,
            instance_id=0x20 + resource_id,
            event_code=0x1F,
            read_umask=0xFFE,
            write_umask=0xFFF,
            read_beat_bytes=64,
            write_beat_bytes=64,
        )
    raise ValueError(f"unsupported DF resource family: {family}")


class DfBandwidthMonitor:
    def __init__(
        self,
        resource_family: str,
        resource_ids: tuple[int, ...],
        sample_slot_ms: int = 20,
    ) -> None:
        if len(set(resource_ids)) != len(resource_ids):
            raise ValueError("DF monitor resource ids must be unique")
        if sample_slot_ms <= 0:
            raise ValueError("DF monitor sample_slot_ms must be > 0")

        self._resource_family = resource_family.upper()
        self._resource_ids = tuple(resource_ids)
        self._resource_specs = tuple(
            _resource_spec(self._resource_family, resource_id)
            for resource_id in self._resource_ids
        )
        self._sample_slot_ms = sample_slot_ms
        self._fd: int | None = None
        self._start_time: float | None = None
        self._stop_event = threading.Event()
        self._thread: threading.Thread | None = None
        self._lock = threading.Lock()
        self._sampled_read_bytes: dict[int, float] = {resource_id: 0.0 for resource_id in self._resource_ids}
        self._sampled_write_bytes: dict[int, float] = {resource_id: 0.0 for resource_id in self._resource_ids}
        self._sampled_duration_s: dict[int, float] = {resource_id: 0.0 for resource_id in self._resource_ids}

    def start(self) -> None:
        if self._fd is not None:
            raise RuntimeError("DF monitor already started")
        self._fd = os.open("/dev/cpu/0/msr", os.O_RDWR)
        _disable_all_counters(self._fd)
        for counter_index in range(16):
            _wrmsr(self._fd, _ctr_msr(counter_index), 0)
        self._start_time = time.monotonic()

        if len(self._resource_specs) <= 8:
            self._arm_simultaneous_counters()
            return

        self._thread = threading.Thread(
            target=self._sequential_sampler_loop,
            name=f"df-bw-{self._resource_family.lower()}",
            daemon=True,
        )
        self._thread.start()

    def _arm_simultaneous_counters(self) -> None:
        assert self._fd is not None
        for slot, spec in enumerate(self._resource_specs):
            read_counter = slot * 2
            write_counter = read_counter + 1
            _wrmsr(
                self._fd,
                _ctl_msr(read_counter),
                _pack_df_perf_ctl(spec.event_sel, spec.read_umask, 1),
            )
            _wrmsr(
                self._fd,
                _ctl_msr(write_counter),
                _pack_df_perf_ctl(spec.event_sel, spec.write_umask, 1),
            )
            _wrmsr(self._fd, _ctr_msr(read_counter), 0)
            _wrmsr(self._fd, _ctr_msr(write_counter), 0)

    def _sample_one_resource(self, spec: DfResourceSpec) -> tuple[float, float, float]:
        assert self._fd is not None
        _wrmsr(self._fd, _ctl_msr(0), _pack_df_perf_ctl(spec.event_sel, spec.read_umask, 1))
        _wrmsr(self._fd, _ctl_msr(1), _pack_df_perf_ctl(spec.event_sel, spec.write_umask, 1))
        _wrmsr(self._fd, _ctr_msr(0), 0)
        _wrmsr(self._fd, _ctr_msr(1), 0)

        start_ns = time.monotonic_ns()
        time.sleep(self._sample_slot_ms / 1000.0)
        read_beats = _rdmsr(self._fd, _ctr_msr(0)) & MASK_48
        write_beats = _rdmsr(self._fd, _ctr_msr(1)) & MASK_48
        end_ns = time.monotonic_ns()

        _wrmsr(self._fd, _ctl_msr(0), 0)
        _wrmsr(self._fd, _ctl_msr(1), 0)

        duration_s = max((end_ns - start_ns) / 1_000_000_000.0, 1e-9)
        read_bytes = float(read_beats * spec.read_beat_bytes)
        write_bytes = float(write_beats * spec.write_beat_bytes)
        return read_bytes, write_bytes, duration_s

    def _sequential_sampler_loop(self) -> None:
        assert self._fd is not None
        while not self._stop_event.is_set():
            for spec in self._resource_specs:
                if self._stop_event.is_set():
                    return
                read_bytes, write_bytes, duration_s = self._sample_one_resource(spec)
                with self._lock:
                    self._sampled_read_bytes[spec.resource_id] += read_bytes
                    self._sampled_write_bytes[spec.resource_id] += write_bytes
                    self._sampled_duration_s[spec.resource_id] += duration_s

    def stop(self) -> DfBandwidthSummary:
        if self._fd is None or self._start_time is None:
            raise RuntimeError("DF monitor is not active")
        duration_s = max(time.monotonic() - self._start_time, 1e-9)
        per_resource: dict[int, DfResourceBandwidth] = {}
        try:
            if len(self._resource_specs) <= 8:
                for slot, spec in enumerate(self._resource_specs):
                    read_counter = slot * 2
                    write_counter = read_counter + 1
                    read_beats = _rdmsr(self._fd, _ctr_msr(read_counter)) & MASK_48
                    write_beats = _rdmsr(self._fd, _ctr_msr(write_counter)) & MASK_48
                    read_mib_s = (
                        read_beats * spec.read_beat_bytes
                    ) / (1024.0 * 1024.0 * duration_s)
                    write_mib_s = (
                        write_beats * spec.write_beat_bytes
                    ) / (1024.0 * 1024.0 * duration_s)
                    per_resource[spec.resource_id] = DfResourceBandwidth(
                        read_mib_s=read_mib_s,
                        write_mib_s=write_mib_s,
                    )
            else:
                self._stop_event.set()
                if self._thread is not None:
                    self._thread.join(timeout=max(duration_s, 1.0) + 1.0)
                with self._lock:
                    for spec in self._resource_specs:
                        sampled_duration_s = self._sampled_duration_s[spec.resource_id]
                        if sampled_duration_s <= 0:
                            per_resource[spec.resource_id] = DfResourceBandwidth(
                                read_mib_s=0.0,
                                write_mib_s=0.0,
                            )
                            continue
                        per_resource[spec.resource_id] = DfResourceBandwidth(
                            read_mib_s=(
                                self._sampled_read_bytes[spec.resource_id]
                                / (1024.0 * 1024.0 * sampled_duration_s)
                            ),
                            write_mib_s=(
                                self._sampled_write_bytes[spec.resource_id]
                                / (1024.0 * 1024.0 * sampled_duration_s)
                            ),
                        )
        finally:
            _disable_all_counters(self._fd)
            os.close(self._fd)
            self._fd = None
            self._start_time = None
            self._thread = None
        return DfBandwidthSummary(
            duration_s=duration_s,
            resource_family=self._resource_family,
            per_resource=per_resource,
        )
