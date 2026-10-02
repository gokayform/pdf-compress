# Pure Rust PDF compression and Ghostscript comparison plan

## Target and acceptance contract

Build an in-process Rust library and CLI without native PDF/image libraries, FFI
in our compressor code, or runtime subprocesses. The user confirmed pure Rust dependencies are allowed and external
FFI libraries are forbidden. The user clarified the target as “lossless or best
looking compression”: appearance preservation takes priority over matching size.
Ghostscript is a PDF interpreter/rewriter, so reproducing its bytes is not a useful
compatibility definition. A 99% claim requires a named corpus, pinned reference
version/command and visual metric. No universal parity is claimed.

Proposed gates: valid and readable output for every accepted document; unchanged
page count, boxes, text, links and forms; exactly equal raster pixels in lossless
mode; at least 0.99 visual similarity for lossy mode on every evaluated page.
Record both pixel MAE similarity and SSIM, since either alone can hide defects.
Measure Rust/Ghostscript output-byte ratio independently. A size gate
is not imposed: the clarified requirement favors quality. Ratios are reported
separately and never used to justify silently degrading the output.

## Architecture and parallel ownership

1. Core agent (GPT-5.6 Luna, max): `src/lib.rs` and core modules.
   Parse PDFs, validate options, reject unsafe rewrites (encryption/signatures),
   preserve document semantics, recompress eligible streams, prune unreachable
   objects conservatively, deduplicate only objects safe to share, serialize,
   and return original bytes if the result grows only when those original bytes
   pass structural checks. Report applied optimizations.
2. Image agent (GPT-5.6 Luna, max): `src/images.rs` and helpers.
   Lossless adaptive PNG predictors plus Flate by default; explicit lossy RGB/gray JPEG optimization,
   conservative handling of masks, Decode arrays, ICC/CMYK, filters and predictors.
   Downsampling must use effective placement DPI, including reuse and transforms;
   ambiguous placement is a reason to preserve, never guess.
3. CLI/docs agent (GPT-5.6 Luna, max): `src/main.rs` and README.
   Typed presets, overrides, clear errors, safe output handling, usage examples,
   accurate feature limitations and independent reference commands.
4. Coordinator: manifests, this plan, `tests/`, review/integration,
   regression fixtures, external reference harness, dependency audit and QA.

## Shared API contract

`Preset::{Lossless, Screen, Ebook, Printer, Prepress}`; `Options::default()` is
lossless; `Options::for_preset(Preset)` constructs settings. Public option fields:
`preset: Preset`, `jpeg_quality: u8`, `target_dpi: Option<u32>`,
`allow_lossy: bool`, `max_input_bytes: usize`, `max_decoded_stream_bytes: usize`,
`keep_if_larger: bool` (default false). `compress(&[u8], &Options)` returns
`Result<CompressionResult, Error>` with `bytes: Vec<u8>` and `report: Report`.
Report fields: `input_bytes`, `output_bytes`, `streams_recompressed`,
`images_optimized`, `objects_removed`, `objects_deduplicated` (usize),
`used_original: bool`, `warnings: Vec<String>`.
Image boundary: `images::optimize_images(&mut lopdf::Document, &Options,
&mut Report) -> Result<(), Error>`; `Error::InvalidInput(String)`,
`Error::Unsupported(String)`, `Error::LimitExceeded(String)`,
`Error::InvalidOptions(String)`, `Error::Pdf(String)`.

## Correctness suite (`tests/`)

The user explicitly requires general viewer compatibility rather than relying
on Chrome repairs. Rewritten documents use classic xref tables, remove stale
incremental pointers, and receive byte-level offset/length checks. Ghostscript
validates with `PDFSTOPONERROR` and `PDFSTOPONWARNING`; Poppler independently
renders every page. These are interoperability checks, not a certification of
every PDF feature or an empirical guarantee for every viewer.

- Synthetic text/vector PDFs, RGB/gray images, repeated assets, binary streams,
  annotations, AcroForm fields, page boxes, transparency and unusual filters.
- Decode content before/after; verify image pixels in lossless mode and semantic
  dictionaries/references, not merely successful parsing by the same library.
- Malformed/truncated input, invalid settings, decode limits, signed/encrypted
  documents, incompressible input and deterministic repeated runs.
- External reference tests are explicitly invoked; missing tools fail those runs
  rather than silently passing. Render input and output with Poppler, compare all
  pages; run a pinned Ghostscript pdfwrite reference and report size ratios.
- Corpus runner accepts caller-owned PDFs and writes a machine-readable report;
  external tools are confined to the test crate. Synthetic results are not evidence
  of production-wide 99% coverage.

## Delivery stages and follow-on parity work

A. Deliver working lossless core, cautious image optimization, CLI, tests and
   measured reference results. Audit the selected dependency tree for native code.
B. Expand the corpus with Chromium outputs, scans, photos, forms, PDF 1.x/2.0,
   object streams, incremental updates, fonts, transparency and color profiles.
C. Close measured size gaps with broader image placement analysis, JPEG tuning,
   font subsetting with pure Rust, object streams and broader predictor coverage.
D. Add explicit policies for encryption, signatures, PDF/A, PDF/X, tagged PDF,
   complex color conversion, JBIG2/JPX and arbitrary PostScript; unsupported
   transformations must be reported rather than represented as Ghostscript parity.
E. Run pinned corpus benchmarks (time, peak memory, size, raster similarity,
   text/forms/links), review outliers, then determine whether the agreed 99% gate
   has actually been met.

## Reference

Ghostscript explains that pdfwrite reconstructs a document and presets change
image quality/resolution: https://ghostscript.com/blog/optimizing-pdfs.html and
https://ghostscript.readthedocs.io/en/gs10.05.1/VectorDevices.html .
Installed reference at start: Ghostscript 10.06.0. Tests record their actual version.

## Initial delivered state

Stage A is implemented in `pdf-compress` and `tests/`. Three
GPT-5.6 Luna agents at max reasoning handled the parallel implementation, and
the coordinator built the independent suite and completed integration fixes.
All 38 tests (including the external viewer test), strict Clippy, and formatting
checks pass. Six two-page fixtures preserve exact pixels in lossless mode;
prepress's minimum measured SSIM is 0.999887972. See
[the measured results and limitations](pdf-compression-validation.md).
Stages B-E remain corpus-driven expansion and certification work, not hidden
claims about compatibility or coverage already achieved.

The broader corpus results supersede those initial fixture counts; see
[the public proof report](pdf-compression-proof.md).

## Repairing the eleven measured compatibility failures

1. Decode the six affected JBIG2 generic/refinement images with pure Rust,
   retaining exact one-bit samples, `/Decode`, masks, and image placement. Encode
   those samples with Flate. Preflight the permitted segment families and their
   intermediate memory requirements; preserve other JBIG2 features explicitly.
2. Sort the unsorted name tree without changing any key/value association.
   Update affected limits, validate duplicate keys and cycles, and restrict the
   repair to standard tree roots rather than guessing from private dictionaries.
3. Remove empty entries from the affected page annotation list. Preserve the
   actual text annotation, including its intentional lack of a popup. Reject a
   checkbox appearance that has a name where its appearance stream should be.
4. Reject zero-area page geometry and detected recursive Type3 execution with
   specific diagnostics. Preserve legal reversed rectangles and valid nested
   Type3 fonts. Do not invent missing page dimensions or glyph appearance.
5. Retain necessary compatibility rewrites even when larger than their source;
   returning the original would undo the repair. Report that choice to callers.
6. Run generated regressions in `tests/`, then the entire frozen
   136-entry manifest. All 102 previously produced validator-clean outputs must
   remain accepted and preserved. Distinguish repaired outputs from rejected
   malformed inputs. Require qpdf, strict Ghostscript, unchanged semantics, and
   exact every-page Poppler/MuPDF pixels; supplement with Quartz page rendering.

The repair gate keeps original source failures in the ordinary corpus report.
Its narrow annotation equivalence rule ignores only empty annotation entries,
with negative controls proving that changed or deleted real annotations fail.
