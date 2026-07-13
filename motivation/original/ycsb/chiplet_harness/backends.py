from __future__ import annotations

import os
import pwd
import shlex
import shutil
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

from .config import BackendConfig


@dataclass(frozen=True)
class InvocationSpec:
    host: str
    run_token: str
    assignment_label: str
    instance_id: int
    instance_dir: Path
    workload_file: Path
    recordcount: int
    operationcount: int
    fieldcount: int | None
    fieldlength: int | None
    threads: int


@dataclass(frozen=True)
class YcsbInvocation:
    binding: str
    extra_props: dict[str, str]
    threads: int
    java_opts: str
    db_target: str
    cleanup_path: Path


@dataclass(frozen=True)
class ExternalInvocation:
    command: list[str]
    env: dict[str, str]
    working_dir: Path | None
    db_target: str
    cleanup_path: Path
    benchmark_name: str
    benchmark_class: str | None


class BackendAdapter:
    name = ""
    mode = "embedded"
    execution_model = "per_instance"
    requires_ycsb_launcher = True
    requires_workload = True
    supports_load_phase = True
    maven_project = ""
    binding = ""
    artifact_glob = ""
    dependency_dir = ""

    def ensure_built(self, ycsb_root: Path, backend_config: BackendConfig, config_path: Path) -> None:
        core_jars = list((ycsb_root / "core" / "target").glob("core-*.jar"))
        binding_jars = list((ycsb_root / self.artifact_glob).parent.glob(Path(self.artifact_glob).name))
        dependencies = list((ycsb_root / self.dependency_dir).glob("*.jar"))
        if core_jars and binding_jars and dependencies:
            return
        subprocess.run(
            [
                "mvn",
                "-Psource-run",
                "-pl",
                self.maven_project,
                "-am",
                "package",
                "-DskipTests",
            ],
            cwd=ycsb_root,
            check=True,
        )

    def default_operationcount(self) -> int:
        raise NotImplementedError

    def default_load_threads(self, backend_config: BackendConfig) -> int:
        return backend_config.threads_per_instance

    def _common_props(self, spec: InvocationSpec) -> dict[str, str]:
        props = {
            "recordcount": str(spec.recordcount),
            "operationcount": str(spec.operationcount),
        }
        if spec.fieldcount is not None:
            props["fieldcount"] = str(spec.fieldcount)
        if spec.fieldlength is not None:
            props["fieldlength"] = str(spec.fieldlength)
        return props

    def _java_opts(self, backend_config: BackendConfig, default: str = "") -> str:
        if backend_config.java_opts.strip():
            return backend_config.java_opts.strip()
        return default

    def load_invocation(self, spec: InvocationSpec, backend_config: BackendConfig) -> YcsbInvocation:
        raise NotImplementedError

    def run_invocation(self, spec: InvocationSpec, backend_config: BackendConfig) -> YcsbInvocation:
        raise NotImplementedError

    def assignment_invocation(
        self,
        config_path: Path,
        instance_dir: Path,
        cores: tuple[int, ...],
        assignment_label: str,
        backend_config: BackendConfig,
    ) -> ExternalInvocation:
        raise NotImplementedError

    def instance_external_invocation(
        self,
        config_path: Path,
        instance_dir: Path,
        assignment_label: str,
        instance_id: int,
        backend_config: BackendConfig,
    ) -> ExternalInvocation:
        raise NotImplementedError


class RocksDbAdapter(BackendAdapter):
    name = "rocksdb"
    binding = "rocksdb"
    maven_project = "site.ycsb:rocksdb-binding"
    artifact_glob = "rocksdb/target/rocksdb-binding-*.jar"
    dependency_dir = "core/target/dependency"

    def default_operationcount(self) -> int:
        return 1_000_000

    def _default_props(self) -> dict[str, str]:
        return {
            "rocksdb.parallelism": "1",
            "rocksdb.maxbackgroundjobs": "1",
            "rocksdb.maxbackgroundcompactions": "1",
            "rocksdb.maxbackgroundflushes": "1",
        }

    def _invocation(self, spec: InvocationSpec, backend_config: BackendConfig, phase: str) -> YcsbInvocation:
        db_dir = spec.instance_dir / "rocksdb"
        props = self._common_props(spec)
        props.update(self._default_props())
        props.update(backend_config.load_props if phase == "load" else backend_config.run_props)
        props["rocksdb.dir"] = str(db_dir)
        return YcsbInvocation(
            binding=self.binding,
            extra_props=props,
            threads=(
                backend_config.load_threads
                if phase == "load" and backend_config.load_threads is not None
                else spec.threads
            ),
            java_opts=self._java_opts(backend_config),
            db_target=str(db_dir),
            cleanup_path=db_dir,
        )

    def load_invocation(self, spec: InvocationSpec, backend_config: BackendConfig) -> YcsbInvocation:
        return self._invocation(spec, backend_config, "load")

    def run_invocation(self, spec: InvocationSpec, backend_config: BackendConfig) -> YcsbInvocation:
        return self._invocation(spec, backend_config, "run")


class OrientDbAdapter(BackendAdapter):
    name = "orientdb"
    binding = "orientdb"
    maven_project = "site.ycsb:orientdb-binding"
    artifact_glob = "orientdb/target/orientdb-binding-*.jar"
    dependency_dir = "orientdb/target/dependency"

    def default_operationcount(self) -> int:
        return 10_000

    def _invocation(self, spec: InvocationSpec, backend_config: BackendConfig, phase: str) -> YcsbInvocation:
        db_dir = spec.instance_dir / "orientdb"
        props = self._common_props(spec)
        props["orientdb.url"] = f"plocal:{db_dir}"
        props["orientdb.newdb"] = "true" if phase == "load" else "false"
        props.update(backend_config.load_props if phase == "load" else backend_config.run_props)
        return YcsbInvocation(
            binding=self.binding,
            extra_props=props,
            threads=(
                backend_config.load_threads
                if phase == "load" and backend_config.load_threads is not None
                else spec.threads
            ),
            java_opts=self._java_opts(backend_config),
            db_target=f"plocal:{db_dir}",
            cleanup_path=db_dir,
        )

    def load_invocation(self, spec: InvocationSpec, backend_config: BackendConfig) -> YcsbInvocation:
        return self._invocation(spec, backend_config, "load")

    def run_invocation(self, spec: InvocationSpec, backend_config: BackendConfig) -> YcsbInvocation:
        return self._invocation(spec, backend_config, "run")


class ElasticsearchAdapter(BackendAdapter):
    name = "elasticsearch"
    binding = "elasticsearch"
    maven_project = "site.ycsb:elasticsearch-binding"
    artifact_glob = "elasticsearch/target/elasticsearch-binding-*.jar"
    dependency_dir = "elasticsearch/target/dependency"

    def default_operationcount(self) -> int:
        return 10_000

    def default_load_threads(self, backend_config: BackendConfig) -> int:
        if backend_config.load_threads is not None:
            return backend_config.load_threads
        return 1

    def _default_run_props(self) -> dict[str, str]:
        return {
            "http.enabled": "false",
            "processors": "1",
            "es.index.key": "es.ycsb",
            "es.number_of_shards": "1",
            "es.number_of_replicas": "0",
        }

    def _default_load_props(self) -> dict[str, str]:
        return self._default_run_props()

    def _invocation(self, spec: InvocationSpec, backend_config: BackendConfig, phase: str) -> YcsbInvocation:
        if spec.threads != 1:
            raise ValueError("elasticsearch run phase requires threads_per_instance=1")

        path_home = spec.instance_dir / "elasticsearch-home"
        cluster_name = (
            f"es.ycsb.{spec.host}.{spec.run_token}.{spec.assignment_label}.inst{spec.instance_id:02d}"
        )
        node_name = f"es-ycsb-{spec.assignment_label}-inst{spec.instance_id:02d}"
        props = self._common_props(spec)
        props["path.home"] = str(path_home)
        props["cluster.name"] = cluster_name
        props["node.name"] = node_name
        if phase == "load":
            props["es.newdb"] = "true"
            props.update(self._default_load_props())
            props.update(backend_config.load_props)
        else:
            props.update(self._default_run_props())
            props.update(backend_config.run_props)
        return YcsbInvocation(
            binding=self.binding,
            extra_props=props,
            threads=self.default_load_threads(backend_config) if phase == "load" else spec.threads,
            java_opts=self._java_opts(backend_config, "-Xms2g -Xmx2g"),
            db_target=str(path_home),
            cleanup_path=path_home,
        )

    def load_invocation(self, spec: InvocationSpec, backend_config: BackendConfig) -> YcsbInvocation:
        return self._invocation(spec, backend_config, "load")

    def run_invocation(self, spec: InvocationSpec, backend_config: BackendConfig) -> YcsbInvocation:
        return self._invocation(spec, backend_config, "run")


class _NpbBaseAdapter(BackendAdapter):
    requires_ycsb_launcher = False
    requires_workload = False
    supports_load_phase = False

    def default_operationcount(self) -> int:
        return 1

    def _option(self, backend_config: BackendConfig, *names: str, required: bool = False) -> str | None:
        for name in names:
            value = backend_config.options.get(name)
            if value is not None and value.strip():
                return value.strip()
        if required:
            joined = " / ".join(names)
            raise RuntimeError(f"backend {self.name} requires backend.options.{joined}")
        return None

    def _resolve_path(self, config_path: Path, raw: str | None) -> Path | None:
        if raw is None:
            return None
        path = Path(raw).expanduser()
        if not path.is_absolute():
            path = (config_path.parent / path).resolve()
        return path

    def _benchmark_name(self, backend_config: BackendConfig, required: bool = True) -> str | None:
        value = self._option(backend_config, "benchmark", "benchmark_name", required=required)
        return None if value is None else value.upper()

    def _benchmark_class(self, backend_config: BackendConfig, required: bool = True) -> str | None:
        value = self._option(backend_config, "benchmark_class", "class", required=required)
        return None if value is None else value.upper()

    def _binary_path(self, config_path: Path, backend_config: BackendConfig) -> tuple[Path, Path | None]:
        explicit_binary = self._resolve_path(config_path, self._option(backend_config, "binary"))
        if explicit_binary is not None:
            return explicit_binary, explicit_binary.parent

        npb_root = self._resolve_path(config_path, self._option(backend_config, "npb_root", required=True))
        assert npb_root is not None
        benchmark = self._benchmark_name(backend_config, required=True).lower()
        benchmark_class = self._benchmark_class(backend_config, required=True)
        return npb_root / "bin" / f"{benchmark}.{benchmark_class}.x", npb_root

    def ensure_built(self, ycsb_root: Path, backend_config: BackendConfig, config_path: Path) -> None:
        del ycsb_root
        binary, npb_root = self._binary_path(config_path, backend_config)
        if binary.exists():
            return
        if npb_root is None:
            raise RuntimeError(f"missing NPB binary: {binary}")
        if not (npb_root / "config" / "make.def").exists():
            raise RuntimeError(
                f"missing NPB config/make.def under {npb_root}; configure NPB before using {self.name}"
            )
        subprocess.run(
            [
                "make",
                self._benchmark_name(backend_config, required=True).lower(),
                f"CLASS={self._benchmark_class(backend_config, required=True)}",
            ],
            cwd=npb_root,
            check=True,
        )
        if not binary.exists():
            raise RuntimeError(f"failed to build NPB benchmark binary: {binary}")

    def _external_invocation(
        self,
        config_path: Path,
        instance_dir: Path,
        backend_config: BackendConfig,
        env_defaults: dict[str, str],
    ) -> ExternalInvocation:
        binary, npb_root = self._binary_path(config_path, backend_config)
        if not binary.exists():
            raise RuntimeError(f"missing NPB binary: {binary}")
        env = dict(backend_config.env)
        for key, value in env_defaults.items():
            env.setdefault(key, value)
        args = shlex.split(self._option(backend_config, "args") or "")
        benchmark_name = self._benchmark_name(backend_config, required=False) or binary.stem.upper()
        return ExternalInvocation(
            command=[str(binary), *args],
            env=env,
            working_dir=npb_root,
            db_target=str(binary),
            cleanup_path=instance_dir / "npb-runtime",
            benchmark_name=benchmark_name,
            benchmark_class=self._benchmark_class(backend_config, required=False),
        )


class NpbOmpAdapter(_NpbBaseAdapter):
    name = "npb_omp"
    mode = "openmp"
    execution_model = "assignment_openmp"

    def assignment_invocation(
        self,
        config_path: Path,
        instance_dir: Path,
        cores: tuple[int, ...],
        assignment_label: str,
        backend_config: BackendConfig,
    ) -> ExternalInvocation:
        del assignment_label
        return self._external_invocation(
            config_path,
            instance_dir,
            backend_config,
            env_defaults={
                "OMP_NUM_THREADS": str(backend_config.threads_per_instance),
                "OMP_PROC_BIND": "close",
                "OMP_PLACES": "cores",
            },
        )


class NpbInstancesAdapter(_NpbBaseAdapter):
    name = "npb_instances"
    mode = "single_thread"
    execution_model = "per_instance_external"

    def instance_external_invocation(
        self,
        config_path: Path,
        instance_dir: Path,
        assignment_label: str,
        instance_id: int,
        backend_config: BackendConfig,
    ) -> ExternalInvocation:
        del assignment_label, instance_id
        if backend_config.threads_per_instance != 1:
            raise RuntimeError("backend npb_instances requires threads_per_instance=1")
        return self._external_invocation(
            config_path,
            instance_dir,
            backend_config,
            env_defaults={
                "OMP_NUM_THREADS": "1",
                "OMP_PROC_BIND": "close",
                "OMP_PLACES": "cores",
            },
        )


class DuckDbTpchAdapter(BackendAdapter):
    name = "duckdb_tpch"
    mode = "analytic"
    execution_model = "per_instance_external"
    requires_ycsb_launcher = False
    requires_workload = False
    supports_load_phase = False

    def default_operationcount(self) -> int:
        return 1

    def _option(self, backend_config: BackendConfig, name: str, default: str | None = None) -> str | None:
        value = backend_config.options.get(name)
        if value is None or not value.strip():
            return default
        return value.strip()

    def _resolve_path(self, config_path: Path, raw: str | None) -> Path | None:
        if raw is None:
            return None
        path = Path(raw).expanduser()
        if not path.is_absolute():
            path = (config_path.parent / path).resolve()
        return path

    def _python_bin(self, config_path: Path, backend_config: BackendConfig) -> str:
        resolved = self._resolve_path(config_path, self._option(backend_config, "python_bin"))
        if resolved is not None:
            return str(resolved)
        return sys.executable

    def _pythonpath(self, backend_config: BackendConfig) -> str | None:
        explicit = self._option(backend_config, "pythonpath")
        if explicit is not None:
            return explicit

        sudo_user = os.environ.get("SUDO_USER")
        if sudo_user:
            try:
                sudo_home = Path(pwd.getpwnam(sudo_user).pw_dir)
            except KeyError:
                sudo_home = None
            if sudo_home is not None:
                candidate = (
                    sudo_home
                    / ".local"
                    / "lib"
                    / f"python{sys.version_info.major}.{sys.version_info.minor}"
                    / "site-packages"
                )
                if candidate.exists():
                    return str(candidate)
        return None

    def _cache_root(self, config_path: Path, backend_config: BackendConfig) -> Path:
        resolved = self._resolve_path(config_path, self._option(backend_config, "cache_root"))
        if resolved is not None:
            return resolved
        return config_path.parent.parent / ".cache" / "duckdb_tpch"

    def _scale_factor(self, backend_config: BackendConfig) -> str:
        return self._option(backend_config, "scale_factor", "1") or "1"

    def _scale_factor_label(self, backend_config: BackendConfig) -> str:
        return f"SF{self._scale_factor(backend_config)}"

    def _database_path(self, config_path: Path, backend_config: BackendConfig) -> Path:
        cache_root = self._cache_root(config_path, backend_config)
        scale_factor = self._scale_factor(backend_config).replace(".", "_")
        return cache_root / f"tpch_sf{scale_factor}.duckdb"

    def _runner_script(self) -> Path:
        return Path(__file__).resolve().parent / "duckdb_tpch_runner.py"

    def ensure_built(self, ycsb_root: Path, backend_config: BackendConfig, config_path: Path) -> None:
        del ycsb_root
        python_bin = self._python_bin(config_path, backend_config)
        env = os.environ.copy()
        pythonpath = self._pythonpath(backend_config)
        if pythonpath is not None:
            existing = env.get("PYTHONPATH", "")
            env["PYTHONPATH"] = pythonpath if not existing else f"{pythonpath}:{existing}"
        subprocess.run(
            [python_bin, "-c", "import duckdb"],
            check=True,
            capture_output=True,
            text=True,
            env=env,
        )
        database_path = self._database_path(config_path, backend_config)
        if database_path.exists():
            return
        database_path.parent.mkdir(parents=True, exist_ok=True)
        command = [
            python_bin,
            str(self._runner_script()),
            "--prepare-only",
            "--database",
            str(database_path),
            "--scale-factor",
            self._scale_factor(backend_config),
        ]
        memory_limit = self._option(backend_config, "memory_limit")
        if memory_limit is not None:
            command.extend(["--memory-limit", memory_limit])
        subprocess.run(command, check=True, env=env)

    def instance_external_invocation(
        self,
        config_path: Path,
        instance_dir: Path,
        assignment_label: str,
        instance_id: int,
        backend_config: BackendConfig,
    ) -> ExternalInvocation:
        del assignment_label, instance_id
        if backend_config.threads_per_instance != 1:
            raise RuntimeError("backend duckdb_tpch requires threads_per_instance=1")

        python_bin = self._python_bin(config_path, backend_config)
        database_path = self._database_path(config_path, backend_config)
        if not database_path.exists():
            self.ensure_built(Path("."), backend_config, config_path)

        command = [
            python_bin,
            str(self._runner_script()),
            "--database",
            str(database_path),
            "--scale-factor",
            self._scale_factor(backend_config),
            "--queries",
            self._option(backend_config, "queries", "1-22") or "1-22",
            "--repeat",
            self._option(backend_config, "repeat", "1") or "1",
        ]
        memory_limit = self._option(backend_config, "memory_limit")
        if memory_limit is not None:
            command.extend(["--memory-limit", memory_limit])

        env = dict(backend_config.env)
        pythonpath = self._pythonpath(backend_config)
        if pythonpath is not None:
            existing = env.get("PYTHONPATH", "")
            env["PYTHONPATH"] = pythonpath if not existing else f"{pythonpath}:{existing}"

        return ExternalInvocation(
            command=command,
            env=env,
            working_dir=None,
            db_target=str(database_path),
            cleanup_path=instance_dir / "duckdb-runtime",
            benchmark_name="DUCKDB_TPCH",
            benchmark_class=self._scale_factor_label(backend_config),
        )


class LlamaCppAdapter(BackendAdapter):
    name = "llamacpp"
    mode = "analytic"
    execution_model = "per_instance_external"
    requires_ycsb_launcher = False
    requires_workload = False
    supports_load_phase = False

    def default_operationcount(self) -> int:
        return 1

    def _option(self, backend_config: BackendConfig, name: str, default: str | None = None) -> str | None:
        value = backend_config.options.get(name)
        if value is None or not value.strip():
            return default
        return value.strip()

    def _resolve_path(self, config_path: Path, raw: str | None) -> Path | None:
        if raw is None:
            return None
        path = Path(raw).expanduser()
        if not path.is_absolute():
            path = (config_path.parent / path).resolve()
        return path

    def _llama_root(self, config_path: Path, backend_config: BackendConfig) -> Path:
        resolved = self._resolve_path(config_path, self._option(backend_config, "llama_root"))
        if resolved is not None:
            return resolved
        return (config_path.parent.parent / "benchmarks" / "llama.cpp").resolve()

    def _binary_candidates(self, config_path: Path, backend_config: BackendConfig) -> tuple[Path, ...]:
        explicit = self._resolve_path(config_path, self._option(backend_config, "binary"))
        if explicit is not None:
            return (explicit,)
        llama_root = self._llama_root(config_path, backend_config)
        return (
            llama_root / "build" / "bin" / "llama-bench",
            llama_root / "build" / "bin" / "Release" / "llama-bench",
        )

    def _binary_path(self, config_path: Path, backend_config: BackendConfig) -> Path:
        candidates = self._binary_candidates(config_path, backend_config)
        for candidate in candidates:
            if candidate.exists():
                return candidate
        return candidates[0]

    def _model_path(self, config_path: Path, backend_config: BackendConfig) -> Path:
        model = self._resolve_path(config_path, self._option(backend_config, "model"))
        if model is None:
            raise RuntimeError("backend llamacpp requires backend.options.model")
        return model

    def _benchmark_class(self, config_path: Path, backend_config: BackendConfig) -> str:
        explicit = self._option(backend_config, "benchmark_class")
        if explicit is not None:
            return explicit
        return self._model_path(config_path, backend_config).stem

    def ensure_built(self, ycsb_root: Path, backend_config: BackendConfig, config_path: Path) -> None:
        del ycsb_root
        binary = self._binary_path(config_path, backend_config)
        if binary.exists():
            return

        llama_root = self._llama_root(config_path, backend_config)
        if not (llama_root / "CMakeLists.txt").exists():
            raise RuntimeError(f"backend llamacpp requires a llama.cpp checkout: {llama_root}")

        build_dir = llama_root / "build"
        subprocess.run(
            [
                "cmake",
                "-S",
                str(llama_root),
                "-B",
                str(build_dir),
                "-DBUILD_SHARED_LIBS=OFF",
                "-DLLAMA_BUILD_TESTS=OFF",
                "-DLLAMA_BUILD_EXAMPLES=OFF",
                "-DLLAMA_BUILD_SERVER=OFF",
                "-DLLAMA_BUILD_TOOLS=ON",
                "-DLLAMA_OPENSSL=OFF",
                "-DGGML_CUDA=OFF",
            ],
            check=True,
        )
        subprocess.run(
            [
                "cmake",
                "--build",
                str(build_dir),
                "--target",
                "llama-bench",
                "-j",
            ],
            check=True,
        )
        if not self._binary_path(config_path, backend_config).exists():
            raise RuntimeError(f"failed to build llama-bench under {build_dir}")

    def instance_external_invocation(
        self,
        config_path: Path,
        instance_dir: Path,
        assignment_label: str,
        instance_id: int,
        backend_config: BackendConfig,
    ) -> ExternalInvocation:
        del assignment_label, instance_id
        binary = self._binary_path(config_path, backend_config)
        if not binary.exists():
            self.ensure_built(Path("."), backend_config, config_path)

        model_path = self._model_path(config_path, backend_config)
        if not model_path.exists():
            raise RuntimeError(f"llamacpp model not found: {model_path}")

        command = [
            str(binary),
            "--model",
            str(model_path),
            "--threads",
            str(backend_config.threads_per_instance),
            "--n-prompt",
            self._option(backend_config, "n_prompt", "32") or "32",
            "--n-gen",
            self._option(backend_config, "n_gen", "16") or "16",
            "--batch-size",
            self._option(backend_config, "batch_size", "512") or "512",
            "--ubatch-size",
            self._option(backend_config, "ubatch_size", "512") or "512",
            "--repetitions",
            self._option(backend_config, "repetitions", "3") or "3",
            "--n-gpu-layers",
            self._option(backend_config, "n_gpu_layers", "0") or "0",
            "--numa",
            self._option(backend_config, "numa", "numactl") or "numactl",
            "--output",
            "jsonl",
        ]

        if self._option(backend_config, "embeddings", "0") == "1":
            command.extend(["--embeddings", "1"])
        if self._option(backend_config, "use_mmap") is not None:
            command.extend(["--mmap", self._option(backend_config, "use_mmap", "1") or "1"])
        if self._option(backend_config, "cpu_mask") is not None:
            command.extend(["--cpu-mask", self._option(backend_config, "cpu_mask", "0x0") or "0x0"])
        if self._option(backend_config, "args") is not None:
            command.extend(shlex.split(self._option(backend_config, "args", "") or ""))

        return ExternalInvocation(
            command=command,
            env=dict(backend_config.env),
            working_dir=self._llama_root(config_path, backend_config),
            db_target=str(model_path),
            cleanup_path=instance_dir / "llamacpp-runtime",
            benchmark_name="LLAMA_BENCH",
            benchmark_class=self._benchmark_class(config_path, backend_config),
        )


class GapbsPageRankAdapter(BackendAdapter):
    name = "gapbs_pagerank"
    mode = "analytic"
    execution_model = "per_instance_external"
    requires_ycsb_launcher = False
    requires_workload = False
    supports_load_phase = False

    def default_operationcount(self) -> int:
        return 1

    def _option(self, backend_config: BackendConfig, name: str, default: str | None = None) -> str | None:
        value = backend_config.options.get(name)
        if value is None or not value.strip():
            return default
        return value.strip()

    def _resolve_path(self, config_path: Path, raw: str | None) -> Path | None:
        if raw is None:
            return None
        path = Path(raw).expanduser()
        if not path.is_absolute():
            path = (config_path.parent / path).resolve()
        return path

    def _gapbs_root(self, config_path: Path, backend_config: BackendConfig) -> Path:
        resolved = self._resolve_path(config_path, self._option(backend_config, "gapbs_root"))
        if resolved is not None:
            return resolved
        return Path("/home/seunghyun/gapbs/gapbs")

    def _binary_path(self, config_path: Path, backend_config: BackendConfig) -> Path:
        explicit = self._resolve_path(config_path, self._option(backend_config, "binary"))
        if explicit is not None:
            return explicit
        return self._gapbs_root(config_path, backend_config) / "pr"

    def _graph_path(self, config_path: Path, backend_config: BackendConfig) -> Path:
        graph = self._resolve_path(config_path, self._option(backend_config, "graph"))
        if graph is None:
            return self._gapbs_root(config_path, backend_config) / "test" / "graphs" / "4.el"
        return graph

    def _benchmark_class(self, config_path: Path, backend_config: BackendConfig) -> str:
        explicit = self._option(backend_config, "benchmark_class")
        if explicit is not None:
            return explicit
        return self._graph_path(config_path, backend_config).stem

    def ensure_built(self, ycsb_root: Path, backend_config: BackendConfig, config_path: Path) -> None:
        del ycsb_root
        binary = self._binary_path(config_path, backend_config)
        graph = self._graph_path(config_path, backend_config)
        if not binary.exists():
            raise RuntimeError(f"missing GAPBS binary for {self.name}: {binary}")
        if not graph.exists():
            raise RuntimeError(f"missing GAPBS graph for {self.name}: {graph}")

    def instance_external_invocation(
        self,
        config_path: Path,
        instance_dir: Path,
        assignment_label: str,
        instance_id: int,
        backend_config: BackendConfig,
    ) -> ExternalInvocation:
        del assignment_label, instance_id
        if backend_config.threads_per_instance != 1:
            raise RuntimeError("backend gapbs_pagerank requires threads_per_instance=1")

        binary = self._binary_path(config_path, backend_config)
        graph = self._graph_path(config_path, backend_config)
        self.ensure_built(Path("."), backend_config, config_path)

        command = [
            str(binary),
            "-f",
            str(graph),
            "-n",
            self._option(backend_config, "trials", "3") or "3",
            "-i",
            self._option(backend_config, "iterations", "20") or "20",
            "-t",
            self._option(backend_config, "tolerance", "1e-4") or "1e-4",
        ]
        source = self._option(backend_config, "source")
        if source is None:
            source = self._option(backend_config, "source_node")
        if source is not None:
            command.extend(["-r", source])
        if self._option(backend_config, "symmetrize", "0") == "1":
            command.append("-s")
        if self._option(backend_config, "memory_saver", "0") == "1":
            command.append("-m")
        if self._option(backend_config, "analysis", "0") == "1":
            command.append("-a")
        if self._option(backend_config, "verify", "0") == "1":
            command.append("-v")
        if self._option(backend_config, "log", "0") == "1":
            command.append("-l")
        if self._option(backend_config, "args") is not None:
            command.extend(shlex.split(self._option(backend_config, "args", "") or ""))

        env = dict(backend_config.env)
        env.setdefault("OMP_NUM_THREADS", "1")
        env.setdefault("OMP_PROC_BIND", "close")
        env.setdefault("OMP_PLACES", "cores")

        return ExternalInvocation(
            command=command,
            env=env,
            working_dir=self._gapbs_root(config_path, backend_config),
            db_target=str(graph),
            cleanup_path=instance_dir / "gapbs-runtime",
            benchmark_name="GAPBS_PAGERANK",
            benchmark_class=self._benchmark_class(config_path, backend_config),
        )


class GapbsBfsAdapter(GapbsPageRankAdapter):
    name = "gapbs_bfs"

    def _binary_path(self, config_path: Path, backend_config: BackendConfig) -> Path:
        explicit = self._resolve_path(config_path, self._option(backend_config, "binary"))
        if explicit is not None:
            return explicit
        return self._gapbs_root(config_path, backend_config) / "bfs"

    def instance_external_invocation(
        self,
        config_path: Path,
        instance_dir: Path,
        assignment_label: str,
        instance_id: int,
        backend_config: BackendConfig,
    ) -> ExternalInvocation:
        del assignment_label, instance_id
        if backend_config.threads_per_instance != 1:
            raise RuntimeError("backend gapbs_bfs requires threads_per_instance=1")

        binary = self._binary_path(config_path, backend_config)
        graph = self._graph_path(config_path, backend_config)
        self.ensure_built(Path("."), backend_config, config_path)

        command = [
            str(binary),
            "-f",
            str(graph),
            "-n",
            self._option(backend_config, "trials", "3") or "3",
        ]
        source = self._option(backend_config, "source")
        if source is None:
            source = self._option(backend_config, "source_node")
        if source is not None:
            command.extend(["-r", source])
        if self._option(backend_config, "symmetrize", "0") == "1":
            command.append("-s")
        if self._option(backend_config, "memory_saver", "0") == "1":
            command.append("-m")
        if self._option(backend_config, "analysis", "0") == "1":
            command.append("-a")
        if self._option(backend_config, "verify", "0") == "1":
            command.append("-v")
        if self._option(backend_config, "log", "0") == "1":
            command.append("-l")
        if self._option(backend_config, "args") is not None:
            command.extend(shlex.split(self._option(backend_config, "args", "") or ""))

        env = dict(backend_config.env)
        env.setdefault("OMP_NUM_THREADS", "1")
        env.setdefault("OMP_PROC_BIND", "close")
        env.setdefault("OMP_PLACES", "cores")

        return ExternalInvocation(
            command=command,
            env=env,
            working_dir=self._gapbs_root(config_path, backend_config),
            db_target=str(graph),
            cleanup_path=instance_dir / "gapbs-runtime",
            benchmark_name="GAPBS_BFS",
            benchmark_class=self._benchmark_class(config_path, backend_config),
        )


class XSBenchAdapter(BackendAdapter):
    name = "xsbench"
    mode = "analytic"
    execution_model = "per_instance_external"
    requires_ycsb_launcher = False
    requires_workload = False
    supports_load_phase = False

    def default_operationcount(self) -> int:
        return 1

    def _option(self, backend_config: BackendConfig, name: str, default: str | None = None) -> str | None:
        value = backend_config.options.get(name)
        if value is None or not value.strip():
            return default
        return value.strip()

    def _resolve_path(self, config_path: Path, raw: str | None) -> Path | None:
        if raw is None:
            return None
        path = Path(raw).expanduser()
        if not path.is_absolute():
            path = (config_path.parent / path).resolve()
        return path

    def _xsbench_root(self, config_path: Path, backend_config: BackendConfig) -> Path:
        resolved = self._resolve_path(config_path, self._option(backend_config, "xsbench_root"))
        if resolved is not None:
            return resolved
        return (config_path.parent.parent / "benchmarks" / "XSBench" / "openmp-threading").resolve()

    def _binary_path(self, config_path: Path, backend_config: BackendConfig) -> Path:
        explicit = self._resolve_path(config_path, self._option(backend_config, "binary"))
        if explicit is not None:
            return explicit
        return self._xsbench_root(config_path, backend_config) / "XSBench"

    def _benchmark_class(self, backend_config: BackendConfig) -> str:
        explicit = self._option(backend_config, "benchmark_class")
        if explicit is not None:
            return explicit
        method = (self._option(backend_config, "simulation_method", "event") or "event").replace(" ", "_")
        size = self._option(backend_config, "size", "small") or "small"
        grid = (self._option(backend_config, "grid_type", "unionized") or "unionized").replace(" ", "_")
        return f"{method}_{size}_{grid}"

    def ensure_built(self, ycsb_root: Path, backend_config: BackendConfig, config_path: Path) -> None:
        del ycsb_root
        root = self._xsbench_root(config_path, backend_config)
        binary = self._binary_path(config_path, backend_config)
        if binary.exists():
            return
        if not (root / "Makefile").exists():
            raise RuntimeError(f"backend xsbench requires an XSBench openmp-threading checkout: {root}")
        subprocess.run(["make", "-j"], cwd=root, check=True)
        if not binary.exists():
            raise RuntimeError(f"failed to build XSBench under {root}")

    def instance_external_invocation(
        self,
        config_path: Path,
        instance_dir: Path,
        assignment_label: str,
        instance_id: int,
        backend_config: BackendConfig,
    ) -> ExternalInvocation:
        del assignment_label, instance_id
        binary = self._binary_path(config_path, backend_config)
        if not binary.exists():
            self.ensure_built(Path("."), backend_config, config_path)

        simulation_method = self._option(backend_config, "simulation_method", "event") or "event"
        size = self._option(backend_config, "size", "small") or "small"
        grid_type = self._option(backend_config, "grid_type", "unionized") or "unionized"

        command = [
            str(binary),
            "-t",
            str(backend_config.threads_per_instance),
            "-m",
            simulation_method,
            "-s",
            size,
            "-G",
            grid_type,
        ]

        particles = self._option(backend_config, "particles")
        if particles is not None:
            command.extend(["-p", particles])
        lookups = self._option(backend_config, "lookups")
        if lookups is not None:
            command.extend(["-l", lookups])
        gridpoints = self._option(backend_config, "gridpoints")
        if gridpoints is not None:
            command.extend(["-g", gridpoints])
        hash_bins = self._option(backend_config, "hash_bins")
        if hash_bins is not None:
            command.extend(["-h", hash_bins])
        kernel = self._option(backend_config, "kernel")
        if kernel is not None:
            command.extend(["-k", kernel])
        binary_mode = self._option(backend_config, "binary_mode")
        if binary_mode is not None:
            command.extend(["-b", binary_mode])
        if self._option(backend_config, "args") is not None:
            command.extend(shlex.split(self._option(backend_config, "args", "") or ""))

        env = dict(backend_config.env)
        env.setdefault("OMP_NUM_THREADS", str(backend_config.threads_per_instance))
        env.setdefault("OMP_PROC_BIND", "close")
        env.setdefault("OMP_PLACES", "cores")

        return ExternalInvocation(
            command=command,
            env=env,
            working_dir=self._xsbench_root(config_path, backend_config),
            db_target=str(binary),
            cleanup_path=instance_dir / "xsbench-runtime",
            benchmark_name="XSBENCH",
            benchmark_class=self._benchmark_class(backend_config),
        )


class FilebenchFileserverAdapter(BackendAdapter):
    name = "filebench_fileserver"
    mode = "analytic"
    execution_model = "per_instance_external"
    requires_ycsb_launcher = False
    requires_workload = False
    supports_load_phase = False

    def default_operationcount(self) -> int:
        return 1

    def _option(self, backend_config: BackendConfig, name: str, default: str | None = None) -> str | None:
        value = backend_config.options.get(name)
        if value is None or not value.strip():
            return default
        return value.strip()

    def _resolve_path(self, config_path: Path, raw: str | None) -> Path | None:
        if raw is None:
            return None
        path = Path(raw).expanduser()
        if not path.is_absolute():
            path = (config_path.parent / path).resolve()
        return path

    def _filebench_root(self, config_path: Path, backend_config: BackendConfig) -> Path:
        resolved = self._resolve_path(config_path, self._option(backend_config, "filebench_root"))
        if resolved is not None:
            return resolved
        return (config_path.parent.parent / "benchmarks" / "filebench").resolve()

    def _binary_path(self, config_path: Path, backend_config: BackendConfig) -> Path:
        explicit = self._resolve_path(config_path, self._option(backend_config, "binary"))
        if explicit is not None:
            return explicit
        return self._filebench_root(config_path, backend_config) / "filebench"

    def _template_path(self, config_path: Path, backend_config: BackendConfig) -> Path:
        explicit = self._resolve_path(config_path, self._option(backend_config, "template"))
        if explicit is not None:
            return explicit
        return self._filebench_root(config_path, backend_config) / "workloads" / "fileserver.f"

    def _benchmark_class(self, backend_config: BackendConfig) -> str:
        return self._option(backend_config, "benchmark_class", "fileserver") or "fileserver"

    def _cvar_runtime_dir(self, filebench_root: Path) -> Path:
        return filebench_root / "cvars" / ".libs" / "filebench"

    def _configured_libdir(self, filebench_root: Path) -> str | None:
        makefile = filebench_root / "Makefile"
        if not makefile.exists():
            return None
        libdir_value: str | None = None
        defs_value: str | None = None
        for line in makefile.read_text(encoding="utf-8", errors="replace").splitlines():
            if line.startswith("libdir = "):
                libdir_value = line.split("=", 1)[1].strip()
            if line.startswith("DEFS = ") and "FBLIBDIR=" in line:
                marker = '-DFBLIBDIR=\\"'
                start = line.find(marker)
                if start == -1:
                    continue
                start += len(marker)
                end = line.find('\\"', start)
                if end == -1:
                    continue
                defs_value = line[start:end]
        if defs_value == "$(libdir)":
            return libdir_value
        return defs_value

    def ensure_built(self, ycsb_root: Path, backend_config: BackendConfig, config_path: Path) -> None:
        del ycsb_root
        filebench_root = self._filebench_root(config_path, backend_config)
        binary = self._binary_path(config_path, backend_config)
        template = self._template_path(config_path, backend_config)
        desired_libdir = str(self._cvar_runtime_dir(filebench_root).resolve())
        configured_libdir = self._configured_libdir(filebench_root)
        cvar_runtime_dir = self._cvar_runtime_dir(filebench_root)
        cvar_plugin = cvar_runtime_dir / "libcvar-gamma.so"
        if binary.exists() and template.exists() and configured_libdir == desired_libdir and cvar_plugin.exists():
            return
        if not (filebench_root / "configure.ac").exists():
            raise RuntimeError(f"backend filebench_fileserver requires a filebench checkout: {filebench_root}")
        subprocess.run(["make", "distclean"], cwd=filebench_root, check=False)
        subprocess.run(["libtoolize"], cwd=filebench_root, check=True)
        subprocess.run(["aclocal"], cwd=filebench_root, check=True)
        subprocess.run(["autoheader"], cwd=filebench_root, check=True)
        subprocess.run(["automake", "--add-missing"], cwd=filebench_root, check=True)
        subprocess.run(["autoconf"], cwd=filebench_root, check=True)
        subprocess.run(["./configure", f"--libdir={(filebench_root / 'cvars' / '.libs').resolve()}"], cwd=filebench_root, check=True)
        subprocess.run(["make", "-j"], cwd=filebench_root, check=True)
        cvar_runtime_dir.mkdir(parents=True, exist_ok=True)
        for plugin in (filebench_root / "cvars" / ".libs").glob("libcvar-*.so*"):
            destination = cvar_runtime_dir / plugin.name
            if destination.exists() or destination.is_symlink():
                destination.unlink()
            destination.symlink_to(plugin)
        if not binary.exists():
            raise RuntimeError(f"failed to build filebench binary: {binary}")
        if not template.exists():
            raise RuntimeError(f"missing filebench fileserver template: {template}")
        if self._option(backend_config, "setarch", "1") != "0" and shutil.which("setarch") is None:
            raise RuntimeError("backend filebench_fileserver requires `setarch` in PATH")

    def _render_workload(self, config_path: Path, instance_dir: Path, backend_config: BackendConfig) -> Path:
        template_path = self._template_path(config_path, backend_config)
        if not template_path.exists():
            raise RuntimeError(f"missing filebench fileserver template: {template_path}")

        data_dir = instance_dir / "filebench-fileserver"
        rendered_path = instance_dir / "fileserver.generated.f"
        replacements = {"set $dir=": f"set $dir={data_dir}"}
        optional_replacements = {
            "set $nfiles=": self._option(backend_config, "nfiles"),
            "set $meandirwidth=": self._option(backend_config, "meandirwidth"),
            "set $filesize=": self._option(backend_config, "filesize"),
            "set $nthreads=": self._option(backend_config, "nthreads"),
            "set $iosize=": self._option(backend_config, "iosize"),
            "set $meanappendsize=": self._option(backend_config, "meanappendsize"),
            "set $runtime=": self._option(backend_config, "runtime"),
        }
        for prefix, value in optional_replacements.items():
            if value is not None:
                replacements[prefix] = f"{prefix}{value}"

        rendered_lines: list[str] = []
        for raw_line in template_path.read_text(encoding="utf-8", errors="replace").splitlines():
            line = raw_line
            for prefix, replacement in replacements.items():
                if line.startswith(prefix):
                    line = replacement
                    break
            rendered_lines.append(line)
        if self._option(backend_config, "latency_histogram", "1") != "0":
            inserted = False
            for index, line in enumerate(rendered_lines):
                if line.strip().startswith("run "):
                    rendered_lines.insert(index, "enable lathist")
                    inserted = True
                    break
            if not inserted:
                rendered_lines.append("enable lathist")
        rendered_path.write_text("\n".join(rendered_lines) + "\n", encoding="utf-8")
        return rendered_path

    def instance_external_invocation(
        self,
        config_path: Path,
        instance_dir: Path,
        assignment_label: str,
        instance_id: int,
        backend_config: BackendConfig,
    ) -> ExternalInvocation:
        del assignment_label, instance_id
        if backend_config.threads_per_instance != 1:
            raise RuntimeError("backend filebench_fileserver requires threads_per_instance=1")

        binary = self._binary_path(config_path, backend_config)
        if not binary.exists():
            self.ensure_built(Path("."), backend_config, config_path)
        workload_path = self._render_workload(config_path, instance_dir, backend_config)

        env = dict(backend_config.env)
        env.setdefault("OMP_NUM_THREADS", "1")
        env.setdefault("OMP_PROC_BIND", "close")
        env.setdefault("OMP_PLACES", "cores")

        if self._option(backend_config, "setarch", "1") != "0":
            arch = self._option(backend_config, "arch", os.uname().machine) or os.uname().machine
            command = ["setarch", arch, "-R", str(binary), "-f", str(workload_path)]
        else:
            command = [str(binary), "-f", str(workload_path)]
        extra_args = self._option(backend_config, "args")
        if extra_args is not None:
            command.extend(shlex.split(extra_args))

        return ExternalInvocation(
            command=command,
            env=env,
            working_dir=self._filebench_root(config_path, backend_config),
            db_target=str(instance_dir / "filebench-fileserver"),
            cleanup_path=instance_dir / "filebench-fileserver",
            benchmark_name="FILEBENCH_FILESERVER",
            benchmark_class=self._benchmark_class(backend_config),
        )


BACKEND_ADAPTERS: dict[str, BackendAdapter] = {
    "rocksdb": RocksDbAdapter(),
    "orientdb": OrientDbAdapter(),
    "elasticsearch": ElasticsearchAdapter(),
    "npb_omp": NpbOmpAdapter(),
    "npb_instances": NpbInstancesAdapter(),
    "duckdb_tpch": DuckDbTpchAdapter(),
    "llamacpp": LlamaCppAdapter(),
    "gapbs_pagerank": GapbsPageRankAdapter(),
    "gapbs_bfs": GapbsBfsAdapter(),
    "xsbench": XSBenchAdapter(),
    "filebench_fileserver": FilebenchFileserverAdapter(),
}


def get_backend_adapter(name: str) -> BackendAdapter:
    try:
        return BACKEND_ADAPTERS[name]
    except KeyError as exc:
        raise ValueError(f"unsupported backend: {name}") from exc
