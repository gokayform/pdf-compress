# Compressor verification

See the [public-corpus proof report](../docs/pdf-compression-proof.md) for
measured results, every exclusion, and the bugs the independent checks exposed.
The [compatibility follow-up](../docs/pdf-compression-compatibility-proof.md)
records eight repairs, three explicit rejections, and preservation of all 102
previously validator-clean outputs.

Compatibility repairs have an additional regression gate in
`scripts/check_compatibility_repairs.py`. Compare the full new `results.jsonl`
against the frozen previous run with `--before`, `--after`,
`--expected-rejections`, and `--output`. The rejection file is a JSON object
mapping exact corpus paths to required diagnostic substrings. It may only name
previously failing outputs. The gate checks unchanged corpus membership and
source hashes, preserves every formerly accepted validator-clean output, and
requires exact Poppler/MuPDF pixels, semantic preservation, qpdf, and strict
Ghostscript for repaired outputs. The ordinary corpus verifier still records
source failures; this extra gate does not relabel them as clean sources.

For the six JBIG2 fixtures whose original bytes expose decoder limitations,
`scripts/verify_jbig2_reference.py` adds a separate sample-level oracle. Supply
`--results`, a new `--output-dir`, `--pdfjs-module` (the installed PDF.js legacy
`pdf.mjs` path), and `--wasm-dir`. It uses PDFium's independent WASM decoder
through PDF.js to decode the original image, compares every packed sample with
the Rust output, and generates an independently written reference PDF. Reference
and actual output must pass qpdf and strict Ghostscript and render exactly alike
in Poppler, MuPDF, and Quartz. Decoder versions, module hashes, original/output
hashes, and every result are recorded. Pass its `proof.json` to the repair gate
as `--jbig2-proof`; the ordinary original-render failures remain visible.

PDF.js/PDFium, Python, and the viewer executables are test-only dependencies.
The production compressor uses the Rust `hayro-jbig2` decoder.

Run its negative controls with:

```sh
python3 -m unittest discover -s tests/scripts -p test_compatibility_gate.py
```

This verification suite lives inside `tests/`. Its support code is
compiled only for integration tests and the `pdf-compress-verify` example. Its test fixtures
are generated locally; no downloaded or private PDFs are required. The default
reference corpus has six two-page PDFs: RGB, grayscale, JPEG, soft masks,
compressed object streams, and a textured gradient.

The JPEG decoder's unit tests use the small libjpeg-turbo files in
`fixtures/jpeg` (sampling layouts, restart intervals, Adobe RGB, multi-scan,
16-bit tables, progressive, arithmetic) and the `djpeg` decode of each as the
reference. `scripts/make_jpeg_fixtures.sh` regenerates them with `cjpeg`/`djpeg`.

```sh
cargo test -p pdf-compress
cargo test -p pdf-compress --test integration external:: --ignored
cargo run -p pdf-compress --example pdf-compress-verify -- --output target/validation-lossless
cargo run -p pdf-compress --example pdf-compress-verify -- --preset prepress --output target/validation-prepress
cargo run -p pdf-compress --example pdf-compress-verify -- --output target/my-corpus file1.pdf file2.pdf
```

The output directory must be new. The runner retains all PDFs and PPM page rasters,
the actual Ghostscript version/arguments, optimization warnings, failures, and a
TSV report. `--gs`, `--pdftoppm`, and `--qpdf` select executable paths. Missing
tools fail explicit reference runs; normal Rust tests need neither tool.

Every input and compressed page is rendered by Poppler at 96 DPI. Strict lossless
requires **exact RGB equality**; lossy requires both RGB MAE similarity and mean
8x8-window luminance SSIM >= 0.99 (override with `--min-similarity`). Neither metric
alone proves readability, so the Rust tests additionally check extracted text,
page boxes, links, canonical form values, and image samples. Ghostscript validates
the Rust output with `PDFSTOPONERROR` and `PDFSTOPONWARNING`; independent raw-byte
checks validate classic xref offsets, while integration tests check stream lengths
and dangling references. These checks do not certify every PDF specification rule.

Ghostscript's lossless comparison uses `/prepress`, disables all downsampling and
forces Flate for color/gray image encoding. Other presets use matching named
Ghostscript presets. These are comparison policies, not identical implementations.
Rust/Ghostscript size ratios are informational because the requirement prioritizes
appearance. Tests render every page of Ghostscript's reference to ensure it opens;
the quality gate compares Rust output with the original, not with an already changed
Ghostscript output. The suite does not use Chrome or its repair behavior.

The built-in fixtures exercise a limited set of features. For a reproducible
public-corpus run, use the pinned acquisition record in
[`corpus/README.md`](corpus/README.md), the independent verifier in
[`scripts/verify_corpus.py`](scripts/verify_corpus.py), and the frozen
[proof protocol](../docs/pdf-compression-proof-protocol.md). The corpus
is compatibility evidence for its recorded inputs, not a representative sample
of every PDF or a universal 99% guarantee.

The primary public-corpus proof run uses every entry in the frozen manifest.
This keeps the denominator honest: failed downloads stay visible as failed
inputs, while negative stress fixtures are recorded as expected rejections by
the verifier. A nonzero exit caused by a recorded failed acquisition is an
expected outcome of this frozen run; report the clean positive subset
separately rather than replacing the primary run.

```sh
cargo build -p pdf-compress --release
# Only needed when target/pdf-compress-corpus is absent.
python3 tests/scripts/acquire_corpus.py

python3 tests/scripts/verify_corpus.py \
  --manifest tests/corpus/manifest.json \
  --input-dir target/pdf-compress-corpus \
  --output-dir target/proof-lossless-all \
  --compressor target/release/pdf-compress \
  --preset lossless \
  --dpi 150 \
  --max-raster-megapixels 50

python3 tests/scripts/verify_corpus.py \
  --manifest tests/corpus/manifest.json \
  --input-dir target/pdf-compress-corpus \
  --output-dir target/proof-prepress-all \
  --compressor target/release/pdf-compress \
  --preset prepress \
  --dpi 150 \
  --max-raster-megapixels 50
```

For an optional clean positive view, select only downloaded, unencrypted
positive entries into a derived manifest. This is an additional diagnostic
denominator; it does not replace the complete-manifest result above.

```sh
python3 - <<'PY'
import json
from pathlib import Path

source = Path("tests/corpus/manifest.json")
target = Path("target/pdf-compress-positive-manifest.json")
manifest = json.loads(source.read_text(encoding="utf-8"))
entries = [
    entry for entry in manifest["entries"]
    if entry.get("status") == "downloaded"
    and entry.get("evidence_class") == "positive_compatibility"
    and entry.get("expected_valid") is True
    and entry.get("encrypted") is False
]
assert len(entries) >= 100, len(entries)
target.parent.mkdir(parents=True, exist_ok=True)
target.write_text(json.dumps({"entries": entries}, indent=2) + "\n", encoding="utf-8")
print(f"selected {len(entries)} positive entries: {target}")
PY

python3 tests/scripts/verify_corpus.py \
  --manifest target/pdf-compress-positive-manifest.json \
  --input-dir target/pdf-compress-corpus \
  --output-dir target/proof-lossless-positive \
  --compressor target/release/pdf-compress \
  --preset lossless \
  --dpi 150 \
  --max-raster-megapixels 50

python3 tests/scripts/verify_corpus.py \
  --manifest target/pdf-compress-positive-manifest.json \
  --input-dir target/pdf-compress-corpus \
  --output-dir target/proof-prepress-positive \
  --compressor target/release/pdf-compress \
  --preset prepress \
  --dpi 150 \
  --max-raster-megapixels 50
```

The independent proof run requires qpdf, Ghostscript, Poppler's `pdftoppm`,
`pdftotext`, and pypdf, plus MuPDF's `mutool` or PyMuPDF. It runs qpdf and
strict Ghostscript checks on each source and output, then renders every page
with both Poppler and MuPDF. Lossless requires exact RGB equality in both
renderers; prepress requires MAE similarity and local 8x8 luminance SSIM of at
least 0.99 on every page in both renderers. Results, tool versions, hashes,
failures, and retained artifacts are written under each new output directory.

Record Ghostscript reference sizes separately for the complete frozen manifest;
these are informational size comparisons and do not change the appearance
quality denominator:

```sh
python3 tests/scripts/ghostscript_sizes.py \
  --manifest tests/corpus/manifest.json \
  --input-dir target/pdf-compress-corpus \
  --output-dir target/ghostscript-sizes-lossless \
  --preset lossless

python3 tests/scripts/ghostscript_sizes.py \
  --manifest tests/corpus/manifest.json \
  --input-dir target/pdf-compress-corpus \
  --output-dir target/ghostscript-sizes-prepress \
  --preset prepress
```

On macOS, an optional third-engine check can compare the positive subset with
Quartz/CoreGraphics. Compile the test-only renderer, create flattened
compressed copies, and run the Quartz verifier as follows. Quartz is an
additional viewer check; it does not replace the qpdf/Ghostscript/Poppler/MuPDF
proof run.

```sh
# macOS only; requires Swift, NumPy, and Pillow for the verifier.
mkdir -p target/quartz/module-cache
swiftc -O -module-cache-path target/quartz/module-cache tests/scripts/render_quartz.swift \
  -o target/quartz/render_quartz

python3 - <<'PY'
import json
import subprocess
from pathlib import Path

manifest = json.loads(Path("target/pdf-compress-positive-manifest.json").read_text())
root = Path("target/pdf-compress-corpus")
cli = Path("target/release/pdf-compress").resolve()
for preset in ("lossless", "prepress"):
    destination = Path("target") / f"quartz-compressed-{preset}"
    destination.mkdir(parents=True, exist_ok=False)
    for entry in manifest["entries"]:
        source = root / entry["local_path"]
        if not source.is_file():
            continue
        output = destination / source.name
        subprocess.run(
            [str(cli), "--preset", preset, "--force", str(source), str(output)],
            check=False,
        )
PY

python3 tests/scripts/verify_quartz.py \
  --manifest target/pdf-compress-positive-manifest.json \
  --input-dir target/pdf-compress-corpus \
  --compressed-dir target/quartz-compressed-lossless \
  --output-dir target/quartz-lossless \
  --renderer target/quartz/render_quartz \
  --preset lossless \
  --dpi 150

python3 tests/scripts/verify_quartz.py \
  --manifest target/pdf-compress-positive-manifest.json \
  --input-dir target/pdf-compress-corpus \
  --compressed-dir target/quartz-compressed-prepress \
  --output-dir target/quartz-prepress \
  --renderer target/quartz/render_quartz \
  --preset prepress \
  --dpi 150
```

See [recorded synthetic validation results](../docs/pdf-compression-validation.md).
The library's input and stream limits are not a process-wide memory/CPU sandbox:
the underlying PDF parser may allocate while loading object/xref streams before
optimization limits apply. Run hostile input in a resource-limited worker process.
