#!/usr/bin/env python3
"""Run the compressor against a caller-owned PDF corpus.

This runner is deliberately independent of the Rust test crate.  It invokes the
CLI and every viewer/oracle in bounded subprocesses, records one JSON object per
manifest entry, and treats missing outputs, invalid inputs, unavailable tools,
and failed quality gates as explicit outcomes.  It does not download corpus
files; the manifest names files already present below ``--input-dir``.

The default evidence policy is conservative:

* qpdf and Ghostscript must accept both the source and produced PDF;
* every page is rendered at 150 DPI by Poppler and MuPDF;
* lossless requires exact pixels for each renderer;
* prepress requires MAE similarity and local 8x8 luminance SSIM of at least
  0.99 for every page, for each renderer;
* page boxes, form fields, annotations/links, and normalized extracted text
  must be unchanged.

The script uses only the Python standard library by default.  NumPy is used
when available for faster metrics, while the small fallback implementation is
kept for environments that only provide the standard library.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import contextvars
import csv
import hashlib
import importlib.util
import json
import math
import os
import platform
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Iterable, Mapping, Sequence


SCHEMA_VERSION = 1
CAPTURE_BYTES = 64 * 1024
TEXT_PREVIEW_BYTES = 4096
SEMANTIC_OUTPUT_BYTES = 4 * 1024 * 1024
DEFAULT_TIMEOUT = 120.0
DEFAULT_DPI = 150
DEFAULT_MAX_RASTER_MEGAPIXELS = 50.0
PERCENT = 100.0
_CASE_DEADLINE: contextvars.ContextVar[float | None] = contextvars.ContextVar(
    "verify_corpus_case_deadline", default=None
)

try:  # Optional acceleration; all correctness decisions have a fallback.
    import numpy as _np  # type: ignore
except Exception:  # pragma: no cover - exercised on minimal Python installs.
    _np = None


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="seconds")


def finite_float(value: Any, default: float = 0.0) -> float:
    try:
        parsed = float(value)
    except (TypeError, ValueError):
        return default
    return parsed if math.isfinite(parsed) else default


def json_safe(value: Any) -> Any:
    """Convert incidental non-JSON values without hiding the original shape."""

    if value is None or isinstance(value, (str, bool, int)):
        return value
    if isinstance(value, float):
        return value if math.isfinite(value) else None
    if isinstance(value, Mapping):
        return {str(key): json_safe(item) for key, item in value.items()}
    if isinstance(value, (list, tuple)):
        return [json_safe(item) for item in value]
    return str(value)


def write_json(path: Path, value: Any) -> None:
    path.write_text(
        json.dumps(json_safe(value), ensure_ascii=True, sort_keys=True, indent=2)
        + "\n",
        encoding="utf-8",
    )


def sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def sha256_file(path: Path) -> tuple[str, int]:
    digest = hashlib.sha256()
    size = 0
    with path.open("rb") as stream:
        while True:
            chunk = stream.read(1024 * 1024)
            if not chunk:
                break
            digest.update(chunk)
            size += len(chunk)
    return digest.hexdigest(), size


def command_for_json(args: Sequence[str | os.PathLike[str]]) -> list[str]:
    shown: list[str] = []
    for value in args:
        text = os.fspath(value)
        if len(text) > 240:
            shown.append("<inline-code>")
        else:
            shown.append(text)
    return shown


def _kill_process_group(process: subprocess.Popen[bytes]) -> None:
    try:
        if os.name == "posix":
            os.killpg(process.pid, signal.SIGKILL)
        else:  # pragma: no cover - this suite runs on macOS/Linux.
            process.kill()
    except (ProcessLookupError, OSError):
        try:
            process.kill()
        except (ProcessLookupError, OSError):
            pass


def _decode_capture(data: bytes, limit: int = CAPTURE_BYTES) -> tuple[str, bool]:
    truncated = len(data) > limit
    if truncated:
        data = data[:limit]
    return data.decode("utf-8", errors="replace"), truncated


def run_command(
    command: Sequence[str | os.PathLike[str]],
    timeout: float,
    *,
    stdout_path: Path | None = None,
    capture_limit: int = CAPTURE_BYTES,
) -> dict[str, Any]:
    """Run one untrusted-tool command without invoking a shell.

    ``start_new_session`` lets timeout handling terminate descendants spawned by
    Ghostscript, Poppler, or MuPDF as a group.  stdout can optionally be sent to
    a file (used for text extraction), while stderr remains bounded in memory.
    """

    command_json = command_for_json(command)
    started = time.monotonic()
    deadline = _CASE_DEADLINE.get()
    effective_timeout = timeout
    if deadline is not None:
        remaining = deadline - started
        if remaining <= 0:
            return {
                "command": command_json,
                "status": "timeout",
                "returncode": None,
                "timed_out": True,
                "elapsed_seconds": 0.0,
                "stdout": "",
                "stderr": "",
                "stdout_truncated": False,
                "stderr_truncated": False,
                "error": "per-PDF deadline elapsed before command launch",
            }
        effective_timeout = min(effective_timeout, remaining)
    process: subprocess.Popen[bytes] | None = None
    output_handle: Any = None
    try:
        if stdout_path is not None:
            stdout_path.parent.mkdir(parents=True, exist_ok=True)
            output_handle = stdout_path.open("wb")
            stdout_target: Any = output_handle
        else:
            stdout_target = subprocess.PIPE
        process = subprocess.Popen(
            [os.fspath(value) for value in command],
            stdin=subprocess.DEVNULL,
            stdout=stdout_target,
            stderr=subprocess.PIPE,
            start_new_session=(os.name == "posix"),
        )
        try:
            stdout_data, stderr_data = process.communicate(timeout=effective_timeout)
            timed_out = False
        except subprocess.TimeoutExpired as error:
            timed_out = True
            _kill_process_group(process)
            cleanup_timeout = 1.0
            if deadline is not None:
                cleanup_timeout = max(0.1, min(cleanup_timeout, deadline - time.monotonic()))
            try:
                stdout_data, stderr_data = process.communicate(timeout=cleanup_timeout)
            except subprocess.TimeoutExpired as cleanup_error:
                _kill_process_group(process)
                stdout_data, stderr_data = cleanup_error.output or b"", cleanup_error.stderr or b""
                try:
                    process.wait(timeout=0.5)
                except (OSError, subprocess.TimeoutExpired):
                    pass
            if stdout_data is None:
                stdout_data = error.output or b""
            if stderr_data is None:
                stderr_data = error.stderr or b""
        return_code = process.returncode
    except FileNotFoundError as error:
        return {
            "command": command_json,
            "status": "tool_unavailable",
            "returncode": None,
            "timed_out": False,
            "elapsed_seconds": round(time.monotonic() - started, 6),
            "stdout": "",
            "stderr": "",
            "stdout_truncated": False,
            "stderr_truncated": False,
            "error": str(error),
        }
    except (OSError, subprocess.SubprocessError) as error:
        if process is not None and process.poll() is None:
            _kill_process_group(process)
            try:
                process.wait(timeout=10)
            except (OSError, subprocess.TimeoutExpired):
                pass
        return {
            "command": command_json,
            "status": "spawn_error",
            "returncode": None if process is None else process.returncode,
            "timed_out": False,
            "elapsed_seconds": round(time.monotonic() - started, 6),
            "stdout": "",
            "stderr": "",
            "stdout_truncated": False,
            "stderr_truncated": False,
            "error": str(error),
        }
    finally:
        if output_handle is not None:
            output_handle.close()

    stdout_data = stdout_data or b""
    stderr_data = stderr_data or b""
    stdout_text, stdout_truncated = _decode_capture(stdout_data, capture_limit)
    stderr_text, stderr_truncated = _decode_capture(stderr_data, capture_limit)
    status = "timeout" if timed_out else ("pass" if return_code == 0 else "failed")
    return {
        "command": command_json,
        "status": status,
        "returncode": return_code,
        "timed_out": timed_out,
        "elapsed_seconds": round(time.monotonic() - started, 6),
        "stdout": stdout_text,
        "stderr": stderr_text,
        "stdout_truncated": stdout_truncated,
        "stderr_truncated": stderr_truncated,
    }


def resolve_executable(value: str | None, default_name: str) -> str | None:
    candidate = value or default_name
    if os.path.sep in candidate or (os.name == "nt" and "/" in candidate):
        path = Path(candidate).expanduser()
        return str(path.resolve()) if path.exists() else None
    found = shutil.which(candidate)
    return str(Path(found).resolve()) if found else None


def probe_version(path: str | None, args: Sequence[str], timeout: float) -> dict[str, Any]:
    if not path:
        return {"path": None, "status": "tool_unavailable", "version": None}
    step = run_command([path, *args], timeout)
    version = (step.get("stdout", "") + step.get("stderr", "")).strip()
    return {
        "path": path,
        "status": step["status"],
        "version": version[:CAPTURE_BYTES] if version else None,
        "command": step["command"],
        "returncode": step["returncode"],
    }


def module_version(module: str) -> dict[str, Any]:
    available = importlib.util.find_spec(module) is not None
    version = None
    if available:
        try:
            imported = __import__(module)
            version = getattr(imported, "__version__", None)
        except Exception as error:  # pragma: no cover - dependency-specific.
            return {"available": True, "version": None, "error": str(error)}
    return {"available": available, "version": version}


def load_manifest(path: Path) -> list[dict[str, Any]]:
    """Accept JSON, JSONL, CSV/TSV, or a plain list of relative paths."""

    raw = path.read_bytes()
    suffix = path.suffix.lower()
    text = raw.decode("utf-8-sig")
    if suffix in {".jsonl", ".ndjson"}:
        records: list[dict[str, Any]] = []
        for line_number, line in enumerate(text.splitlines(), 1):
            if not line.strip() or line.lstrip().startswith("#"):
                continue
            try:
                value = json.loads(line)
            except json.JSONDecodeError as error:
                records.append(
                    {
                        "_manifest_error": f"line {line_number}: invalid JSON: {error}",
                        "_manifest_line": line_number,
                    }
                )
                continue
            records.append(_coerce_manifest_record(value))
        return records
    if suffix == ".json":
        value = json.loads(text)
        return _records_from_json(value)
    if suffix in {".csv", ".tsv"}:
        delimiter = "\t" if suffix == ".tsv" else ","
        return [dict(row) for row in csv.DictReader(text.splitlines(), delimiter=delimiter)]

    lines = [line.strip() for line in text.splitlines() if line.strip() and not line.lstrip().startswith("#")]
    return [{"path": line} for line in lines]


def _coerce_manifest_record(value: Any) -> dict[str, Any]:
    if isinstance(value, Mapping):
        return dict(value)
    if isinstance(value, (str, int, float)):
        return {"path": str(value)}
    return {"_manifest_error": f"manifest record is not an object: {value!r}"}


def _records_from_json(value: Any) -> list[dict[str, Any]]:
    if isinstance(value, list):
        return [_coerce_manifest_record(item) for item in value]
    if isinstance(value, Mapping):
        for key in ("files", "documents", "entries", "corpus", "records"):
            nested = value.get(key)
            if isinstance(nested, list):
                return [_coerce_manifest_record(item) for item in nested]
        return [dict(value)]
    return [{"_manifest_error": "top-level JSON value is not a record or list"}]


def manifest_path(record: Mapping[str, Any]) -> str | None:
    # Corpus manifests may retain a provenance-only source_path while
    # local_path names the downloaded file actually under --input-dir.
    for key in ("path", "file", "filename", "input", "local_path", "source_path"):
        value = record.get(key)
        if value is not None and str(value).strip():
            return str(value)
    return None


def manifest_id(record: Mapping[str, Any], index: int) -> str:
    for key in ("id", "name", "key", "slug"):
        value = record.get(key)
        if value is not None and str(value).strip():
            return str(value)
    path = manifest_path(record)
    return Path(path).stem if path else f"record-{index + 1:06d}"


def expected_sha256(record: Mapping[str, Any]) -> str | None:
    for key in ("sha256", "sha-256", "expected_sha256", "checksum"):
        value = record.get(key)
        if value is not None and str(value).strip():
            return str(value).strip().lower().removeprefix("sha256:")
    return None


def expected_validity(record: Mapping[str, Any]) -> bool | None:
    value = record.get("expected_valid")
    if isinstance(value, bool):
        return value
    if value is None:
        return None
    text = str(value).strip().lower()
    if text in {"true", "yes", "1", "valid"}:
        return True
    if text in {"false", "no", "0", "invalid"}:
        return False
    return None


def explicit_compressor_rejection(step: Mapping[str, Any]) -> bool:
    """Accept a negative result only for a normal, diagnosed rejection."""

    if step.get("status") != "failed" or step.get("returncode") in (None, 0) or step.get("timed_out"):
        return False
    message = f"{step.get('stdout', '')} {step.get('stderr', '')}".lower()
    markers = (
        "invalid",
        "unsupported",
        "encrypted",
        "password",
        "limit",
        "malformed",
        "missing object",
        "cycle",
        "pdf(",
    )
    return any(marker in message for marker in markers)


def safe_component(value: str, fallback: str) -> str:
    value = re.sub(r"[^A-Za-z0-9_.-]+", "_", value).strip("._")
    return value[:100] or fallback


def resolve_input(input_dir: Path, value: str) -> Path:
    candidate = Path(value).expanduser()
    if not candidate.is_absolute():
        candidate = input_dir / candidate
    return candidate.resolve()


def qpdf_check(path: Path, tools: Mapping[str, Any], timeout: float) -> dict[str, Any]:
    executable = tools.get("qpdf")
    if not executable:
        return {"status": "tool_unavailable", "error": "qpdf was not found"}
    return run_command([executable, "--check", os.fspath(path)], timeout)


def ghostscript_check(path: Path, tools: Mapping[str, Any], timeout: float) -> dict[str, Any]:
    executable = tools.get("gs")
    if not executable:
        return {"status": "tool_unavailable", "error": "Ghostscript was not found"}
    return run_command(
        [
            executable,
            "-q",
            "-dSAFER",
            "-dBATCH",
            "-dNOPAUSE",
            "-dPDFSTOPONERROR",
            "-dPDFSTOPONWARNING",
            "-sDEVICE=nullpage",
            "-f",
            os.fspath(path),
        ],
        timeout,
    )


def structural_checks(path: Path, tools: Mapping[str, Any], timeout: float) -> dict[str, Any]:
    qpdf = qpdf_check(path, tools, timeout)
    ghostscript = ghostscript_check(path, tools, timeout)
    statuses = (qpdf.get("status"), ghostscript.get("status"))
    return {
        "qpdf": qpdf,
        "ghostscript": ghostscript,
        "engines": {
            "qpdf": {
                "executable": tools.get("qpdf"),
                "version": tools.get("_versions", {}).get("qpdf", {}).get("version"),
            },
            "ghostscript": {
                "executable": tools.get("gs"),
                "version": tools.get("_versions", {}).get("ghostscript", {}).get("version"),
            },
        },
        "valid": all(status == "pass" for status in statuses),
        "invalid": any(status == "failed" for status in statuses),
        "unavailable": any(status in {"tool_unavailable", "spawn_error"} for status in statuses),
    }


def _discover_ppm(directory: Path, prefix: str = "page") -> list[tuple[int, Path]]:
    found: list[tuple[int, Path]] = []
    pattern = re.compile(rf"^{re.escape(prefix)}-(\d+)\.ppm$", re.IGNORECASE)
    for path in directory.glob(f"{prefix}-*.ppm"):
        match = pattern.match(path.name)
        if match:
            found.append((int(match.group(1)), path))
    return sorted(found)


def _ppm_dimensions(path: Path) -> tuple[int, int, int]:
    raw = path.read_bytes()
    magic, width, height, max_value, offset = _ppm_header(raw)
    expected_channels = 1 if magic == b"P5" else 3
    expected = width * height * expected_channels
    if len(raw) - offset < expected:
        raise ValueError(f"PPM pixel payload is truncated ({len(raw) - offset} < {expected})")
    return width, height, len(raw)


def _ppm_header(raw: bytes) -> tuple[bytes, int, int, int, int]:
    position = 0

    def token() -> bytes:
        nonlocal position
        while position < len(raw) and raw[position] in b" \t\r\n":
            position += 1
        if position < len(raw) and raw[position] == ord("#"):
            while position < len(raw) and raw[position] not in b"\r\n":
                position += 1
            return token()
        start = position
        while position < len(raw) and raw[position] not in b" \t\r\n#":
            position += 1
        if start == position:
            raise ValueError("missing PPM header token")
        return raw[start:position]

    magic = token()
    if magic not in {b"P5", b"P6"}:
        raise ValueError(f"unsupported raster format {magic!r}")
    width = int(token())
    height = int(token())
    max_value = int(token())
    if width <= 0 or height <= 0 or max_value <= 0 or max_value > 255:
        raise ValueError("invalid PPM dimensions or max value")
    # The header's single separating whitespace belongs to the header.  Do not
    # skip arbitrary bytes here: the first raster byte may itself be whitespace.
    if position >= len(raw) or raw[position] not in b" \t\r\n":
        raise ValueError("PPM header has no raster separator")
    separator = raw[position]
    position += 1
    if separator == ord("\r") and position < len(raw) and raw[position] == ord("\n"):
        position += 1
    return magic, width, height, max_value, position


def read_ppm(path: Path) -> tuple[int, int, bytes]:
    raw = path.read_bytes()
    magic, width, height, max_value, offset = _ppm_header(raw)
    channels = 1 if magic == b"P5" else 3
    count = width * height * channels
    data = raw[offset : offset + count]
    if len(data) != count:
        raise ValueError("PPM pixel payload is truncated")
    if channels == 1:
        data = bytes(channel for value in data for channel in (value, value, value))
    if max_value != 255:
        data = bytes(round(value * 255 / max_value) for value in data)
    return width, height, data


def _ssim_blocks(left: Any, right: Any) -> float:
    constant_1 = (0.01 * 255.0) ** 2
    constant_2 = (0.03 * 255.0) ** 2
    height, width = left.shape
    scores: list[float] = []
    block_number = 0
    for top in range(0, height, 8):
        for column in range(0, width, 8):
            if block_number % 256 == 0:
                deadline = _CASE_DEADLINE.get()
                if deadline is not None and time.monotonic() >= deadline:
                    raise TimeoutError("per-PDF deadline elapsed during SSIM")
            block_number += 1
            first = left[top : top + 8, column : column + 8]
            second = right[top : top + 8, column : column + 8]
            first_mean = float(first.mean())
            second_mean = float(second.mean())
            first_centered = first - first_mean
            second_centered = second - second_mean
            count = first.size
            # Match tests::support::compare: population moments (divide
            # by N), including edge windows smaller than 8x8.
            divisor = max(1, count)
            first_variance = max(0.0, float((first_centered * first_centered).sum()) / divisor)
            second_variance = max(0.0, float((second_centered * second_centered).sum()) / divisor)
            covariance = float((first_centered * second_centered).sum()) / divisor
            numerator = (2 * first_mean * second_mean + constant_1) * (
                2 * covariance + constant_2
            )
            denominator = (first_mean * first_mean + second_mean * second_mean + constant_1) * (
                first_variance + second_variance + constant_2
            )
            scores.append(numerator / denominator if denominator else 1.0)
    return sum(scores) / len(scores) if scores else 1.0


def _metrics_numpy(left: bytes, right: bytes, width: int, height: int) -> dict[str, Any]:
    assert _np is not None
    first = _np.frombuffer(left, dtype=_np.uint8).reshape((height, width, 3))
    second = _np.frombuffer(right, dtype=_np.uint8).reshape((height, width, 3))
    difference = _np.abs(first.astype(_np.int16) - second.astype(_np.int16))
    max_error = int(difference.max()) if difference.size else 0
    mae_similarity = 1.0 - float(difference.mean()) / 255.0 if difference.size else 1.0
    luminance_first = (
        first[..., 0].astype(_np.float64) * 0.2126
        + first[..., 1].astype(_np.float64) * 0.7152
        + first[..., 2].astype(_np.float64) * 0.0722
    )
    luminance_second = (
        second[..., 0].astype(_np.float64) * 0.2126
        + second[..., 1].astype(_np.float64) * 0.7152
        + second[..., 2].astype(_np.float64) * 0.0722
    )
    return {
        "exact": bool(max_error == 0),
        "max_channel_error": max_error,
        "mae_similarity": mae_similarity,
        "ssim": _ssim_blocks(luminance_first, luminance_second),
    }


def _metrics_fallback(left: bytes, right: bytes, width: int, height: int) -> dict[str, Any]:
    count = max(1, len(left))
    total = 0
    max_error = 0
    first_luma: list[float] = []
    second_luma: list[float] = []
    for index, (first, second) in enumerate(zip(left, right)):
        difference = abs(first - second)
        total += difference
        max_error = max(max_error, difference)
        if index % 3 == 0:
            first_luma.append(0.0)
            second_luma.append(0.0)
        channel = index % 3
        first_luma[-1] += first * (0.2126, 0.7152, 0.0722)[channel]
        second_luma[-1] += second * (0.2126, 0.7152, 0.0722)[channel]

    constant_1 = (0.01 * 255.0) ** 2
    constant_2 = (0.03 * 255.0) ** 2
    scores: list[float] = []
    block_number = 0
    for top in range(0, height, 8):
        for column in range(0, width, 8):
            if block_number % 256 == 0:
                deadline = _CASE_DEADLINE.get()
                if deadline is not None and time.monotonic() >= deadline:
                    raise TimeoutError("per-PDF deadline elapsed during SSIM")
            block_number += 1
            values_a: list[float] = []
            values_b: list[float] = []
            for row in range(top, min(height, top + 8)):
                start = row * width + column
                end = min(row * width + min(width, column + 8), len(first_luma))
                values_a.extend(first_luma[start:end])
                values_b.extend(second_luma[start:end])
            if not values_a:
                continue
            mean_a = sum(values_a) / len(values_a)
            mean_b = sum(values_b) / len(values_b)
            # Population moments, matching the Rust oracle.
            divisor = max(1, len(values_a))
            variance_a = max(0.0, sum((value - mean_a) ** 2 for value in values_a) / divisor)
            variance_b = max(0.0, sum((value - mean_b) ** 2 for value in values_b) / divisor)
            covariance = sum(
                (a - mean_a) * (b - mean_b) for a, b in zip(values_a, values_b)
            ) / divisor
            numerator = (2 * mean_a * mean_b + constant_1) * (2 * covariance + constant_2)
            denominator = (mean_a * mean_a + mean_b * mean_b + constant_1) * (
                variance_a + variance_b + constant_2
            )
            scores.append(numerator / denominator if denominator else 1.0)
    return {
        "exact": max_error == 0,
        "max_channel_error": max_error,
        "mae_similarity": 1.0 - (total / count) / 255.0,
        "ssim": sum(scores) / len(scores) if scores else 1.0,
    }


def pixel_metrics(left: bytes, right: bytes, width: int, height: int) -> dict[str, Any]:
    if len(left) != len(right):
        return {
            "exact": False,
            "max_channel_error": None,
            "mae_similarity": 0.0,
            "ssim": 0.0,
            "error": "raster byte lengths differ",
        }
    if left == right:
        return {
            "exact": True,
            "max_channel_error": 0,
            "mae_similarity": 1.0,
            "ssim": 1.0,
        }
    if _np is not None:
        return _metrics_numpy(left, right, width, height)
    return _metrics_fallback(left, right, width, height)


def write_diff(path: Path, left: bytes, right: bytes, width: int, height: int) -> None:
    diff = bytearray(len(left))
    for index, (first, second) in enumerate(zip(left, right)):
        diff[index] = min(255, abs(first - second) * 4)
    path.write_bytes(f"P6\n{width} {height}\n255\n".encode("ascii") + bytes(diff))


def render_pdf(
    path: Path,
    renderer: str,
    role: str,
    tools: Mapping[str, Any],
    case_dir: Path,
    timeout: float,
    dpi: int,
    max_raster_megapixels: float,
    expected_page_count: int | None = None,
) -> dict[str, Any]:
    render_dir = case_dir / "renders" / renderer / role
    render_dir.mkdir(parents=True, exist_ok=True)
    prefix = render_dir / "page"
    if renderer == "poppler":
        executable = tools.get("pdftoppm")
        if not executable:
            return {
                "status": "tool_unavailable",
                "error": "pdftoppm was not found",
                "pages": [],
                "directory": os.fspath(render_dir),
            }
        command = [
            executable,
            "-r",
            str(dpi),
            "-aa",
            "yes",
            "-aaVector",
            "yes",
            os.fspath(path),
            os.fspath(prefix),
        ]
    elif renderer == "mupdf":
        executable = tools.get("mutool")
        if executable:
            command = [
                executable,
                "draw",
                "-q",
                "-r",
                str(dpi),
                "-F",
                "ppm",
                "-o",
                os.fspath(render_dir / "page-%d.ppm"),
                os.fspath(path),
            ]
        elif tools.get("pymupdf"):
            python = tools.get("python") or sys.executable
            command = [
                python,
                "-c",
                PYMUPDF_RENDERER,
                os.fspath(path),
                os.fspath(render_dir),
                str(dpi),
            ]
        else:
            return {
                "status": "tool_unavailable",
                "error": "neither mutool nor PyMuPDF was found",
                "pages": [],
                "directory": os.fspath(render_dir),
            }
    else:
        raise ValueError(f"unknown renderer {renderer}")

    step = run_command(command, timeout)
    discovered = _discover_ppm(render_dir)
    pages: list[dict[str, Any]] = []
    resource_limited = False
    parse_error: str | None = None
    total_pixels = 0
    for number, raster in discovered:
        try:
            width, height, byte_size = _ppm_dimensions(raster)
        except (OSError, ValueError) as error:
            parse_error = f"page {number}: {error}"
            break
        megapixels = width * height / 1_000_000.0
        total_pixels += width * height
        if megapixels > max_raster_megapixels:
            resource_limited = True
        pages.append(
            {
                "number": number,
                "width": width,
                "height": height,
                "bytes": byte_size,
                "megapixels": megapixels,
                "path": os.fspath(raster),
            }
        )
    if parse_error:
        status = "failed"
    elif resource_limited:
        status = "resource_limit"
    elif step["status"] != "pass":
        status = step["status"]
    elif not pages:
        status = "failed"
        parse_error = "renderer produced no PPM pages"
    elif expected_page_count is not None and len(pages) != expected_page_count:
        status = "failed"
        parse_error = f"renderer page count {len(pages)} != semantic page count {expected_page_count}"
    elif expected_page_count is not None and [page["number"] for page in pages] != list(
        range(1, expected_page_count + 1)
    ):
        status = "failed"
        parse_error = "renderer page numbers do not match semantic page order"
    else:
        status = "pass"
    return {
        "status": status,
        "step": step,
        "pages": pages,
        "page_count": len(pages),
        "page_numbers": [page["number"] for page in pages],
        "expected_page_count": expected_page_count,
        "semantic_page_count_match": expected_page_count is None or len(pages) == expected_page_count,
        "total_megapixels": total_pixels / 1_000_000.0,
        "directory": os.fspath(render_dir),
        "error": parse_error
        or ("rendered page exceeds --max-raster-megapixels" if resource_limited else None),
    }


def raster_preflight(
    semantic: Mapping[str, Any], dpi: int, max_raster_megapixels: float
) -> dict[str, Any]:
    """Reject obviously huge pages before asking a viewer to allocate them."""

    if semantic.get("status") != "pass":
        return {"status": "unknown", "pages": [], "error": "page boxes unavailable"}
    offenders: list[dict[str, Any]] = []
    estimates: list[dict[str, Any]] = []
    for index, page in enumerate(semantic.get("snapshot", {}).get("pages", []), 1):
        box = page.get("crop_box") or page.get("media_box") or []
        if not isinstance(box, list) or len(box) != 4:
            continue
        try:
            unit = float(page.get("user_unit", 1.0) or 1.0)
            width_points = abs(float(box[2]) - float(box[0])) * unit
            height_points = abs(float(box[3]) - float(box[1])) * unit
            megapixels = width_points / 72.0 * dpi * height_points / 72.0 * dpi / 1_000_000.0
        except (TypeError, ValueError):
            continue
        estimate = {"page": index, "estimated_megapixels": megapixels}
        estimates.append(estimate)
        if megapixels > max_raster_megapixels:
            offenders.append(estimate)
    return {
        "status": "resource_limit" if offenders else "pass",
        "pages": offenders,
        "estimates": estimates,
        "error": "page estimate exceeds --max-raster-megapixels" if offenders else None,
    }


PYMUPDF_RENDERER = r'''
import pathlib, sys
import fitz

source = pathlib.Path(sys.argv[1])
destination = pathlib.Path(sys.argv[2])
dpi = float(sys.argv[3])
document = fitz.open(source)
matrix = fitz.Matrix(dpi / 72.0, dpi / 72.0)
for number, page in enumerate(document, 1):
    pixmap = page.get_pixmap(matrix=matrix, alpha=False)
    (destination / f"page-{number}.ppm").write_bytes(pixmap.tobytes("ppm"))
'''


def compare_rendered_pages(
    renderer: str,
    source_render: Mapping[str, Any],
    output_render: Mapping[str, Any],
    case_dir: Path,
    preset: str,
    threshold: float = 0.99,
) -> dict[str, Any]:
    result: dict[str, Any] = {
        "status": "failed",
        "passed": False,
        "renderer": renderer,
        "pages": [],
        "failure_artifacts": [],
    }
    if source_render.get("status") != "pass" or output_render.get("status") != "pass":
        result["status"] = "not_comparable"
        result["error"] = "source or output rendering did not complete"
        return result
    source_pages = list(source_render.get("pages", []))
    output_pages = list(output_render.get("pages", []))
    if len(source_pages) != len(output_pages):
        result["status"] = "failed"
        result["error"] = f"page count differs ({len(source_pages)} != {len(output_pages)})"
        return result

    first_failure: tuple[int, Path | None, Path | None, dict[str, Any]] | None = None
    for source_page, output_page in zip(source_pages, output_pages):
        source_path = Path(source_page["path"])
        output_path = Path(output_page["path"])
        page_result: dict[str, Any] = {
            "number": source_page["number"],
            "source_width": source_page["width"],
            "source_height": source_page["height"],
            "output_width": output_page["width"],
            "output_height": output_page["height"],
        }
        if (source_page["width"], source_page["height"]) != (
            output_page["width"],
            output_page["height"],
        ):
            page_result.update(
                {
                    "passed": False,
                    "error": "raster dimensions differ",
                    "mae_similarity": 0.0,
                    "ssim": 0.0,
                    "max_channel_error": None,
                }
            )
        else:
            try:
                source_width, source_height, source_pixels = read_ppm(source_path)
                output_width, output_height, output_pixels = read_ppm(output_path)
                metrics = pixel_metrics(source_pixels, output_pixels, source_width, source_height)
                page_result.update(metrics)
                if preset == "lossless":
                    page_result["passed"] = bool(metrics.get("exact"))
                else:
                    page_result["passed"] = bool(
                        metrics.get("mae_similarity", 0.0) >= threshold
                        and metrics.get("ssim", 0.0) >= threshold
                    )
            except (OSError, ValueError, TimeoutError) as error:
                page_result.update(
                    {
                        "passed": False,
                        "error": str(error),
                        "mae_similarity": 0.0,
                        "ssim": 0.0,
                        "max_channel_error": None,
                    }
                )
        result["pages"].append(page_result)
        if not page_result.get("passed") and first_failure is None:
            first_failure = (
                int(source_page["number"]),
                source_path if source_path.exists() else None,
                output_path if output_path.exists() else None,
                page_result,
            )

    if first_failure is None:
        result["status"] = "pass"
        result["passed"] = True
        return result

    number, source_path, output_path, page_result = first_failure
    failure_dir = case_dir / "failure" / renderer / f"page-{number}"
    failure_dir.mkdir(parents=True, exist_ok=True)
    if source_path is not None:
        source_copy = failure_dir / "source.ppm"
        shutil.copy2(source_path, source_copy)
        result["failure_artifacts"].append(os.fspath(source_copy))
    if output_path is not None:
        output_copy = failure_dir / "output.ppm"
        shutil.copy2(output_path, output_copy)
        result["failure_artifacts"].append(os.fspath(output_copy))
    if source_path is not None and output_path is not None:
        try:
            source_width, source_height, source_pixels = read_ppm(source_path)
            output_width, output_height, output_pixels = read_ppm(output_path)
            if (source_width, source_height) == (output_width, output_height):
                diff_path = failure_dir / "diff.ppm"
                write_diff(diff_path, source_pixels, output_pixels, source_width, source_height)
                result["failure_artifacts"].append(os.fspath(diff_path))
        except (OSError, ValueError):
            pass
    result["error"] = f"first failing page: {number}"
    result["first_failure"] = page_result
    return result


def _normalise_newlines(data: bytes) -> bytes:
    return data.replace(b"\r\n", b"\n").replace(b"\r", b"\n")


def extract_text(
    path: Path,
    role: str,
    tools: Mapping[str, Any],
    case_dir: Path,
    timeout: float,
) -> dict[str, Any]:
    executable = tools.get("pdftotext")
    if not executable:
        return {"status": "tool_unavailable", "error": "pdftotext was not found"}
    text_path = case_dir / "text" / f"{role}.txt"
    text_path.parent.mkdir(parents=True, exist_ok=True)
    step = run_command(
        [executable, "-layout", os.fspath(path), os.fspath(text_path)], timeout
    )
    result: dict[str, Any] = {"status": step["status"], "step": step}
    if step["status"] != "pass" or not text_path.exists():
        result["error"] = "pdftotext did not produce an output file"
        return result
    data = text_path.read_bytes()
    normalised = _normalise_newlines(data)
    preview = normalised[:TEXT_PREVIEW_BYTES].decode("utf-8", errors="replace")
    result.update(
        {
            "path": os.fspath(text_path),
            "bytes": len(data),
            "sha256": sha256_bytes(data),
            "normalized_bytes": len(normalised),
            "normalized_sha256": sha256_bytes(normalised),
            "preview": preview,
            "preview_truncated": len(normalised) > TEXT_PREVIEW_BYTES,
        }
    )
    return result


PYPDF_SNAPSHOT = r'''
import hashlib, json, math, pathlib, sys
from pypdf import PdfReader

source = pathlib.Path(sys.argv[1])
reader = PdfReader(str(source), strict=False)
stack = set()
page_refs = {}
for page_number, page in enumerate(reader.pages, 1):
    reference = getattr(page, "indirect_reference", None)
    if reference is not None:
        try:
            page_refs[(reference.idnum, reference.generation)] = page_number
        except Exception:
            pass

def scalar(value):
    if value is None or isinstance(value, (str, bool, int)):
        return value
    if isinstance(value, float):
        return value if math.isfinite(value) else None
    return str(value)

def canonical(value, depth=0):
    if depth > 24:
        return "<depth-limit>"
    class_name = value.__class__.__name__
    if class_name == "IndirectObject":
        try:
            page_number = page_refs.get((value.idnum, value.generation))
            if page_number is not None:
                return {"__page_reference__": page_number}
        except Exception:
            pass
        try:
            return canonical(value.get_object(), depth + 1)
        except Exception as error:
            return {"__indirect_error__": str(error)}
    if class_name in {"NullObject"}:
        return None
    if class_name in {"NameObject", "TextStringObject", "ByteStringObject", "NumberObject", "FloatObject"}:
        if class_name == "ByteStringObject":
            return {"__bytes__": bytes(value).hex()}
        return scalar(value)
    if isinstance(value, (list, tuple)):
        return [canonical(item, depth + 1) for item in value]
    if hasattr(value, "keys") and hasattr(value, "items"):
        identity = id(value)
        if identity in stack:
            return "<cycle>"
        stack.add(identity)
        result = {}
        for key, item in value.items():
            key = str(key)
            if key in {"/Parent", "/P", "/_States_"}:
                # _States_ is pypdf get_fields() metadata, not a PDF field.
                # Its heuristic can include encoding keys from an appearance
                # stream. Actual /AP and /AS remain compared on annotations.
                continue
            if hasattr(value, "get_data") and key in {"/Length", "/Filter", "/DecodeParms", "/DL"}:
                # Stream representation is intentionally independent of the
                # compressor's chosen filter and encoded length.  The decoded
                # stream hash below still detects content changes.
                continue
            result[key] = canonical(item, depth + 1)
        if hasattr(value, "get_data"):
            # Stream bytes are intentionally summarized: image recompression is
            # allowed to change encoded bytes while dictionary semantics remain.
            try:
                raw = value.get_data()
                result["__stream__"] = {"decoded_bytes": len(raw), "sha256": hashlib.sha256(raw).hexdigest()}
            except Exception as error:
                result["__stream__"] = {"error": str(error)}
        stack.remove(identity)
        return result
    return scalar(value)

def box(value):
    try:
        return [float(value.left), float(value.bottom), float(value.right), float(value.top)]
    except Exception:
        return canonical(value)

def annotations(value):
    result = canonical(value)
    if result is None:
        # PDF dictionary null values are equivalent to an absent optional key.
        return []
    if isinstance(result, list):
        # A null member is not an annotation and has no identity, appearance,
        # action, or field value to preserve. Ignore only these top-level empty
        # placeholders; every actual annotation and all its keys still compare.
        return [item for item in result if item is not None]
    return result

pages = []
for page in reader.pages:
    try:
        user_unit = float(page.get("/UserUnit", 1.0) or 1.0)
    except Exception:
        user_unit = 1.0
    pages.append({
        "media_box": box(page.mediabox),
        "crop_box": box(page.cropbox),
        "bleed_box": box(page.bleedbox),
        "trim_box": box(page.trimbox),
        "art_box": box(page.artbox),
        "rotation": int(page.rotation or 0),
        "user_unit": user_unit,
        "annotations": annotations(page.get("/Annots", [])),
    })
fields = reader.get_fields() or {}
print(json.dumps({"page_count": len(pages), "pages": pages, "fields": canonical(fields)}, sort_keys=True))
'''


def semantic_snapshot(
    path: Path,
    role: str,
    tools: Mapping[str, Any],
    case_dir: Path,
    timeout: float,
) -> dict[str, Any]:
    if not tools.get("pypdf"):
        return {"status": "tool_unavailable", "error": "pypdf was not found"}
    python = tools.get("python") or sys.executable
    step = run_command(
        [python, "-c", PYPDF_SNAPSHOT, os.fspath(path)],
        timeout,
        capture_limit=SEMANTIC_OUTPUT_BYTES,
    )
    result: dict[str, Any] = {"status": step["status"], "step": step}
    if step["status"] != "pass":
        result["error"] = "pypdf snapshot failed"
        return result
    try:
        snapshot = json.loads(step["stdout"])
    except json.JSONDecodeError as error:
        result["status"] = "failed"
        result["error"] = f"pypdf snapshot was not JSON: {error}"
        return result
    snapshot_path = case_dir / "semantic" / f"{role}.json"
    snapshot_path.parent.mkdir(parents=True, exist_ok=True)
    write_json(snapshot_path, snapshot)
    result.update({"path": os.fspath(snapshot_path), "snapshot": snapshot})
    return result


def compare_semantics(
    source: Mapping[str, Any], output: Mapping[str, Any], source_text: Mapping[str, Any], output_text: Mapping[str, Any]
) -> dict[str, Any]:
    result: dict[str, Any] = {"passed": False}
    if source.get("status") != "pass" or output.get("status") != "pass":
        result["status"] = "not_comparable"
        result["error"] = "source or output semantic snapshot failed"
        return result
    source_snapshot = source.get("snapshot", {})
    output_snapshot = output.get("snapshot", {})
    source_pages = source_snapshot.get("pages", [])
    output_pages = output_snapshot.get("pages", [])
    result.update(
        {
            "status": "pass",
            "page_count_equal": source_snapshot.get("page_count") == output_snapshot.get("page_count"),
            "page_boxes_equal": [
                {
                    "page": index + 1,
                    "equal": left == right,
                    "source": left,
                    "output": right,
                }
                for index, (left, right) in enumerate(zip(source_pages, output_pages))
            ],
            "fields_equal": source_snapshot.get("fields") == output_snapshot.get("fields"),
            "annotations_equal": [
                {
                    "page": index + 1,
                    "equal": left.get("annotations") == right.get("annotations"),
                    "source": left.get("annotations"),
                    "output": right.get("annotations"),
                }
                for index, (left, right) in enumerate(zip(source_pages, output_pages))
            ],
            "text_equal": source_text.get("normalized_sha256") == output_text.get("normalized_sha256"),
        }
    )
    result["all_page_boxes_equal"] = all(item["equal"] for item in result["page_boxes_equal"])
    result["all_annotations_equal"] = all(item["equal"] for item in result["annotations_equal"])
    result["passed"] = bool(
        result["page_count_equal"]
        and result["all_page_boxes_equal"]
        and result["fields_equal"]
        and result["all_annotations_equal"]
        and result["text_equal"]
    )
    if not result["passed"]:
        result["status"] = "failed"
    return result


def _tool_path_map(args: argparse.Namespace) -> dict[str, Any]:
    paths = {
        "qpdf": resolve_executable(args.qpdf, "qpdf"),
        "gs": resolve_executable(args.gs, "gs"),
        "pdftoppm": resolve_executable(args.pdftoppm, "pdftoppm"),
        "mutool": resolve_executable(args.mutool, "mutool"),
        "pdftotext": resolve_executable(args.pdftotext, "pdftotext"),
        "pdfinfo": resolve_executable(args.pdfinfo, "pdfinfo"),
        "python": args.python or sys.executable,
    }
    helper_python = paths["python"]
    if helper_python != sys.executable:
        paths["pypdf"] = run_command(
            [helper_python, "-c", "import pypdf"], 10.0
        )["status"] == "pass"
        paths["pymupdf"] = run_command(
            [helper_python, "-c", "import fitz"], 10.0
        )["status"] == "pass"
    else:
        paths["pypdf"] = module_version("pypdf")["available"]
        paths["pymupdf"] = module_version("fitz")["available"]
    return paths


def tool_provenance(tools: Mapping[str, Any], timeout: float) -> dict[str, Any]:
    versions = {
        "qpdf": probe_version(tools.get("qpdf"), ["--version"], timeout),
        "ghostscript": probe_version(tools.get("gs"), ["--version"], timeout),
        "pdftoppm": probe_version(tools.get("pdftoppm"), ["-v"], timeout),
        "mutool": probe_version(tools.get("mutool"), ["-v"], timeout),
        "pdftotext": probe_version(tools.get("pdftotext"), ["-v"], timeout),
        "pdfinfo": probe_version(tools.get("pdfinfo"), ["-v"], timeout),
    }
    return {
        "python": sys.version,
        "platform": platform.platform(),
        "numpy": module_version("numpy"),
        "pillow": module_version("PIL"),
        "pypdf": module_version("pypdf"),
        "pymupdf": module_version("fitz"),
        "executables": dict(tools),
        "versions": versions,
    }


def _required_tool_failures(tools: Mapping[str, Any]) -> list[str]:
    missing: list[str] = []
    for name in ("qpdf", "gs", "pdftoppm", "pdftotext", "pypdf"):
        if not tools.get(name):
            missing.append(name)
    if not tools.get("mutool") and not tools.get("pymupdf"):
        missing.append("mutool-or-pymupdf")
    return missing


def renderer_engine_metadata(renderer: str, tools: Mapping[str, Any]) -> dict[str, Any]:
    versions = tools.get("_versions", {})
    if renderer == "poppler":
        return {
            "engine": "Poppler pdftoppm",
            "executable": tools.get("pdftoppm"),
            "version": versions.get("pdftoppm", {}).get("version"),
        }
    if tools.get("mutool"):
        return {
            "engine": "MuPDF mutool",
            "executable": tools.get("mutool"),
            "version": versions.get("mutool", {}).get("version"),
        }
    return {
        "engine": "PyMuPDF",
        "executable": tools.get("python"),
        "version": tools.get("_pymupdf_version"),
    }


def _cleanup_case(case_dir: Path, result: Mapping[str, Any]) -> None:
    """Remove passed intermediate rasters while keeping failure evidence."""

    comparisons = result.get("render_comparisons", {})
    render_root = case_dir / "renders"
    for renderer, comparison in comparisons.items():
        if comparison.get("passed"):
            shutil.rmtree(render_root / renderer, ignore_errors=True)
        elif comparison.get("failure_artifacts"):
            shutil.rmtree(render_root / renderer, ignore_errors=True)
    if result.get("overall_pass"):
        shutil.rmtree(case_dir / "text", ignore_errors=True)
        shutil.rmtree(case_dir / "semantic", ignore_errors=True)


def _base_result(index: int, record: Mapping[str, Any], case_dir: Path) -> dict[str, Any]:
    return {
        "schema_version": SCHEMA_VERSION,
        "index": index,
        "id": manifest_id(record, index),
        "manifest": json_safe(dict(record)),
        "expected_valid": expected_validity(record),
        "case_directory": os.fspath(case_dir),
        "status": "not_started",
        "overall_pass": False,
        "failures": [],
    }


def process_record(
    index: int,
    record: Mapping[str, Any],
    *,
    input_dir: Path,
    output_root: Path,
    compressor: str,
    compressor_sha256: str | None,
    tools: Mapping[str, Any],
    preset: str,
    dpi: int,
    limit: int | None,
    timeout: float,
    max_raster_megapixels: float,
    threshold: float,
) -> dict[str, Any]:
    started = time.monotonic()
    case_deadline = started + timeout
    deadline_token = _CASE_DEADLINE.set(case_deadline)

    def budget() -> float:
        # The timeout is both the per-tool bound and the per-PDF wall-clock
        # budget.  A hostile document cannot consume timeout seconds once for
        # every oracle and renderer in sequence.
        return max(0.1, min(timeout, case_deadline - time.monotonic()))

    case_name = f"{index + 1:06d}-{safe_component(manifest_id(record, index), f'record-{index + 1:06d}')}"
    case_dir = output_root / "cases" / case_name
    if case_dir.exists():
        shutil.rmtree(case_dir)
    case_dir.mkdir(parents=True, exist_ok=True)
    result = _base_result(index, record, case_dir)
    expected_valid = expected_validity(record)
    result["expectation"] = {"expected_valid": expected_valid, "passed": None}
    result["compressor"] = {"path": compressor, "sha256": compressor_sha256}
    result["config"] = {
        "preset": preset,
        "dpi": dpi,
        "limit": limit,
        "timeout_seconds": timeout,
        "case_budget_seconds": timeout,
        "max_raster_megapixels": max_raster_megapixels,
        "quality_threshold": threshold,
    }

    def finish(status: str, failures: Iterable[str]) -> dict[str, Any]:
        result["status"] = status
        result["failures"] = list(dict.fromkeys(failures))
        expected_rejection = result.get("expectation", {}).get("passed") is True
        negative_accepted = result.get("expectation", {}).get("outcome") == "negative_accepted"
        result["overall_pass"] = not negative_accepted and (expected_rejection or not result["failures"])
        result["elapsed_seconds"] = round(time.monotonic() - started, 6)
        _cleanup_case(case_dir, result)
        write_json(case_dir / "result.json", result)
        _CASE_DEADLINE.reset(deadline_token)
        return result

    manifest_error = record.get("_manifest_error")
    if manifest_error:
        return finish("manifest_error", [str(manifest_error)])
    relative_input = manifest_path(record)
    if not relative_input:
        return finish("manifest_error", ["manifest record has no path/file field"])
    input_path = resolve_input(input_dir, relative_input)
    result["input"] = {"manifest_path": relative_input, "path": os.fspath(input_path)}
    if not input_path.is_file():
        return finish("input_missing", ["input file does not exist"])

    try:
        source_sha, source_bytes = sha256_file(input_path)
    except OSError as error:
        return finish("input_error", [f"could not read input: {error}"])
    result["input"].update({"sha256": source_sha, "bytes": source_bytes})
    expected = expected_sha256(record)
    if expected:
        result["input"]["expected_sha256"] = expected
        result["input"]["sha256_match"] = source_sha == expected

    source_checks = structural_checks(input_path, tools, budget())
    result["source_checks"] = source_checks
    source_snapshot = semantic_snapshot(input_path, "source", tools, case_dir, budget())
    source_text = extract_text(input_path, "source", tools, case_dir, budget())
    result["source_semantics"] = source_snapshot
    result["source_text"] = source_text

    failures: list[str] = []
    if expected and source_sha != expected:
        failures.append("manifest_sha256_mismatch")
    if source_checks.get("invalid"):
        failures.append("source_invalid")
    elif source_checks.get("unavailable") or not source_checks.get("valid"):
        failures.append("source_structural_check_unavailable")
    if source_snapshot.get("status") != "pass":
        failures.append("source_semantic_snapshot_failed")
    if source_text.get("status") != "pass":
        failures.append("source_text_extraction_failed")
    if limit is not None and source_bytes > limit:
        result["compression"] = {"status": "input_limit", "error": f"{source_bytes} > {limit}"}
        failures.append("input_exceeds_limit")
        return finish("input_limit", failures)

    output_path = case_dir / "output.pdf"
    # ``dpi`` controls the independent 150-DPI rendering policy.  Leave the
    # compressor's preset defaults intact so prepress is evaluated at its own
    # target resolution; a caller can still pass a separately extended CLI
    # command by selecting a preset whose documented defaults are desired.
    command = [compressor, "--preset", preset]
    if limit is not None:
        command.extend(["--max-input-bytes", str(limit)])
    command.extend(["--force", os.fspath(input_path), os.fspath(output_path)])
    compression_step = run_command(command, budget())
    result["compression"] = compression_step
    if compression_step.get("status") != "pass":
        failures.append(f"compressor_{compression_step.get('status', 'failed')}")
    if not output_path.is_file():
        if expected_valid is False and explicit_compressor_rejection(compression_step):
            result["expectation"] = {
                "expected_valid": False,
                "passed": True,
                "outcome": "expected_rejection",
            }
            return finish("expected_rejection", failures)
        if expected_valid is False and compression_step.get("status") in {
            "timeout",
            "tool_unavailable",
            "spawn_error",
        }:
            failures.append("negative_rejection_unbounded_or_unavailable")
        failures.append("output_missing")
        if expected_valid is False:
            result["expectation"] = {
                "expected_valid": False,
                "passed": False,
                "outcome": "rejection_unverified",
            }
        return finish("output_missing", failures)

    try:
        output_sha, output_bytes = sha256_file(output_path)
    except OSError as error:
        failures.append(f"output_read_error:{error}")
        return finish("output_error", failures)
    result["output"] = {"path": os.fspath(output_path), "sha256": output_sha, "bytes": output_bytes}
    if expected_valid is False:
        result["expectation"] = {
            "expected_valid": False,
            "passed": False,
            "outcome": "negative_accepted",
        }
        failures.append("negative_accepted")
    output_checks = structural_checks(output_path, tools, budget())
    result["output_checks"] = output_checks
    if output_checks.get("invalid"):
        failures.append("output_invalid")
    elif output_checks.get("unavailable") or not output_checks.get("valid"):
        failures.append("output_structural_check_unavailable")

    output_snapshot = semantic_snapshot(output_path, "output", tools, case_dir, budget())
    output_text = extract_text(output_path, "output", tools, case_dir, budget())
    result["output_semantics"] = output_snapshot
    result["output_text"] = output_text
    semantic = compare_semantics(source_snapshot, output_snapshot, source_text, output_text)
    result["semantic_comparison"] = semantic
    if not semantic.get("passed"):
        failures.append("semantic_comparison_failed")

    renderers: dict[str, Any] = {}
    comparisons: dict[str, Any] = {}
    for renderer in ("poppler", "mupdf"):
        source_preflight = raster_preflight(source_snapshot, dpi, max_raster_megapixels)
        output_preflight = raster_preflight(output_snapshot, dpi, max_raster_megapixels)
        if source_preflight["status"] == "resource_limit":
            source_render = {
                "status": "resource_limit",
                "pages": [],
                "preflight": source_preflight,
                "error": source_preflight["error"],
            }
        else:
            source_render = render_pdf(
                input_path,
                renderer,
                "source",
                tools,
                case_dir,
                budget(),
                dpi,
                max_raster_megapixels,
                source_snapshot.get("snapshot", {}).get("page_count")
                if source_snapshot.get("status") == "pass"
                else None,
            )
            source_render["preflight"] = source_preflight
        if output_preflight["status"] == "resource_limit":
            output_render = {
                "status": "resource_limit",
                "pages": [],
                "preflight": output_preflight,
                "error": output_preflight["error"],
            }
        else:
            output_render = render_pdf(
                output_path,
                renderer,
                "output",
                tools,
                case_dir,
                budget(),
                dpi,
                max_raster_megapixels,
                output_snapshot.get("snapshot", {}).get("page_count")
                if output_snapshot.get("status") == "pass"
                else None,
            )
            output_render["preflight"] = output_preflight
        engine = renderer_engine_metadata(renderer, tools)
        source_render["engine"] = engine
        output_render["engine"] = engine
        renderers[renderer] = {"engine": engine, "source": source_render, "output": output_render}
        comparison = compare_rendered_pages(
            renderer,
            source_render,
            output_render,
            case_dir,
            preset,
            threshold,
        )
        comparisons[renderer] = comparison
        if not comparison.get("passed"):
            failures.append(f"{renderer}_raster_gate_failed")
    result["renders"] = renderers
    result["render_comparisons"] = comparisons
    if result.get("expectation", {}).get("outcome") == "negative_accepted":
        result["expectation"]["validated"] = bool(
            output_checks.get("valid")
            and semantic.get("passed")
            and all(item.get("passed") for item in comparisons.values())
        )

    if failures:
        if result.get("expectation", {}).get("outcome") == "negative_accepted":
            return finish("negative_accepted", failures)
        return finish("failed", failures)
    return finish("passed", [])


def parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", required=True, type=Path)
    parser.add_argument("--input-dir", required=True, type=Path)
    parser.add_argument("--output-dir", required=True, type=Path)
    parser.add_argument("--compressor", type=str, default="target/release/pdf-compress")
    parser.add_argument("--preset", choices=("lossless", "prepress"), default="lossless")
    parser.add_argument("--workers", type=int, default=1)
    parser.add_argument("--timeout", type=float, default=DEFAULT_TIMEOUT)
    parser.add_argument("--dpi", type=int, default=DEFAULT_DPI)
    parser.add_argument(
        "--limit",
        type=int,
        default=None,
        help="optional maximum input bytes; passed to the compressor as --max-input-bytes",
    )
    parser.add_argument(
        "--max-raster-megapixels",
        type=float,
        default=DEFAULT_MAX_RASTER_MEGAPIXELS,
        help="fail a page explicitly when its 150-DPI raster exceeds this size",
    )
    parser.add_argument("--qpdf", default=None)
    parser.add_argument("--gs", default=None)
    parser.add_argument("--pdftoppm", default=None)
    parser.add_argument("--mutool", default=None)
    parser.add_argument("--pdftotext", default=None)
    parser.add_argument("--pdfinfo", default=None)
    parser.add_argument("--python", default=None, help="Python executable for pypdf/PyMuPDF helpers")
    parser.add_argument("--overwrite", action="store_true", help="allow a non-empty output directory")
    return parser.parse_args(argv)


def _write_summary(output_dir: Path, summary: Mapping[str, Any]) -> None:
    write_json(output_dir / "summary.json", summary)
    counts = summary["counts"]
    lines = [
        f"Corpus verification ({summary['config']['preset']})",
        f"Documents: {counts['total']} total; {counts['passed']} passed; {counts['failed']} failed",
        f"Expected rejections: {counts['expected_rejections']}; negative accepted: {counts['negative_accepted']} ({counts['negative_accepted_validated']} validated); unexpected acceptance: {counts['unexpected_acceptance']}",
        f"Source invalid: {counts['source_invalid']}; missing output: {counts['output_missing']}; source/tool errors: {counts['infrastructure_or_input_errors']}",
        f"Quality denominator: {counts['total']} (failed and missing-output documents remain counted)",
        f"Results: {output_dir / 'results.jsonl'}",
    ]
    (output_dir / "summary.txt").write_text("\n".join(lines) + "\n", encoding="utf-8")


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(argv)
    if args.workers < 1 or args.timeout <= 0 or args.dpi <= 0:
        raise SystemExit("--workers, --timeout, and --dpi must be positive")
    if args.limit is not None and args.limit <= 0:
        raise SystemExit("--limit must be positive")
    if args.max_raster_megapixels <= 0 or not math.isfinite(args.max_raster_megapixels):
        raise SystemExit("--max-raster-megapixels must be positive and finite")
    if not 0.0 <= 0.99 <= 1.0:
        raise AssertionError("invalid fixed quality threshold")
    threshold = 0.99

    manifest_path_value = args.manifest.expanduser().resolve()
    input_dir = args.input_dir.expanduser().resolve()
    output_dir = args.output_dir.expanduser().resolve()
    if not manifest_path_value.is_file():
        raise SystemExit(f"manifest does not exist: {manifest_path_value}")
    if not input_dir.is_dir():
        raise SystemExit(f"input directory does not exist: {input_dir}")
    if output_dir.exists() and any(output_dir.iterdir()) and not args.overwrite:
        raise SystemExit(f"output directory is not empty; use --overwrite: {output_dir}")
    output_dir.mkdir(parents=True, exist_ok=True)
    if args.overwrite:
        shutil.rmtree(output_dir / "cases", ignore_errors=True)
        for name in ("results.jsonl", "summary.json", "summary.txt", "provenance.json"):
            (output_dir / name).unlink(missing_ok=True)
    (output_dir / "cases").mkdir(parents=True, exist_ok=True)

    compressor_path = resolve_executable(args.compressor, args.compressor)
    if not compressor_path:
        raise SystemExit(f"compressor executable not found: {args.compressor}")
    frozen_dir = output_dir / "tools"
    frozen_dir.mkdir(parents=True, exist_ok=True)
    frozen_compressor = frozen_dir / Path(compressor_path).name
    shutil.copy2(compressor_path, frozen_compressor)
    frozen_compressor.chmod(frozen_compressor.stat().st_mode | 0o100)
    compressor_sha, _ = sha256_file(frozen_compressor)
    frozen_verifier = frozen_dir / Path(__file__).name
    shutil.copy2(Path(__file__).resolve(), frozen_verifier)
    verifier_sha, _ = sha256_file(frozen_verifier)

    try:
        records = load_manifest(manifest_path_value)
    except (OSError, ValueError, json.JSONDecodeError) as error:
        raise SystemExit(f"could not read manifest: {error}") from error
    if not records:
        raise SystemExit("manifest contains no records")

    tools = _tool_path_map(args)
    provenance = {
        "schema_version": SCHEMA_VERSION,
        "started_at": utc_now(),
        "manifest": os.fspath(manifest_path_value),
        "manifest_sha256": sha256_file(manifest_path_value)[0],
        "input_dir": os.fspath(input_dir),
        "output_dir": os.fspath(output_dir),
        "compressor": {"path": os.fspath(frozen_compressor), "sha256": compressor_sha},
        "verifier": {"path": os.fspath(frozen_verifier), "sha256": verifier_sha},
        "tools": tool_provenance(tools, args.timeout),
        "config": {
            "preset": args.preset,
            "workers": args.workers,
            "timeout_seconds": args.timeout,
            "dpi": args.dpi,
            "limit": args.limit,
            "max_raster_megapixels": args.max_raster_megapixels,
            "quality_threshold": threshold,
        },
    }
    tools["_versions"] = provenance["tools"]["versions"]
    write_json(output_dir / "provenance.json", provenance)

    common = {
        "input_dir": input_dir,
        "output_root": output_dir,
        "compressor": os.fspath(frozen_compressor),
        "compressor_sha256": compressor_sha,
        "tools": tools,
        "preset": args.preset,
        "dpi": args.dpi,
        "limit": args.limit,
        "timeout": args.timeout,
        "max_raster_megapixels": args.max_raster_megapixels,
        "threshold": threshold,
    }

    def run_one(item: tuple[int, dict[str, Any]]) -> dict[str, Any]:
        index, record = item
        try:
            return process_record(index, record, **common)
        except Exception as error:  # Every manifest record still gets a JSONL outcome.
            case_name = f"{index + 1:06d}-{safe_component(manifest_id(record, index), f'record-{index + 1:06d}')}"
            case_dir = output_dir / "cases" / case_name
            case_dir.mkdir(parents=True, exist_ok=True)
            result = _base_result(index, record, case_dir)
            result.update(
                {
                    "status": "internal_error",
                    "failures": [f"runner_exception:{type(error).__name__}:{error}"],
                    "elapsed_seconds": 0.0,
                }
            )
            write_json(case_dir / "result.json", result)
            return result

    results_path = output_dir / "results.jsonl"
    results: list[dict[str, Any]] = []
    indexed_records = list(enumerate(records))
    with results_path.open("w", encoding="utf-8") as stream:
        if args.workers == 1:
            completed: Iterable[dict[str, Any]] = (run_one(item) for item in indexed_records)
        else:
            executor = concurrent.futures.ThreadPoolExecutor(max_workers=args.workers)
            futures = [executor.submit(run_one, item) for item in indexed_records]
            completed = (future.result() for future in futures)
        for result in completed:
            results.append(result)
            stream.write(json.dumps(json_safe(result), ensure_ascii=True, sort_keys=True) + "\n")
            stream.flush()
        if args.workers != 1:
            executor.shutdown(wait=True)

    counts = {
        "total": len(results),
        "passed": sum(1 for result in results if result.get("overall_pass")),
        "failed": sum(1 for result in results if not result.get("overall_pass")),
        "expected_rejections": sum(
            1 for result in results if result.get("expectation", {}).get("outcome") == "expected_rejection"
        ),
        "unexpected_acceptance": sum(
            1 for result in results if result.get("expectation", {}).get("outcome") == "unexpected_acceptance"
        ),
        "negative_accepted": sum(
            1 for result in results if result.get("expectation", {}).get("outcome") == "negative_accepted"
        ),
        "negative_accepted_validated": sum(
            1
            for result in results
            if result.get("expectation", {}).get("outcome") == "negative_accepted"
            and result.get("expectation", {}).get("validated")
        ),
        "source_invalid": sum(1 for result in results if "source_invalid" in result.get("failures", [])),
        "output_missing": sum(1 for result in results if "output_missing" in result.get("failures", [])),
        "infrastructure_or_input_errors": sum(
            1
            for result in results
            if result.get("status") in {"input_missing", "input_error", "internal_error"}
            or any("unavailable" in failure or "failed" in failure for failure in result.get("failures", []))
        ),
    }
    missing_tools = _required_tool_failures(tools)
    provenance["finished_at"] = utc_now()
    provenance["required_tool_failures"] = missing_tools
    write_json(output_dir / "provenance.json", provenance)
    summary = {
        "schema_version": SCHEMA_VERSION,
        "started_at": provenance["started_at"],
        "finished_at": provenance["finished_at"],
        "manifest": os.fspath(manifest_path_value),
        "results_jsonl": os.fspath(results_path),
        "verifier": provenance["verifier"],
        "compressor": provenance["compressor"],
        "config": provenance["config"],
        "counts": counts,
        "quality_denominator": counts["total"],
        "pass_rate": counts["passed"] / counts["total"] if counts["total"] else 0.0,
        "required_tool_failures": missing_tools,
        "all_passed": counts["passed"] == counts["total"] and not missing_tools,
    }
    _write_summary(output_dir, {"config": provenance["config"], "counts": counts, **summary})
    print(json.dumps(summary, sort_keys=True))
    if missing_tools:
        return 2
    return 0 if summary["all_passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
