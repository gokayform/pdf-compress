#!/usr/bin/env python3
"""Acquire a bounded, reproducible public PDF.js compatibility corpus.

The checked-in manifest describes files obtained from one pinned Mozilla
pdf.js revision.  The PDF bytes themselves belong under ``target/`` and are
never written to the repository.  A run discovers the source tree and test
manifest from GitHub, applies the deterministic curation policy below, and
records every selected download, including failed or intentionally negative
fixtures.

Only Python's standard library is used.  Network requests have bounded
timeouts and retries.  A single response is capped at five MiB and a complete
run is capped at 150 MiB.  The caps are enforced while streaming, rather than
trusting Content-Length.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import sys
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Iterable
from urllib.error import HTTPError, URLError
from urllib.parse import quote
from urllib.request import Request, urlopen


ROOT = Path(__file__).resolve().parents[2]
MANIFEST_PATH = ROOT / "tests" / "corpus" / "manifest.json"
DEFAULT_OUTPUT = ROOT / "target" / "pdf-compress-corpus"

REPOSITORY = "mozilla/pdf.js"
PINNED_REVISION = "c33c32aed46637ec6010e9f6c031c72e1c529d31"
TREE_URL = (
    f"https://api.github.com/repos/{REPOSITORY}/git/trees/"
    f"{PINNED_REVISION}?recursive=1"
)
TEST_MANIFEST_URL = (
    f"https://raw.githubusercontent.com/{REPOSITORY}/{PINNED_REVISION}/"
    "test/test_manifest.json"
)
RAW_BASE_URL = (
    f"https://raw.githubusercontent.com/{REPOSITORY}/{PINNED_REVISION}/"
)

SUPPLEMENT_SOURCES: tuple[dict[str, Any], ...] = (
    {
        "name": "rfc8259",
        "url": "https://www.rfc-editor.org/rfc/pdfrfc/rfc8259.txt.pdf",
        "category": "documents",
        "tags": ("documents", "real-world"),
        "description": "RFC 8259 JSON specification PDF",
    },
    {
        "name": "pdf20-an002-af",
        "url": "https://pdfa.org/download-area/publications/PDF20_AN002-AF.pdf",
        "category": "documents",
        "tags": ("documents", "standards", "real-world"),
        "description": "PDF Association PDF 2.0 publication",
    },
    {
        "name": "wai-pageauth-tech",
        "url": "https://www.w3.org/WAI/GL/WD-WAI-PAGEAUTH-19990104/wai-pageauth-tech.pdf",
        "category": "documents",
        "tags": ("documents", "accessibility", "real-world"),
        "description": "W3C page-authoring techniques working draft",
    },
    {
        "name": "wcag22-headers-footers",
        "url": "https://www.w3.org/WAI/WCAG22/working-examples/pdf-headers-footers/headers-footers-oo.pdf",
        "category": "forms",
        "tags": ("documents", "forms", "real-world"),
        "description": "W3C WCAG PDF headers and footers working example",
    },
    {
        "name": "attention-is-all-you-need",
        "url": "https://arxiv.org/pdf/1706.03762",
        "category": "documents",
        "tags": ("documents", "papers", "real-world"),
        "description": "arXiv research paper 1706.03762",
    },
    {
        "name": "bert",
        "url": "https://arxiv.org/pdf/1810.04805",
        "category": "documents",
        "tags": ("documents", "papers", "real-world"),
        "description": "arXiv research paper 1810.04805",
    },
)

MAX_FILE_BYTES = 5 * 1024 * 1024
MAX_TOTAL_BYTES = 150 * 1024 * 1024
REQUEST_TIMEOUT_SECONDS = 30
RETRIES = 3
MIN_POSITIVE_FILES = 100
DEFAULT_POSITIVE_FILES = 120
DEFAULT_NEGATIVE_FILES = 16
USER_AGENT = "pdf-compress-corpus/1.0 (public compatibility corpus)"


# These are deliberately descriptive hints, not claims about the PDF internals.
# The source test manifest and the downloaded bytes remain the provenance of
# each entry; a reviewer can change the curation policy without changing the
# pinned source revision.
CATEGORY_RULES: tuple[tuple[str, tuple[str, ...]], ...] = (
    (
        "forms",
        (
            "acroform",
            "annotation",
            "annot",
            "field",
            "form",
            "freetext",
            "listbox",
            "resetform",
            "stamp",
            "widget",
            "xfa",
        ),
    ),
    (
        "transparency",
        (
            "alpha",
            "blend",
            "gradient",
            "knockout",
            "mesh",
            "opacity",
            "overprint",
            "pattern",
            "shading",
            "smask",
            "transparent",
            "transparency",
        ),
    ),
    (
        "images",
        (
            "bitmap",
            "ccitt",
            "cmyk",
            "image",
            "jbig",
            "jpeg",
            "jpg",
            "jp2",
            "jpx",
            "photo",
            "picture",
            "scan",
            "tiff",
        ),
    ),
    (
        "object-streams",
        (
            "compressed",
            "incremental",
            "lineariz",
            "objstm",
            "objectstream",
            "pdfkit",
            "xref",
        ),
    ),
    (
        "encoding",
        (
            "arabic",
            "asciihex",
            "cmap",
            "copy_paste",
            "diacritic",
            "eucjp",
            "encoding",
            "escape",
            "jis",
            "ligature",
            "noembed",
            "plusminus",
            "saslprep",
            "sjis",
            "unicode",
            "utf",
        ),
    ),
    (
        "fonts",
        (
            "cff",
            "cid",
            "dingbat",
            "font",
            "glyph",
            "truetype",
            "type3",
            "ttf",
        ),
    ),
)

DOCUMENT_HINTS = (
    "freeculture.pdf",
    "franz.pdf",
    "franz_2.pdf",
    "openoffice.pdf",
    "pdfjs_wikipedia.pdf",
    "personwithdog.pdf",
    "prefilled_f1040.pdf",
    "scorecard_reduced.pdf",
    "tamreview.pdf",
    "tracemonkey.pdf",
)

NEGATIVE_HINTS = (
    "fuzzed",
    "malformed",
    "invalid",
    "corrupt",
    "truncated",
    "unreadable",
    "encrypted",
    "protected",
    "secHandler",
    "signed_verified",
    "bomb_giant",
    "pdfjsbad",
    "helloworld-bad",
    "scan-bad",
    "bad-pagelabels",
    "bad-appearance",
    "boundingbox_invalid",
    "contentstreamcycle",
    "xref_command_missing",
)


@dataclass(frozen=True)
class Candidate:
    source_path: str
    category: str
    tags: tuple[str, ...]
    evidence_class: str
    source_ids: tuple[str, ...]
    source_types: tuple[str, ...]
    source_links: tuple[bool, ...]
    source_url: str | None = None
    origin_kind: str = "github_pdfjs"
    origin_revision: str = PINNED_REVISION
    source_description: str | None = None


class DownloadError(Exception):
    """A bounded download failed or exceeded a curation limit."""


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--output",
        type=Path,
        default=DEFAULT_OUTPUT,
        help="directory for downloaded PDFs (default: target/pdf-compress-corpus)",
    )
    parser.add_argument(
        "--manifest",
        type=Path,
        default=MANIFEST_PATH,
        help="checked-in JSON manifest to write",
    )
    parser.add_argument(
        "--positive-count",
        type=int,
        default=DEFAULT_POSITIVE_FILES,
        help=f"positive files to acquire, at least {MIN_POSITIVE_FILES} (default: {DEFAULT_POSITIVE_FILES})",
    )
    parser.add_argument(
        "--negative-count",
        type=int,
        default=DEFAULT_NEGATIVE_FILES,
        help=f"negative/stress files to attempt (default: {DEFAULT_NEGATIVE_FILES})",
    )
    parser.add_argument(
        "--timeout",
        type=float,
        default=REQUEST_TIMEOUT_SECONDS,
        help=f"per-request timeout in seconds (default: {REQUEST_TIMEOUT_SECONDS:g})",
    )
    parser.add_argument(
        "--retries",
        type=int,
        default=RETRIES,
        help=f"attempts per request (default: {RETRIES})",
    )
    args = parser.parse_args()
    if args.positive_count < MIN_POSITIVE_FILES:
        parser.error(f"--positive-count must be at least {MIN_POSITIVE_FILES}")
    if args.negative_count < 0:
        parser.error("--negative-count must not be negative")
    if args.timeout <= 0:
        parser.error("--timeout must be positive")
    if args.retries < 1 or args.retries > 8:
        parser.error("--retries must be between 1 and 8")
    return args


def request_bytes(url: str, *, timeout: float, retries: int, limit: int) -> bytes:
    """Fetch one bounded response with deterministic retry behavior."""

    last_error: Exception | None = None
    for attempt in range(retries):
        try:
            request = Request(
                url,
                headers={
                    "Accept": "application/json, application/octet-stream",
                    "User-Agent": USER_AGENT,
                },
            )
            with urlopen(request, timeout=timeout) as response:
                content_length = response.headers.get("Content-Length")
                if content_length is not None:
                    try:
                        if int(content_length) > limit:
                            raise DownloadError(
                                f"response advertises {content_length} bytes, limit is {limit}"
                            )
                    except ValueError:
                        pass
                body = response.read(limit + 1)
                if len(body) > limit:
                    raise DownloadError(f"response exceeds bounded limit of {limit} bytes")
                return body
        except HTTPError as error:
            last_error = error
            if error.code < 500 and error.code != 429:
                break
        except (DownloadError, OSError, URLError) as error:
            last_error = error
        if attempt + 1 < retries:
            time.sleep(min(2.0**attempt, 4.0))
    raise DownloadError(f"{url}: {last_error}")


def fetch_json(url: str, *, timeout: float, retries: int) -> Any:
    try:
        return json.loads(request_bytes(url, timeout=timeout, retries=retries, limit=MAX_FILE_BYTES))
    except json.JSONDecodeError as error:
        raise DownloadError(f"{url}: invalid JSON: {error}") from error


def discover_pdf_paths(*, timeout: float, retries: int) -> list[str]:
    tree = fetch_json(TREE_URL, timeout=timeout, retries=retries)
    if tree.get("truncated"):
        raise DownloadError("GitHub tree response was truncated; refusing an incomplete corpus")
    paths = {
        item["path"]
        for item in tree.get("tree", [])
        if item.get("type") == "blob"
        and item.get("path", "").lower().endswith(".pdf")
        and (
            item["path"].startswith("test/pdfs/")
            or item["path"] == "examples/learning/helloworld.pdf"
        )
    }
    if not paths:
        raise DownloadError("pinned pdf.js tree contained no PDF fixtures")
    return sorted(paths)


def source_index(*, timeout: float, retries: int) -> dict[str, list[dict[str, Any]]]:
    manifest = fetch_json(TEST_MANIFEST_URL, timeout=timeout, retries=retries)
    index: dict[str, list[dict[str, Any]]] = {}
    for item in manifest:
        if not isinstance(item, dict):
            continue
        source_file = item.get("file")
        if not isinstance(source_file, str) or not source_file.endswith(".pdf"):
            continue
        path = source_file if source_file.startswith("test/") else f"test/{source_file}"
        index.setdefault(path, []).append(item)
    return index


def classify(source_path: str, index: dict[str, list[dict[str, Any]]]) -> Candidate:
    lowered = source_path.lower()
    basename = lowered.rsplit("/", 1)[-1]
    evidence_class = "positive_compatibility"
    if any(hint.lower() in basename for hint in NEGATIVE_HINTS):
        evidence_class = "negative_stress"
    if "contentstreamcycle" in basename and "nocycle" not in basename:
        evidence_class = "negative_stress"

    tags = {
        category
        for category, hints in CATEGORY_RULES
        if any(hint in basename for hint in hints)
    }
    if basename in {hint.lower() for hint in DOCUMENT_HINTS} or not tags:
        tags.add("documents")
    if basename == "pdfkit_compressed.pdf":
        tags.add("object-streams")

    category_order = (
        "documents",
        "forms",
        "images",
        "fonts",
        "transparency",
        "encoding",
        "object-streams",
    )
    category = next((name for name in category_order if name in tags), "documents")
    source_items = index.get(source_path, [])
    return Candidate(
        source_path=source_path,
        category=category,
        tags=tuple(sorted(tags)),
        evidence_class=evidence_class,
        source_ids=tuple(str(item["id"]) for item in source_items if "id" in item),
        source_types=tuple(str(item["type"]) for item in source_items if "type" in item),
        source_links=tuple(bool(item.get("link", False)) for item in source_items),
    )


def supplement_candidates() -> list[Candidate]:
    candidates = []
    for source in SUPPLEMENT_SOURCES:
        url = str(source["url"])
        candidates.append(
            Candidate(
                source_path=f"supplement/{source['name']}.pdf",
                category=str(source["category"]),
                tags=tuple(sorted(str(tag) for tag in source["tags"])),
                evidence_class="positive_compatibility",
                source_ids=(),
                source_types=(),
                source_links=(),
                source_url=url,
                origin_kind="external_url",
                origin_revision=f"url-sha256:{hashlib.sha256(url.encode('utf-8')).hexdigest()}",
                source_description=str(source["description"]),
            )
        )
    return candidates


def curated_candidates(
    paths: Iterable[str], index: dict[str, list[dict[str, Any]]], *, positive_count: int, negative_count: int
) -> tuple[list[Candidate], list[Candidate]]:
    candidates = [classify(path, index) for path in paths]
    positives = sorted(
        (item for item in candidates if item.evidence_class == "positive_compatibility"),
        key=lambda item: item.source_path,
    )
    positives = supplement_candidates() + positives
    negatives = sorted(
        (item for item in candidates if item.evidence_class == "negative_stress"),
        key=lambda item: item.source_path,
    )

    selected: list[Candidate] = []
    selected_paths: set[str] = set()

    # Make the common feature classes visible before filling the remainder with
    # deterministic PDF.js regression documents.  Categories may overlap.
    category_minimums = {
        "documents": 12,
        "forms": 12,
        "images": 12,
        "fonts": 12,
        "transparency": 10,
        "encoding": 6,
        "object-streams": 2,
    }
    for category, minimum in category_minimums.items():
        matching = [item for item in positives if category in item.tags]
        for item in matching[:minimum]:
            if item.source_path not in selected_paths:
                selected.append(item)
                selected_paths.add(item.source_path)

    for item in positives:
        if len(selected) >= positive_count:
            break
        if item.source_path not in selected_paths:
            selected.append(item)
            selected_paths.add(item.source_path)

    return selected, negatives[:negative_count]


def encoded_source_url(candidate: Candidate) -> str:
    if candidate.source_url is not None:
        return candidate.source_url
    return RAW_BASE_URL + quote(candidate.source_path, safe="/")


def is_encrypted_pdf(data: bytes) -> bool:
    # This is intentionally conservative.  A false positive excludes a file
    # from positive evidence; it never silently treats an encrypted PDF as
    # compatible evidence.
    return re.search(rb"/Encrypt(?=[\x00-\x20\x28\x29\x3c\x3e\x5b\x5d\x7b\x7d/%%])", data) is not None


def safe_name(source_path: str, index: int, evidence_class: str) -> Path:
    basename = source_path.rsplit("/", 1)[-1]
    cleaned = re.sub(r"[^A-Za-z0-9._-]+", "_", basename)
    prefix = "negative" if evidence_class == "negative_stress" else "positive"
    return Path(prefix) / f"{index:03d}-{cleaned}"


def download_file(
    url: str,
    destination: Path,
    *,
    timeout: float,
    retries: int,
    remaining_budget: int,
) -> tuple[int, str]:
    """Stream one file to an atomic temporary path and return size/hash."""

    if remaining_budget <= 0:
        raise DownloadError("total download budget exhausted")
    destination.parent.mkdir(parents=True, exist_ok=True)
    temporary = destination.with_name(destination.name + ".part")
    last_error: Exception | None = None
    limit = min(MAX_FILE_BYTES, remaining_budget)
    for attempt in range(retries):
        try:
            if temporary.exists():
                temporary.unlink()
            request = Request(url, headers={"User-Agent": USER_AGENT})
            with urlopen(request, timeout=timeout) as response, temporary.open("wb") as output:
                content_length = response.headers.get("Content-Length")
                if content_length is not None:
                    try:
                        advertised = int(content_length)
                    except ValueError:
                        advertised = 0
                    if advertised > limit:
                        raise DownloadError(
                            f"response advertises {advertised} bytes, bounded remaining budget is {limit}"
                        )
                digest = hashlib.sha256()
                size = 0
                while True:
                    chunk = response.read(min(64 * 1024, limit - size + 1))
                    if not chunk:
                        break
                    size += len(chunk)
                    if size > limit:
                        raise DownloadError(f"file exceeds {MAX_FILE_BYTES} bytes or total budget")
                    output.write(chunk)
                    digest.update(chunk)
                output.flush()
                os.fsync(output.fileno())
            os.replace(temporary, destination)
            return size, digest.hexdigest()
        except HTTPError as error:
            last_error = error
            if error.code < 500 and error.code != 429:
                break
        except (DownloadError, OSError, URLError) as error:
            last_error = error
        if temporary.exists():
            temporary.unlink()
        if attempt + 1 < retries:
            time.sleep(min(2.0**attempt, 4.0))
    if temporary.exists():
        temporary.unlink()
    raise DownloadError(f"{url}: {last_error}")


def entry_base(candidate: Candidate, source_url: str) -> dict[str, Any]:
    entry = {
        "source_path": candidate.source_path,
        "source_url": source_url,
        "origin_kind": candidate.origin_kind,
        "origin_revision": candidate.origin_revision,
        "category": candidate.category,
        "tags": list(candidate.tags),
        "evidence_class": candidate.evidence_class,
        "expected_valid": candidate.evidence_class == "positive_compatibility",
        "source_test_ids": list(candidate.source_ids),
        "source_test_types": list(candidate.source_types),
        "source_test_links": list(candidate.source_links),
    }
    if candidate.source_description is not None:
        entry["source_description"] = candidate.source_description
    return entry


def acquire(args: argparse.Namespace) -> int:
    if args.output.is_file():
        raise DownloadError(f"--output is a file: {args.output}")
    args.output.mkdir(parents=True, exist_ok=True)
    args.manifest.parent.mkdir(parents=True, exist_ok=True)

    paths = discover_pdf_paths(timeout=args.timeout, retries=args.retries)
    index = source_index(timeout=args.timeout, retries=args.retries)
    positives, negatives = curated_candidates(
        paths,
        index,
        positive_count=args.positive_count,
        negative_count=args.negative_count,
    )
    if len(positives) < args.positive_count:
        raise DownloadError(
            f"pinned source has only {len(positives)} positive candidates; requested {args.positive_count}"
        )

    selected = positives + negatives
    entries: list[dict[str, Any]] = []
    total_bytes = 0
    positive_downloaded = 0
    positive_non_encrypted = 0
    positive_encrypted_excluded = 0
    positive_non_pdf_excluded = 0
    for number, candidate in enumerate(selected, start=1):
        source_url = encoded_source_url(candidate)
        entry = entry_base(candidate, source_url)
        local_path = safe_name(candidate.source_path, number, candidate.evidence_class)
        destination = args.output / local_path
        entry["local_path"] = local_path.as_posix()
        try:
            size, digest = download_file(
                source_url,
                destination,
                timeout=args.timeout,
                retries=args.retries,
                remaining_budget=MAX_TOTAL_BYTES - total_bytes,
            )
            # Files are already bounded to 5 MiB, so inspect the complete
            # response for an encryption marker rather than assuming the
            # trailer is within the first chunk.
            data = destination.read_bytes()
            pdf_header = data.startswith(b"%PDF-")
            encrypted = is_encrypted_pdf(data)
            entry.update(
                {
                    "status": "downloaded",
                    "size": size,
                    "sha256": digest,
                    "pdf_header": pdf_header,
                    "encrypted": encrypted,
                }
            )
            total_bytes += size
            if candidate.evidence_class == "positive_compatibility":
                positive_downloaded += 1
                if encrypted:
                    entry["evidence_class"] = "excluded_encrypted"
                    entry["expected_valid"] = False
                    entry["exclusion_reason"] = "encryption marker found in PDF header"
                    positive_encrypted_excluded += 1
                elif not pdf_header:
                    entry["evidence_class"] = "excluded_non_pdf"
                    entry["expected_valid"] = False
                    entry["exclusion_reason"] = "download did not begin with a PDF header"
                    positive_non_pdf_excluded += 1
                else:
                    positive_non_encrypted += 1
        except DownloadError as error:
            entry.update(
                {
                    "status": "failed",
                    "size": None,
                    "sha256": None,
                    "pdf_header": None,
                    "encrypted": None,
                    "failure": str(error),
                }
            )
        entries.append(entry)
        print(
            f"{entry['status']:>10} {candidate.evidence_class:>20} "
            f"{candidate.source_path}"
        )

    manifest = {
        "schema_version": 1,
        "corpus": "pdf-compress-public",
        "source": {
            "repository": REPOSITORY,
            "revision": PINNED_REVISION,
            "tree_url": TREE_URL,
            "test_manifest_url": TEST_MANIFEST_URL,
            "raw_base_url": RAW_BASE_URL,
            "selection_scope": "test/pdfs plus examples/learning/helloworld.pdf and fixed supplements",
            "supplements": [
                {
                    "name": source["name"],
                    "url": source["url"],
                    "category": source["category"],
                    "description": source["description"],
                }
                for source in SUPPLEMENT_SOURCES
            ],
        },
        "policy": {
            "max_file_bytes": MAX_FILE_BYTES,
            "max_total_bytes": MAX_TOTAL_BYTES,
            "request_timeout_seconds": args.timeout,
            "retries": args.retries,
            "minimum_positive_non_encrypted": MIN_POSITIVE_FILES,
            "positive_candidates_requested": args.positive_count,
            "negative_candidates_requested": args.negative_count,
            "selection_is_best_effort": True,
            "representativeness_claim": False,
        },
        "summary": {
            "entries": len(entries),
            "downloaded_bytes": total_bytes,
            "positive_downloaded": positive_downloaded,
            "positive_non_encrypted": positive_non_encrypted,
            "positive_encrypted_excluded": positive_encrypted_excluded,
            "positive_non_pdf_excluded": positive_non_pdf_excluded,
            "negative_attempted": len(negatives),
            "failed_downloads": sum(item["status"] == "failed" for item in entries),
        },
        "entries": entries,
    }
    temporary_manifest = args.manifest.with_name(args.manifest.name + ".part")
    temporary_manifest.write_text(
        json.dumps(manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    os.replace(temporary_manifest, args.manifest)

    if positive_non_encrypted < MIN_POSITIVE_FILES:
        raise DownloadError(
            f"only {positive_non_encrypted} non-encrypted positive PDFs acquired; "
            f"need at least {MIN_POSITIVE_FILES}. Manifest records all attempts."
        )
    print(
        f"acquired {positive_non_encrypted} non-encrypted positive PDFs and "
        f"{len(negatives)} negative/stress attempts ({total_bytes} bytes)"
    )
    print(f"manifest: {args.manifest}")
    print(f"corpus: {args.output}")
    return 0


def main() -> int:
    args = parse_args()
    try:
        return acquire(args)
    except DownloadError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
