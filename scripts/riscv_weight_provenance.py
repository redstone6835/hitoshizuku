#!/usr/bin/env python3
"""RISC-V 指令权重实验产物的严格来源链与离线复验。"""

from __future__ import annotations

import argparse
import hashlib
import hmac
import json
import re
import stat
import tempfile
from collections.abc import Mapping, MutableMapping, Sequence
from pathlib import Path, PurePosixPath
from typing import Any

from riscv_weight_host_telemetry import (
    TelemetryError,
    audit as audit_host_telemetry,
    verify_binding as verify_host_audit_binding,
)
from riscv_weight_model_seal import verify_model_document_seal


MANIFEST_SCHEMA = "mygo.riscv-instruction-weight-provenance.v1"
MODEL_BINDING_SCHEMA = "mygo.riscv-instruction-weight-model-provenance.v1"
CANONICALIZATION = "utf8-json-sort-keys-compact-no-nan-v1"
MODEL_FIELD = "artifact_provenance"
SHA256_RE = re.compile(r"[0-9a-f]{64}")
CHECKSUM_LINE_RE = re.compile(r"([0-9a-f]{64}) [ *](.+)")

REQUIRED_ARTIFACTS = frozenset(
    {
        "kernel",
        "probe",
        "plugin",
        "qemu_version",
        "qemu_binary_checksum",
        "artifact_checksums",
        "samples",
        "run_design",
        "host_telemetry",
        "host_audit",
        "host_audit_binding",
        "weights_pre_finalization",
        "ml_validation",
    }
)
OPTIONAL_ARTIFACTS = frozenset({"isolation_state"})
BUILD_CHECKSUM_ROLES = ("kernel", "probe", "plugin")
HOST_AUDIT_THRESHOLD_ARGUMENTS = {
    "max_sibling_busy": "max_sibling_busy",
    "max_load_per_cpu": "max_load_per_cpu",
    "min_frequency_ratio": "min_frequency_ratio",
    "require_frequency_floor": "require_frequency_floor",
    "require_window_frequency": "require_window_frequency",
    "min_window_aperf_mperf_ratio": "min_window_frequency_ratio",
    "require_frequency_preflight": "require_frequency_preflight",
    "min_window_to_preflight_frequency_ratio": "min_window_to_preflight_ratio",
    "max_frequency_preflight_age_seconds": "max_frequency_preflight_age_seconds",
    "max_window_frequency_coefficient_of_variation": "max_window_frequency_cv",
    "max_selected_cpu_interrupts_per_second": "max_interrupts_per_second",
    "require_interrupt_evidence": "require_interrupts",
    "require_schedstat": "require_schedstat",
    "max_runqueue_wait_fraction": "max_runqueue_wait_fraction",
    "max_temperature_span_c": "max_temperature_span",
    "max_temperature_c": "max_temperature",
    "min_selected_cpu_busy": "min_selected_busy",
    "max_cpu_psi_stall_fraction": "max_cpu_psi",
    "max_memory_psi_stall_fraction": "max_memory_psi",
    "require_psi": "require_psi",
    "min_mem_available_kib": "min_mem_available_kib",
}


class ProvenanceError(ValueError):
    """来源链缺失、含歧义，或任一外部产物已漂移。"""


def _canonical_bytes(value: Any) -> bytes:
    try:
        return json.dumps(
            value,
            ensure_ascii=False,
            sort_keys=True,
            separators=(",", ":"),
            allow_nan=False,
        ).encode("utf-8")
    except (TypeError, ValueError) as error:
        raise ProvenanceError("provenance 不能规范化为有限 JSON") from error


def _sha256_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def _safe_regular_file(path: Path, root: Path) -> tuple[Path, str]:
    root = root.resolve(strict=True)
    try:
        metadata = path.lstat()
    except OSError as error:
        raise ProvenanceError(f"产物不可读：{path}") from error
    if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISREG(metadata.st_mode):
        raise ProvenanceError(f"产物必须是非符号链接普通文件：{path}")
    resolved = path.resolve(strict=True)
    try:
        relative = resolved.relative_to(root)
    except ValueError as error:
        raise ProvenanceError(f"产物越出 provenance root：{path}") from error
    relative_text = relative.as_posix()
    if relative_text in {"", "."} or PurePosixPath(relative_text).is_absolute():
        raise ProvenanceError(f"产物相对路径非法：{path}")
    return resolved, relative_text


def artifact_identity(path: str | Path, root: str | Path) -> dict[str, Any]:
    resolved, relative = _safe_regular_file(Path(path), Path(root))
    digest = hashlib.sha256()
    size = 0
    with resolved.open("rb") as stream:
        while chunk := stream.read(1024 * 1024):
            digest.update(chunk)
            size += len(chunk)
    return {"path": relative, "sha256": digest.hexdigest(), "size": size}


def _validate_identity(value: Any, owner: str) -> dict[str, Any]:
    if not isinstance(value, Mapping) or set(value) != {"path", "sha256", "size"}:
        raise ProvenanceError(f"{owner} identity 字段不完整")
    path = value.get("path")
    digest = value.get("sha256")
    size = value.get("size")
    if (
        not isinstance(path, str)
        or not path
        or PurePosixPath(path).is_absolute()
        or ".." in PurePosixPath(path).parts
        or not isinstance(digest, str)
        or SHA256_RE.fullmatch(digest) is None
        or isinstance(size, bool)
        or not isinstance(size, int)
        or size < 0
    ):
        raise ProvenanceError(f"{owner} identity 非法")
    return {"path": path, "sha256": digest, "size": size}


def _identity_matches(left: Mapping[str, Any], right: Mapping[str, Any]) -> bool:
    return (
        left.get("sha256") == right.get("sha256")
        and left.get("size") == right.get("size")
    )


def _json_object(path: Path, owner: str) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise ProvenanceError(f"{owner} 不是有效 JSON：{path}") from error
    if not isinstance(value, dict):
        raise ProvenanceError(f"{owner} 必须是 JSON object")
    return value


def _jsonl(path: Path, owner: str) -> list[dict[str, Any]]:
    rows: list[dict[str, Any]] = []
    try:
        with path.open(encoding="utf-8") as stream:
            for line_number, raw in enumerate(stream, 1):
                if not raw.strip():
                    raise ProvenanceError(f"{owner} 第 {line_number} 行为空")
                value = json.loads(raw)
                if not isinstance(value, dict):
                    raise ProvenanceError(f"{owner} 第 {line_number} 行不是 object")
                rows.append(value)
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise ProvenanceError(f"{owner} 不是有效 JSONL：{path}") from error
    if not rows:
        raise ProvenanceError(f"{owner} 不能为空")
    return rows


def _artifact_path(root: Path, identity: Mapping[str, Any], owner: str) -> Path:
    validated = _validate_identity(identity, owner)
    candidate = root / str(validated["path"])
    actual = artifact_identity(candidate, root)
    if actual != validated:
        raise ProvenanceError(f"{owner} SHA-256/size/path 与磁盘产物不一致")
    return candidate.resolve(strict=True)


def _parse_sha256sum(path: Path, owner: str) -> list[tuple[str, str]]:
    result: list[tuple[str, str]] = []
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except (OSError, UnicodeError) as error:
        raise ProvenanceError(f"{owner} 不可读") from error
    if not lines or any(not line for line in lines):
        raise ProvenanceError(f"{owner} 不能为空或含空行")
    for index, line in enumerate(lines, 1):
        match = CHECKSUM_LINE_RE.fullmatch(line)
        if match is None or "\x00" in match.group(2):
            raise ProvenanceError(f"{owner} 第 {index} 行不是严格 sha256sum 格式")
        result.append((match.group(1), match.group(2)))
    return result


def _checksum_path_key(path: str, root: Path) -> str:
    candidate = Path(path)
    if candidate.is_absolute():
        try:
            return candidate.resolve(strict=False).relative_to(root.resolve()).as_posix()
        except ValueError as error:
            raise ProvenanceError(f"checksum 路径越出 provenance root：{path}") from error
    normalized = PurePosixPath(path)
    if ".." in normalized.parts:
        raise ProvenanceError(f"checksum 路径含上级跳转：{path}")
    return normalized.as_posix()


def _build_checksum_relationship(
    paths: Mapping[str, Path], identities: Mapping[str, Mapping[str, Any]], root: Path
) -> dict[str, Any]:
    entries = _parse_sha256sum(paths["artifact_checksums"], "artifacts.sha256")
    indexed: dict[str, str] = {}
    for digest, raw_path in entries:
        key = _checksum_path_key(raw_path, root)
        if key in indexed:
            raise ProvenanceError(f"artifacts.sha256 重复路径：{key}")
        indexed[key] = digest
    expected = {str(identities[role]["path"]): str(identities[role]["sha256"]) for role in BUILD_CHECKSUM_ROLES}
    if indexed != expected:
        raise ProvenanceError("artifacts.sha256 未精确绑定 kernel/probe/plugin")

    qemu_entries = _parse_sha256sum(
        paths["qemu_binary_checksum"], "qemu binary checksum"
    )
    if len(qemu_entries) != 1:
        raise ProvenanceError("qemu binary checksum 必须恰含一个条目")
    qemu_digest, qemu_path = qemu_entries[0]
    if not Path(qemu_path).is_absolute() or not qemu_path.endswith("qemu-system-riscv64"):
        raise ProvenanceError("qemu binary checksum 必须记录绝对 qemu-system-riscv64 路径")
    version_lines = paths["qemu_version"].read_text(encoding="utf-8").splitlines()
    if not version_lines or "QEMU emulator version" not in version_lines[0]:
        raise ProvenanceError("qemu-version.txt 缺少 QEMU emulator version 首行")
    return {
        "build_artifacts": expected,
        "qemu_binary": {"path": qemu_path, "sha256": qemu_digest},
        "qemu_version_first_line": version_lines[0],
    }


def _binding_identity_matches(value: Any, identity: Mapping[str, Any]) -> bool:
    return (
        isinstance(value, Mapping)
        and value.get("sha256") == identity.get("sha256")
        and (
            "size" not in value
            or value.get("size") == identity.get("size")
        )
    )


def _strict_embedded_identity_matches(
    value: Any, identity: Mapping[str, Any]
) -> bool:
    """审计 JSON 使用 path+sha256；path 可随整个 acquisition 目录搬移。"""

    return (
        isinstance(value, Mapping)
        and set(value) == {"path", "sha256"}
        and isinstance(value.get("path"), str)
        and bool(value.get("path"))
        and value.get("sha256") == identity.get("sha256")
    )


def _normalize_embedded_input_paths(document: Mapping[str, Any]) -> dict[str, Any]:
    """路径可随 acquisition root 搬移，内容身份与其余审计结果不可变化。"""

    normalized = dict(document)
    inputs = document.get("inputs")
    if isinstance(inputs, Mapping):
        normalized["inputs"] = {
            name: (
                {key: value for key, value in identity.items() if key != "path"}
                if isinstance(identity, Mapping)
                else identity
            )
            for name, identity in inputs.items()
        }
    return normalized


def _replay_host_audit(
    paths: Mapping[str, Path], audit: Mapping[str, Any], isolation_path: Path | None
) -> dict[str, Any]:
    thresholds = audit.get("thresholds")
    if (
        not isinstance(thresholds, Mapping)
        or set(thresholds) != set(HOST_AUDIT_THRESHOLD_ARGUMENTS)
    ):
        raise ProvenanceError("host audit thresholds 字段集合不符合重放契约")
    require_isolation = audit.get("isolation_state_checks_required")
    if not isinstance(require_isolation, bool):
        raise ProvenanceError("host audit isolation_state_checks_required 非布尔值")

    arguments: dict[str, Any] = {
        argument: thresholds[field]
        for field, argument in HOST_AUDIT_THRESHOLD_ARGUMENTS.items()
    }
    arguments.update(
        {
            "input": str(paths["host_telemetry"]),
            "run_design": str(paths["run_design"]),
            "isolation_state": (
                None if isolation_path is None else str(isolation_path)
            ),
            "require_isolation_state": require_isolation,
        }
    )
    try:
        with tempfile.TemporaryDirectory(prefix="riscv-weight-host-replay-") as directory:
            output = Path(directory) / "host-audit.json"
            arguments["output"] = str(output)
            status = audit_host_telemetry(argparse.Namespace(**arguments))
            replayed = _json_object(output, "重放 host audit")
    except (OSError, TelemetryError, ValueError) as error:
        raise ProvenanceError(f"host audit 不能从原始宿主证据重放：{error}") from error
    if status != 0 or replayed.get("status") != "accepted":
        raise ProvenanceError("host audit 从 telemetry/run-design/isolation-state 重放后未通过")
    if _canonical_bytes(_normalize_embedded_input_paths(replayed)) != _canonical_bytes(
        _normalize_embedded_input_paths(audit)
    ):
        raise ProvenanceError("host audit 与原始宿主证据的重放结果不一致")
    return replayed


def _replay_host_audit_binding(
    paths: Mapping[str, Path], binding: Mapping[str, Any]
) -> dict[str, Any]:
    try:
        with tempfile.TemporaryDirectory(prefix="riscv-weight-binding-replay-") as directory:
            output = Path(directory) / "host-audit-binding.json"
            status = verify_host_audit_binding(
                argparse.Namespace(
                    audit=str(paths["host_audit"]),
                    input=str(paths["host_telemetry"]),
                    run_design=str(paths["run_design"]),
                    source="current",
                    output=str(output),
                )
            )
            replayed = _json_object(output, "重放 host audit binding")
    except (OSError, TelemetryError, ValueError) as error:
        raise ProvenanceError(f"host audit binding 不能从当前证据重放：{error}") from error
    if status != 0 or replayed.get("publication_allowed") is not True:
        raise ProvenanceError("host audit binding 从当前证据重放后未通过")
    if _canonical_bytes(_normalize_embedded_input_paths(replayed)) != _canonical_bytes(
        _normalize_embedded_input_paths(binding)
    ):
        raise ProvenanceError("host audit binding 与当前证据的重放结果不一致")
    return replayed


def _expected_launch_manifest(rows: Sequence[Mapping[str, Any]]) -> dict[str, Any]:
    expected: dict[str, Any] = {}
    run_ids: set[str] = set()
    run_orders: set[int] = set()
    super_rows: dict[str, list[Mapping[str, Any]]] = {}
    required = {
        "run_id", "run_order", "super_run_id", "super_run_order",
        "crossover_pair", "crossover_design", "timing_launch_position",
        "plugin_off_launch_position",
    }
    for index, row in enumerate(rows):
        if set(row) != required:
            raise ProvenanceError(f"run-design[{index}] 字段集合不符合严格契约")
        run_id = row["run_id"]
        super_id = row["super_run_id"]
        integer_names = (
            "run_order", "super_run_order", "crossover_pair",
            "timing_launch_position", "plugin_off_launch_position",
        )
        if not isinstance(run_id, str) or not run_id or not isinstance(super_id, str) or not super_id:
            raise ProvenanceError("run-design 标识符非法")
        if any(isinstance(row[name], bool) or not isinstance(row[name], int) for name in integer_names):
            raise ProvenanceError("run-design 整数字段非法")
        if run_id in run_ids or row["run_order"] in run_orders:
            raise ProvenanceError("run-design run_id/run_order 不唯一")
        if row["crossover_pair"] not in {1, 2} or row["crossover_design"] not in {"ABBA", "BAAB"}:
            raise ProvenanceError("run-design crossover 字段非法")
        run_ids.add(run_id)
        run_orders.add(row["run_order"])
        super_rows.setdefault(super_id, []).append(row)
        for mode, position in (
            ("timing", row["timing_launch_position"]),
            ("plugin-off", row["plugin_off_launch_position"]),
        ):
            launch_id = f"{super_id}-{position}-{mode}"
            if launch_id in expected:
                raise ProvenanceError("run-design launch_id 不唯一")
            expected[launch_id] = {
                "launch_id": launch_id,
                "super_run_id": super_id,
                "run_id": run_id,
                "mode": mode,
                "launch_position": position,
                "crossover_pair": row["crossover_pair"],
                "crossover_design": row["crossover_design"],
                "super_run_order": row["super_run_order"],
                "run_order": row["run_order"],
            }
    if sorted(run_orders) != list(range(len(rows))):
        raise ProvenanceError("run-design run_order 必须从 0 连续")
    super_orders: list[int] = []
    for super_id, members in super_rows.items():
        if len(members) != 2 or {row["crossover_pair"] for row in members} != {1, 2}:
            raise ProvenanceError(f"super-run={super_id} 未闭合两个 crossover pair")
        designs = {row["crossover_design"] for row in members}
        orders = {row["super_run_order"] for row in members}
        if len(designs) != 1 or len(orders) != 1:
            raise ProvenanceError(f"super-run={super_id} 设计/order 不一致")
        super_orders.append(next(iter(orders)))
        design = next(iter(designs))
        actual = {
            (mode, position)
            for row in members
            for mode, position in (
                ("timing", row["timing_launch_position"]),
                ("plugin-off", row["plugin_off_launch_position"]),
            )
        }
        wanted = (
            {("timing", 1), ("plugin-off", 2), ("plugin-off", 3), ("timing", 4)}
            if design == "ABBA"
            else {("plugin-off", 1), ("timing", 2), ("timing", 3), ("plugin-off", 4)}
        )
        if actual != wanted:
            raise ProvenanceError(f"super-run={super_id} 不符合 {design}")
    if sorted(super_orders) != list(range(len(super_rows))):
        raise ProvenanceError("super_run_order 必须从 0 连续")
    return expected


def _collection_relationship(
    paths: Mapping[str, Path], identities: Mapping[str, Mapping[str, Any]]
) -> dict[str, Any]:
    design_rows = _jsonl(paths["run_design"], "run-design")
    expected_launches = _expected_launch_manifest(design_rows)
    design_by_run = {row["run_id"]: row for row in design_rows}
    samples = _jsonl(paths["samples"], "samples")
    sample_runs: set[str] = set()
    for index, row in enumerate(samples):
        run_id = row.get("run_id")
        if run_id not in design_by_run:
            raise ProvenanceError(f"samples[{index}] run_id 不在 run-design")
        design = design_by_run[str(run_id)]
        for name in (
            "run_order", "super_run_id", "super_run_order", "crossover_pair",
            "crossover_design", "timing_launch_position",
            "plugin_off_launch_position",
        ):
            if row.get(name) != design[name]:
                raise ProvenanceError(f"samples[{index}].{name} 与 run-design 不一致")
        sample_runs.add(str(run_id))
    if sample_runs != set(design_by_run):
        raise ProvenanceError("samples 未完整覆盖 run-design")

    audit = _json_object(paths["host_audit"], "host audit")
    binding = _json_object(paths["host_audit_binding"], "host audit binding")
    if (
        audit.get("schema") != "mygo.riscv-weight-host-audit.v1"
        or audit.get("status") != "accepted"
        or audit.get("failures") != []
    ):
        raise ProvenanceError("host audit 未严格 accepted")
    audit_inputs = audit.get("inputs")
    if (
        not isinstance(audit_inputs, Mapping)
        or set(audit_inputs) != {"telemetry", "run_design", "isolation_state"}
    ):
        raise ProvenanceError("host audit 缺少 inputs")
    for name, role in (("telemetry", "host_telemetry"), ("run_design", "run_design")):
        if not _strict_embedded_identity_matches(
            audit_inputs.get(name), identities[role]
        ):
            raise ProvenanceError(f"host audit 未绑定当前 {name}")
    isolation_required = audit.get("isolation_state_checks_required") is True
    isolation_input = audit_inputs.get("isolation_state")
    if isolation_input is None:
        if isolation_required:
            raise ProvenanceError("host audit 要求但未绑定当前 isolation_state")
        isolation_path = None
    elif "isolation_state" not in identities or not _strict_embedded_identity_matches(
        isolation_input, identities["isolation_state"]
    ):
        raise ProvenanceError("host audit 未绑定当前 isolation_state")
    else:
        isolation_path = paths["isolation_state"]
    _replay_host_audit(paths, audit, isolation_path)

    checks = binding.get("checks")
    if (
        binding.get("schema") != "mygo.riscv-weight-host-audit-binding.v1"
        or binding.get("source") != "current"
        or binding.get("publication_allowed") is not True
        or not isinstance(checks, Mapping)
        or not checks
        or not all(value is True for value in checks.values())
        or binding.get("failures") != []
    ):
        raise ProvenanceError("host audit binding 未严格通过")
    binding_inputs = binding.get("inputs")
    if (
        not isinstance(binding_inputs, Mapping)
        or set(binding_inputs) != {"audit", "telemetry", "run_design"}
    ):
        raise ProvenanceError("host audit binding 缺少 inputs")
    for name, role in (
        ("audit", "host_audit"),
        ("telemetry", "host_telemetry"),
        ("run_design", "run_design"),
    ):
        if not _strict_embedded_identity_matches(
            binding_inputs.get(name), identities[role]
        ):
            raise ProvenanceError(f"host audit binding 未绑定当前 {name}")
    launch_hash = _sha256_bytes(
        json.dumps(expected_launches, sort_keys=True, separators=(",", ":"), ensure_ascii=True).encode()
    )
    if binding.get("launch_manifest_sha256") != launch_hash:
        raise ProvenanceError("host audit binding launch manifest 与 run-design 不一致")
    _replay_host_audit_binding(paths, binding)
    return {
        "run_count": len(design_rows),
        "sample_count": len(samples),
        "launch_count": len(expected_launches),
        "launch_manifest_sha256": launch_hash,
        "isolation_state_required": isolation_required,
    }


def _ml_relationship(
    paths: Mapping[str, Path], identities: Mapping[str, Mapping[str, Any]]
) -> dict[str, Any]:
    validation = _json_object(paths["ml_validation"], "ML validation")
    bindings = validation.get("input_bindings")
    if validation.get("schema") != "mygo.riscv-instruction-ml-validation.v3" or not isinstance(bindings, Mapping):
        raise ProvenanceError("ML validation schema/input_bindings 不符合契约")
    for name, role in (
        ("samples", "samples"),
        ("statistical_weights_pre_finalization", "weights_pre_finalization"),
    ):
        if not _binding_identity_matches(bindings.get(name), identities[role]):
            raise ProvenanceError(f"ML validation 未绑定当前 {name}")
    return {
        "schema": validation["schema"],
        "samples_sha256": identities["samples"]["sha256"],
        "weights_pre_finalization_sha256": identities["weights_pre_finalization"]["sha256"],
    }


def _relationships(
    paths: Mapping[str, Path], identities: Mapping[str, Mapping[str, Any]], root: Path
) -> dict[str, Any]:
    return {
        "build": _build_checksum_relationship(paths, identities, root),
        "collection": _collection_relationship(paths, identities),
        "ml": _ml_relationship(paths, identities),
    }


def create_manifest(
    *, root: str | Path, artifacts: Mapping[str, str | Path]
) -> dict[str, Any]:
    root_path = Path(root).resolve(strict=True)
    roles = set(artifacts)
    if not REQUIRED_ARTIFACTS.issubset(roles) or not roles.issubset(
        REQUIRED_ARTIFACTS | OPTIONAL_ARTIFACTS
    ):
        missing = sorted(REQUIRED_ARTIFACTS - roles)
        extra = sorted(roles - REQUIRED_ARTIFACTS - OPTIONAL_ARTIFACTS)
        raise ProvenanceError(f"artifact roles 非法：missing={missing}, extra={extra}")
    paths: dict[str, Path] = {}
    identities: dict[str, dict[str, Any]] = {}
    seen_paths: set[str] = set()
    for role in sorted(roles):
        identity = artifact_identity(artifacts[role], root_path)
        if identity["path"] in seen_paths:
            raise ProvenanceError("不同 artifact role 不得复用同一文件")
        seen_paths.add(str(identity["path"]))
        identities[role] = identity
        paths[role] = root_path / str(identity["path"])
    relationships = _relationships(paths, identities, root_path)
    payload = {
        "schema": MANIFEST_SCHEMA,
        "canonicalization": CANONICALIZATION,
        "artifacts": identities,
        "relationships": relationships,
    }
    payload["chain_sha256"] = _sha256_bytes(_canonical_bytes(payload))
    return payload


def verify_manifest(
    manifest: Mapping[str, Any], *, root: str | Path, rehash: bool = True
) -> dict[str, Any]:
    if set(manifest) != {
        "schema", "canonicalization", "artifacts", "relationships", "chain_sha256"
    }:
        raise ProvenanceError("provenance manifest 顶层字段集合不符合契约")
    if manifest.get("schema") != MANIFEST_SCHEMA or manifest.get("canonicalization") != CANONICALIZATION:
        raise ProvenanceError("provenance manifest 协议不受支持")
    artifacts = manifest.get("artifacts")
    roles = set(artifacts) if isinstance(artifacts, Mapping) else set()
    if not REQUIRED_ARTIFACTS.issubset(roles) or not roles.issubset(REQUIRED_ARTIFACTS | OPTIONAL_ARTIFACTS):
        raise ProvenanceError("provenance manifest artifact roles 不完整")
    identities = {
        role: _validate_identity(identity, f"artifacts.{role}")
        for role, identity in artifacts.items()  # type: ignore[union-attr]
    }
    if len({identity["path"] for identity in identities.values()}) != len(identities):
        raise ProvenanceError("provenance manifest artifact 路径不唯一")
    claimed_chain = manifest.get("chain_sha256")
    if not isinstance(claimed_chain, str) or SHA256_RE.fullmatch(claimed_chain) is None:
        raise ProvenanceError("provenance manifest chain_sha256 非法")
    unsigned = {key: value for key, value in manifest.items() if key != "chain_sha256"}
    if not hmac.compare_digest(_sha256_bytes(_canonical_bytes(unsigned)), claimed_chain):
        raise ProvenanceError("provenance manifest chain_sha256 不一致")
    if rehash:
        root_path = Path(root).resolve(strict=True)
        paths = {
            role: _artifact_path(root_path, identity, f"artifacts.{role}")
            for role, identity in identities.items()
        }
        actual_relationships = _relationships(paths, identities, root_path)
        if _canonical_bytes(actual_relationships) != _canonical_bytes(manifest.get("relationships")):
            raise ProvenanceError("provenance manifest relationships 与产物重放不一致")
    return dict(manifest)


def write_manifest(
    output: str | Path, *, root: str | Path, artifacts: Mapping[str, str | Path]
) -> dict[str, Any]:
    output_path = Path(output)
    manifest = create_manifest(root=root, artifacts=artifacts)
    output_path.write_text(
        json.dumps(manifest, ensure_ascii=False, indent=2, sort_keys=True, allow_nan=False) + "\n",
        encoding="utf-8",
    )
    return manifest


def _verify_model_relationships(
    document: Mapping[str, Any], manifest: Mapping[str, Any], root: Path
) -> None:
    artifacts = manifest["artifacts"]
    audit = _json_object(root / artifacts["host_audit"]["path"], "host audit")
    binding = _json_object(
        root / artifacts["host_audit_binding"]["path"], "host audit binding"
    )
    if document.get("host_isolation_audit") != audit:
        raise ProvenanceError("final weights 内嵌 host audit 与来源产物不一致")
    if document.get("host_isolation_audit_binding") != binding:
        raise ProvenanceError("final weights 内嵌 host audit binding 与来源产物不一致")
    if document.get("host_isolation_audit_source") != "current":
        raise ProvenanceError("final weights host audit 不是 current acquisition")
    evidence = document.get("ml_validation_evidence")
    validation = document.get("ml_validation")
    validation_artifact = _json_object(
        root / artifacts["ml_validation"]["path"], "ML validation artifact"
    )
    if not isinstance(evidence, Mapping) or not isinstance(validation, Mapping):
        raise ProvenanceError("final weights 缺少 ML evidence")
    external_bindings = validation_artifact.get("input_bindings")
    if (
        validation_artifact.get("schema")
        != "mygo.riscv-instruction-ml-validation.v3"
        or not isinstance(external_bindings, Mapping)
        or validation.get("schema") != validation_artifact.get("schema")
        or evidence.get("schema") != validation_artifact.get("schema")
        or validation.get("conclusion") != validation_artifact.get("conclusion")
    ):
        raise ProvenanceError("final weights 内嵌 ML schema/conclusion 与来源产物不一致")
    for owner in (evidence, validation):
        if not _binding_identity_matches(owner.get("evidence_artifact", owner.get("artifact")), artifacts["ml_validation"]):
            raise ProvenanceError("final weights ML evidence artifact 未绑定来源产物")
        inputs = owner.get("input_bindings")
        if not isinstance(inputs, Mapping):
            raise ProvenanceError("final weights ML evidence 缺少 input_bindings")
        if inputs != external_bindings:
            raise ProvenanceError("final weights ML input_bindings 与来源产物不一致")
        if not _binding_identity_matches(inputs.get("samples"), artifacts["samples"]):
            raise ProvenanceError("final weights ML evidence 未绑定 samples")
        if not _binding_identity_matches(
            inputs.get("statistical_weights_pre_finalization"),
            artifacts["weights_pre_finalization"],
        ):
            raise ProvenanceError("final weights ML evidence 未绑定 pre-final weights")


def attach_model_provenance(
    document: MutableMapping[str, Any], *, manifest_path: str | Path, root: str | Path
) -> dict[str, Any]:
    root_path = Path(root).resolve(strict=True)
    path = Path(manifest_path)
    manifest = _json_object(path, "provenance manifest")
    verify_manifest(manifest, root=root_path, rehash=True)
    _verify_model_relationships(document, manifest, root_path)
    binding = {
        "schema": MODEL_BINDING_SCHEMA,
        "manifest_artifact": artifact_identity(path, root_path),
        "manifest_chain_sha256": manifest["chain_sha256"],
        "manifest": manifest,
    }
    document[MODEL_FIELD] = binding
    return binding


def finalize_model_provenance(
    weights_path: str | Path, *, manifest_path: str | Path, root: str | Path
) -> dict[str, Any]:
    """在 ML gate 通过后附加来源链，再生成覆盖来源链的最终 seal。"""

    weights = Path(weights_path)
    document = _json_object(weights, "final weights")
    document.pop("publication_seal", None)
    attach_model_provenance(document, manifest_path=manifest_path, root=root)
    try:
        from riscv_weight_model_seal import seal_model_document

        seal_model_document(document)
    except ValueError as error:
        raise ProvenanceError(str(error)) from error
    temporary = weights.with_name(f".{weights.name}.provenance.tmp")
    temporary.write_text(
        json.dumps(document, ensure_ascii=False, indent=2, sort_keys=True, allow_nan=False)
        + "\n",
        encoding="utf-8",
    )
    temporary.replace(weights)
    return document


def verify_finalized_model(
    weights_path: str | Path,
    *,
    root: str | Path,
    manifest_path: str | Path | None = None,
) -> dict[str, Any]:
    root_path = Path(root).resolve(strict=True)
    document = _json_object(Path(weights_path), "final weights")
    try:
        verify_model_document_seal(document)
    except ValueError as error:
        raise ProvenanceError(str(error)) from error
    binding = document.get(MODEL_FIELD)
    if not isinstance(binding, Mapping) or set(binding) != {
        "schema", "manifest_artifact", "manifest_chain_sha256", "manifest"
    }:
        raise ProvenanceError("final weights 缺少完整 artifact provenance")
    if binding.get("schema") != MODEL_BINDING_SCHEMA:
        raise ProvenanceError("final weights artifact provenance schema 不受支持")
    embedded = binding.get("manifest")
    if not isinstance(embedded, Mapping):
        raise ProvenanceError("final weights 未内嵌 provenance manifest")
    verify_manifest(embedded, root=root_path, rehash=True)
    if binding.get("manifest_chain_sha256") != embedded.get("chain_sha256"):
        raise ProvenanceError("final weights manifest chain binding 不一致")
    identity = _validate_identity(binding.get("manifest_artifact"), "manifest_artifact")
    external_path = (
        root_path / str(identity["path"])
        if manifest_path is None
        else Path(manifest_path)
    )
    actual_identity = artifact_identity(external_path, root_path)
    if actual_identity != identity:
        raise ProvenanceError("final weights 绑定的外部 manifest 已漂移")
    external = _json_object(external_path, "external provenance manifest")
    if _canonical_bytes(external) != _canonical_bytes(embedded):
        raise ProvenanceError("final weights 内嵌与外部 provenance manifest 不一致")
    _verify_model_relationships(document, embedded, root_path)
    return document


def discover_provenance_root(weights_path: str | Path) -> Path:
    """从已封印模型的 manifest 相对路径唯一推导 acquisition root。"""

    weights = Path(weights_path).resolve(strict=True)
    document = _json_object(weights, "final weights")
    binding = document.get(MODEL_FIELD)
    identity = (
        binding.get("manifest_artifact") if isinstance(binding, Mapping) else None
    )
    validated = _validate_identity(identity, "manifest_artifact")
    relative = Path(str(validated["path"]))
    matches: list[Path] = []
    for ancestor in weights.parents:
        candidate = ancestor / relative
        if not candidate.is_file() or candidate.is_symlink():
            continue
        try:
            if artifact_identity(candidate, ancestor) == validated:
                matches.append(ancestor)
        except (OSError, ProvenanceError):
            continue
    if len(matches) != 1:
        raise ProvenanceError(
            "不能从 final weights 唯一推导 provenance root；请显式指定"
        )
    return matches[0]


def _artifact_arguments(values: Sequence[str]) -> dict[str, Path]:
    result: dict[str, Path] = {}
    for value in values:
        role, separator, path = value.partition("=")
        if not separator or not role or not path or role in result:
            raise ProvenanceError(f"--artifact 必须是唯一 role=path：{value!r}")
        result[role] = Path(path)
    return result


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)
    create = subparsers.add_parser("create")
    create.add_argument("--root", required=True)
    create.add_argument("--output", required=True)
    create.add_argument("--artifact", action="append", default=[])
    verify = subparsers.add_parser("verify")
    verify.add_argument("--root", required=True)
    verify.add_argument("--weights", required=True)
    verify.add_argument("--manifest")
    finalize = subparsers.add_parser("finalize")
    finalize.add_argument("--root", required=True)
    finalize.add_argument("--weights", required=True)
    finalize.add_argument("--manifest", required=True)
    arguments = parser.parse_args(argv)
    if arguments.command == "create":
        write_manifest(
            arguments.output,
            root=arguments.root,
            artifacts=_artifact_arguments(arguments.artifact),
        )
    elif arguments.command == "verify":
        verify_finalized_model(
            arguments.weights, root=arguments.root, manifest_path=arguments.manifest
        )
    else:
        finalize_model_provenance(
            arguments.weights, root=arguments.root, manifest_path=arguments.manifest
        )
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, ProvenanceError) as error:
        print(f"riscv weight provenance: {error}", file=__import__("sys").stderr)
        raise SystemExit(1)
