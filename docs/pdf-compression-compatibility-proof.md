# Compatibility repair proof — 2026-10-01

The eleven previously emitted PDFs that failed a validator are now handled as
**eight repaired outputs and three explicit rejections**. Both lossless and
prepress pass the dedicated repair gate. Across the complete frozen corpus,
**all 110 emitted PDFs, containing 183 pages, pass qpdf and strict Ghostscript**.
All 102 previously validator-clean outputs remain **byte-for-byte identical**
to the earlier outputs; their raster and semantic checks also pass.

This follows the [earlier proof](pdf-compression-proof.md), whose original
failures and artifacts are retained. The complete corpus is still not an
all-green run: missing downloads, unsupported protected documents, malformed
inputs, a timed-out stress case, and broken source-renderer behavior remain
visible. This is measured compatibility evidence, not certification of every
PDF feature or viewer.

## What changed

| Previous failure | Result |
|---|---|
| 025, 029, 115, 116, 117, 120: unsupported JBIG2 intermediate generic regions | Decode with pure Rust, preserve one-bit image samples, encode with Flate. All six outputs validate and match the independent image reference exactly. |
| 069: TAMReview tree warning | Remove one identical duplicate page-label pair. The unique mapping remains `0 → { /S /D, /St 1 }`; the named destination remains unchanged. |
| 085: text annotation without popup | Remove the null entry from `/Annots [4 0 R null]`. Preserve the real annotation and its intentional lack of `/Popup`. |
| 121: recursive Type3 execution | Reject with a specific recursive-glyph diagnostic; emit no PDF. |
| 126: zero-sized page box | Reject with a specific `/MediaBox` diagnostic; emit no PDF. |
| 127: malformed checkbox appearance | Reject because `/AP /N /Off` contains a name instead of an appearance stream; emit no PDF. |

The last three are refusals, not repaired documents. Inventing page dimensions,
glyph execution behavior, or a missing checkbox appearance would introduce a
new interpretation of the document.

The implementation is in `src/compatibility.rs`, `annotations.rs`,
and `jbig2.rs`. `Report.compatibility_repairs` counts changes that must survive
the no-growth fallback. The CLI reports these repairs; a larger repaired output
is retained instead of returning the incompatible original.

JBIG2 conversion is limited to supported streams containing intermediate
generic regions. Already-compatible streams are preserved. Preflight checks
bound intermediate bitmap requirements and reject missing, duplicate, forward,
or unsuitable segment references before decoding. Masks, `/Decode`, dimensions,
and placement remain intact. Production uses `hayro-jbig2` 0.3.0 with no native
codec or PDF-engine FFI; reference tools are test-only dependencies.

## Why the image proof needs an independent reference

The original six JBIG2 PDFs do not have consistent rendering across the tested
viewers. MuPDF reports unimplemented intermediate generic-region decoding and
produces black pages while returning exit code zero. Ghostscript rejects the
same feature. On case 115, Poppler and Quartz also disagree with the independent
decoded bitmap. Preserving those failed original renders would preserve the
decoder defect, not establish sample preservation.

The ordinary original-versus-output comparisons remain recorded as failures:

| Original-render comparison, each preset | Exact PDFs / pages |
|---|---:|
| Poppler | 109 / 182 |
| MuPDF | 104 / 177 |
| Quartz | 109 / 182 |

The additional JBIG2 proof does the following:

1. Decode each original image through PDFium's WASM decoder in PDF.js 5.6.205.
2. Compare every meaningful packed sample with the Rust output, allowing only
   padding outside the image width to be cleared. Image attributes also match.
3. Use pypdf to write a reference PDF replacing only the original image encoding
   with the independently decoded samples.
4. Require qpdf and strict Ghostscript to accept both reference and actual output.
5. Compare every page of actual output and reference at 150 DPI in Poppler,
   MuPDF, and Quartz. Require exact pixels, not a relaxed similarity threshold.

All six pass every step in both presets. Each decoded image is 399 × 400 pixels
and 20,000 packed bytes, with sample SHA-256:

`68729cd515c668992c596df83a7437956a7f5a9df0d603e26b43aa3fc4a88f28`

The other 104 emitted PDFs match their original renderings exactly in all three
engines. All 110 outputs preserve the checked text, fields, real annotations,
page boxes, rotations, and UserUnit values. Annotation comparison ignores only
empty top-level annotation placeholders and absent-equivalent `/Annots null`;
negative controls ensure that changing or deleting a real annotation fails.
Quartz is a page-content renderer here, not a full Preview UI test; Poppler,
MuPDF, and semantic checks cover widget appearances and field values.

## Regression checks and scope

**67 Rust tests passed**, including the explicitly invoked viewer and JBIG2
corpus tests. Eight Python proof controls passed. Formatting, diff checks, and
Clippy with `-D warnings` pass. The six generated prepress
fixtures also pass; the textured JPEG fixture retains minimum measured SSIM
0.999887972 at the Rust runner's 96 DPI.

The complete 136-entry corpus was rerun in both presets using the same installed
qpdf 12.4.2, Ghostscript 10.06.0, Poppler 25.11.0, MuPDF 1.28.5, and Quartz tools
as the earlier proof. Both presets produce identical public-corpus output bytes.
Their 110 accepted inputs total 9,859,531 bytes; output totals 7,692,486 bytes.
The original 94 validator-clean compressed inputs retain the previous 25.6%
combined size reduction. These measurements do not establish Ghostscript size
parity.

The first repair iteration exposed three legal indirect name-tree keys being
rejected and an unnecessary conversion of a previously compatible JBIG2 image.
Both regressions were fixed before this final run. Review also exposed hayro's
fallback for missing refinement references; explicit preflight rejection and
generated regressions now cover that case. Earlier failed iterations remain in
the artifact directory.

Page-box validation preserves legal reversed rectangles and inherited null
entries. Type3 cycle detection is a bounded scan of supported content encodings,
not a complete interpreter or a proof of termination for all possible PDFs.
The compressor's limits are not a process-wide memory or CPU sandbox.

## Reproduction and evidence

Use the [verification suite instructions](../tests/README.md)
for both complete corpus runs, Quartz, and the independent JBIG2 reference.
The full corpus verifier intentionally exits nonzero because it retains the
source failures and exclusions. The separate repair gate passes:

```sh
python3 tests/scripts/check_compatibility_repairs.py \
  --before target/corpus-proof-lossless-complete/results.jsonl \
  --after target/compatibility-repair-proof/lossless-verified/results.jsonl \
  --expected-rejections tests/corpus/compatibility-rejections.json \
  --jbig2-proof target/compatibility-repair-proof/lossless-jbig2-reference/proof.json \
  --output target/compatibility-repair-proof/lossless-gate.json
```

Repeat with the corresponding prepress paths. The gate requires unchanged corpus
membership and source hashes, validates each specific rejection, and preserves
all previously successful outputs. Independent image-reference evidence must
match the exact input/output hashes and pass all three exact raster checks.

The [machine-readable results](pdf-compression-compatibility-results.json)
retain all 136 outcomes in both presets, final gates, Quartz outcomes, independent
image proof, and provenance. Full artifacts are under
`target/compatibility-repair-proof/`; `lossless-verified`, `prepress-verified`,
their `*-quartz` and `*-jbig2-reference` directories are the final runs.

Tested release executable SHA-256:
`5ba1b12ce3ecfa2d3a699634c5247336e15789b34dff20112450f006226a3492`.
