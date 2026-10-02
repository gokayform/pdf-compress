# pdf-compress

`pdf-compress` is an in-process PDF compressor written in Rust. It does not
invoke Ghostscript, Poppler, a shell, or a native image/PDF library at runtime.
The default mode is strict lossless and is the right starting point when a
document must retain its decoded visual content.

## Library

```toml
[dependencies]
pdf-compress = { git = "https://github.com/gokayform/pdf-compress.git", rev = "<commit>" }
```

```rust
let result = pdf_compress::compress(&input, &pdf_compress::Options::default())?;
std::fs::write("out.pdf", &result.bytes)?;
```

`compress` never panics: every failure, including malformed or hostile input,
is returned as a `pdf_compress::Error`, so callers that treat compression as
optional can fall back to the original bytes.

## Command line

Build or run the binary from the repository root:

```text
cargo run -p pdf-compress -- input.pdf output.pdf
cargo run -p pdf-compress -- --preset ebook --jpeg-quality 72 --dpi 150 input.pdf ebook.pdf
cargo run -p pdf-compress -- --preset prepress input.pdf press-ready.pdf
```

The command takes an input path and an output path. `--preset lossless` is the
default. The public preset defaults are:

| Preset | JPEG quality | Target DPI | Lossy image transforms |
| --- | ---: | ---: | --- |
| `lossless` | 100 | none | disabled |
| `screen` | 60 | 72 | enabled |
| `ebook` | 75 | 150 | enabled |
| `printer` | 85 | 300 | enabled |
| `prepress` | 95 | 300 | enabled |

Lossless mode may rewrite ordinary 8-bit DeviceRGB and DeviceGray image
samples with PDF PNG predictor 15 plus Flate. Predictor decoding restores the
same samples byte-for-byte; it is not JPEG compression. The implementation also
applies conservative lossless stream and object rewrites.

The lossy presets may JPEG-compress and downsample eligible images. They are
quality and size policies, not a promise that every image will be changed.
`--jpeg-quality` accepts 1 through 100 and `--dpi` accepts a positive integer;
both override a lossy preset and are rejected with `lossless`. The
`--max-input-bytes` and `--max-decoded-stream-bytes` limits accept positive byte
counts. Their defaults are 256 MiB and 128 MiB. `--keep-if-larger` permits the
library to return a valid rewritten PDF even when it is larger than the input;
without it, a structurally sound input keeps its original bytes when the
rewritten serialization grows, unless a compatibility repair must be retained.
Necessary repairs can increase file size; the CLI reports that decision.

An existing output is refused. The compressor completes parsing and
compression before writing, and `--force` writes a temporary file in the
destination directory and atomically replaces the destination after success.
This makes an explicit in-place operation (`input.pdf input.pdf --force`) safe
if parsing, decoding, or compression fails. Without `--force`, the destination
is opened with `create_new`, so a concurrent writer cannot be silently
overwritten.

Run `pdf-compress --help` for the complete option list.

## Library API

The core API is intentionally small:

```rust,ignore
use pdf_compress::{compress, Options, Preset};

let options = Options::for_preset(Preset::Ebook);
let result = compress(&input_pdf, &options)?;
std::fs::write("output.pdf", result.bytes)?;
println!("{} bytes → {} bytes", result.report.input_bytes, result.report.output_bytes);
# Ok::<(), Box<dyn std::error::Error>>(())
```

`Options::default()` selects `Preset::Lossless`. Public limits cover the input
and decoded stream sizes. `CompressionResult::report` records stream and image
optimizations, removed or deduplicated objects, `compatibility_repairs`, whether
the original bytes were used, and warnings that need caller attention.

## Current coverage and boundaries

The lossless image predictor currently covers 8-bit DeviceRGB and DeviceGray
image XObjects whose samples can be decoded conservatively from raw or simple
Flate streams. It writes PNG predictor 15 rows with Flate and preserves the
decoded sample bytes, including when a mask or `/Decode` array changes their
interpretation. Those dictionary entries remain intact. Existing `/DecodeParms`,
non-8-bit images, other color spaces, unsupported filter chains, and ambiguous
metadata are excluded from predictor optimization. Generic lossless stream
rewrites likewise keep unknown filters and their parameters intact.

A separate compatibility pass losslessly rewrites supported JBIG2 images that
use intermediate generic regions to one-bit Flate streams, using `hayro-jbig2`
without native codec FFI. It checks segment references and bounds intermediate
bitmaps before decoding. Other JBIG2 families and already-compatible streams
remain unchanged. Image dimensions, masks, `/Decode`, and placement are retained.

The pass also repairs standard name/number tree ordering and identical duplicate
pairs, and removes null annotation placeholders without flattening annotations.
Conflicting duplicate keys, zero-area page boxes, detected recursive Type3
execution, and malformed widget appearance states are rejected. Type3 checking
is a conservative scan of supported content encodings, not a complete PDF
interpreter or a proof that every possible execution path terminates.

Lossy image work is deliberately selective. It can JPEG-compress and
downsample unmasked 8-bit RGB or grayscale images when every page/form
placement can be resolved. Images discovered through annotation appearances
(`/AP`), tiling patterns, Type3 glyph programs, soft-mask or group appearances,
or another placement graph outside the supported page/form walk are treated as
ambiguous and kept at their original resolution. Masks, color profiles,
CMYK/indexed data, unusual filters, predictors, unsupported decode parameters,
and invalid transforms are also preserved or reported. Images that are already
JPEG (`DCTDecode`) are decoded only by the crate's own panic-free baseline
decoder (sequential Huffman, 8-bit, gray or YCbCr/RGB, honouring
`/ColorTransform` and the Adobe marker), then downsampled and re-encoded like
other images and kept only when the result is smaller. Progressive,
arithmetic-coded, lossless, hierarchical, 12-bit, and CMYK/YCCK JPEGs, as well
as malformed or truncated JPEG data, are preserved with a warning. Shared images are not
downsampled when the usage analysis cannot establish a safe size; uncommon
resource indirection should still be checked with the reference harness.

The output is intended to remain a normal PDF that general viewers can open.
The reference harness checks structural invariants and uses Poppler and
Ghostscript with strict stop-on-error settings, but those checks do not amount
to an empirical guarantee for every viewer or every PDF feature. Encrypted and
signed documents are rejected because a complete rewrite would invalidate
their security model. The compressor does not claim support for every PDF
feature, including arbitrary PostScript, all image codecs, tagged-PDF/PDF-A/
PDF-X policy checks, or every incremental-update arrangement. Callers should
inspect warnings and validate important output documents in their own workflow.

The input and decoded-stream limits are compressor limits, not a process-wide
memory or CPU sandbox. The underlying PDF parser may allocate objects and
object/xref streams while loading the input before those optimization limits
apply. Run hostile or untrusted input in a resource-limited worker process.

Ghostscript is a useful independent reference for experiments, but its
`pdfwrite` output is a reconstruction with its own defaults. This project does
not claim universal byte, size, or visual equivalence with Ghostscript and does
not claim a production-wide 99% parity result. Such a result would require a
pinned Ghostscript version and command, a named corpus, raster and semantic
checks, and an agreed size metric.

## Reference harness

The [verification suite](tests/README.md) in `tests/` contains synthetic
correctness checks and the reference harness. Run the Rust tests with:

```text
cargo test -p pdf-compress
```

Run the reference harness with a new output directory:

```text
cargo run -p pdf-compress --example pdf-compress-verify -- --output NEW_DIR [PDF...]
cargo run -p pdf-compress --example pdf-compress-verify -- --preset prepress --output NEW_DIR [PDF...]
cargo test -p pdf-compress --test integration external:: --ignored
```

When no PDFs are supplied, the harness generates six synthetic PDFs covering
RGB, grayscale, JPEG, soft-mask, object-stream, and textured-image cases. Every original,
compressed, and Ghostscript reference page is rendered by Poppler at 96 DPI.
The Rust output is checked by Ghostscript with `PDFSTOPONERROR` and
`PDFSTOPONWARNING`, and the raster gate checks every page: lossless requires
exact RGB equality; lossy requires both MAE similarity and local SSIM at the
configured threshold. The output directory must not already exist. Poppler and
Ghostscript are required for these explicit reference runs; the library and
CLI never launch those executables.

The harness records the Ghostscript version, executable paths, arguments, warnings, failures,
PDFs, page rasters, and a TSV report. Ghostscript size ratios are
informational. The synthetic corpus and these checks do not certify every PDF
specification rule, every viewer, or a universal 99% result.

Measured compatibility and quality results, including all exclusions, are in the
[public-corpus proof report](docs/pdf-compression-proof.md).
The [compatibility repair follow-up](docs/pdf-compression-compatibility-proof.md)
records the eleven-case repair and rejection results.
