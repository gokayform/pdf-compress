# Measured PDF compression proof — 2026-10-01

**Historical baseline:** the [compatibility repair follow-up](pdf-compression-compatibility-proof.md)
now records eight repaired outputs, three explicit rejections, and no remaining
validator failures among the 110 emitted PDFs. This earlier report and its
failures are retained for comparison.

The final compressor preserved **all 130 pages of 94 successfully compressed,
validator-clean public PDFs exactly** at 150 DPI in Poppler, MuPDF, and Apple's
Quartz page renderer. Text, field values, annotations, page counts, page boxes,
rotation, and UserUnit comparisons also passed. Every one of those outputs passed
qpdf and strict Ghostscript checks. Their combined size fell from 8,485,804 to
6,316,440 bytes: **25.6% smaller**.

This is measured evidence for the stated files. The complete corpus did **not**
pass every gate: it includes broken files, unsupported reference-engine features,
encryption, signatures, and a resource-stress case. Eleven outputs from inputs
that already failed baseline checks still fail one of those checks. Universal
viewer compatibility has therefore **not** been established.

## Corpus and denominators

The [frozen manifest](../tests/corpus/manifest.json) was acquired
from Mozilla PDF.js revision `c33c32aed46637ec6010e9f6c031c72e1c529d31`, supplemented
by public PDF Association, W3C, and research documents. It includes forms, fonts,
JBIG2/images, transparency, encodings, object streams, and intentional stress
fixtures. Its selection is feature-oriented and contains many related fixtures;
it is not a random or representative population sample.

| Measurement | Result, for each of lossless and prepress |
|---|---:|
| Selected manifest entries | 136 |
| Downloaded and attempted | 134 |
| Failed acquisitions, retained in manifest | 2 |
| Sources passing both qpdf and strict Ghostscript | 95 |
| Those sources successfully compressed and fully preserved | 94 PDFs / 130 pages |
| Remaining validator-clean source | 1 signed PDF, explicitly refused |
| Total produced outputs, including baseline-problem inputs | 113 |
| Exact Poppler and MuPDF comparisons | 113 PDFs / 188 pages in each engine |
| Exact Quartz comparisons | 112 PDFs / 185 pages |
| Outputs passing qpdf without warnings | 112 / 113 |
| Outputs passing both qpdf and strict Ghostscript | 102 / 113 |
| New validation or preservation failures on validator-clean compressed sources | 0 |

Quartz could not render the original three-page `boundingBox_invalid` negative
fixture; this is an explicit failed source render, not a pass. Quartz here means
`CGPDFDocument`/`drawPDFPage`, not an automated test of Preview's full UI. Its
page-content rendering does not paint interactive widget appearances; Poppler,
MuPDF, and the semantic checks cover those appearances and field values.

The two acquisition failures are an RFC URL returning 404 and a PDF exceeding
the predeclared 5 MiB download cap. The image-bomb fixture exhausted its 60-second
case budget. These outcomes remain recorded. Signed and encrypted documents are
refused rather than rewritten with invalidated signatures or protection.

Both public-corpus presets produced identical PDF bytes: conservative image
eligibility and size checks chose lossless operations for this sample. This is
not evidence that JPEG degradation was exercised across all those documents.
The separate quality fixtures below exercise that path.

## Actual lossy compression

Six generated two-page fixtures were also checked with qpdf, strict Ghostscript,
Poppler, MuPDF, and Quartz. All six passed, including the textured image whose
JPEG encoding genuinely changes pixels.

| Textured fixture | Result |
|---|---:|
| Original | 1,106,807 bytes |
| Rust lossless | 746,910 bytes |
| Rust prepress | 303,550 bytes |
| Ghostscript prepress reference | 225,987 bytes |
| Minimum Poppler SSIM at 150 DPI | 0.999713261 |
| Minimum MuPDF SSIM at 150 DPI | 0.999838059 |
| Minimum Quartz SSIM at 150 DPI | 0.999961301 |
| Minimum RGB MAE similarity across the three engines | 0.999788698 |

That is **72.6% smaller than the input**, with worst measured page SSIM above
**99.97%**. These are explicitly defined image metrics, not a universal perceptual
percentage. Lossless requires exact RGB equality. Prepress requires both local
8×8-window luminance SSIM and RGB MAE similarity ≥ 0.99 on every page. Text and
form checks supplement the raster metrics.

## Size comparison with Ghostscript

On 92 validator-clean inputs with successful, qpdf-clean Ghostscript reference
outputs, the input total was 5,367,151 bytes:

| Policy | Rust total | Ghostscript total |
|---|---:|---:|
| Lossless reference policy | 4,809,823 | 4,184,131 |
| Prepress reference policy | 4,809,823 | 3,212,334 |

Rust was larger in aggregate on that paired subset. No Ghostscript size-parity
claim is made. Its lossless reference disables downsampling and forces Flate for
color/gray images; font and document reconstruction can still differ. Prepress
uses Ghostscript's named preset. All commands and output hashes are retained.

Among the 94 validator-clean compressed inputs, 87 became smaller and seven
became larger. The larger cases retain the audited full rewrite when the
compressor's conservative byte-level source check cannot certify the original
for its no-growth fallback. The CLI reports that decision. Returning an original
is never counted as a compression improvement.

## Bugs exposed and fixed

1. **Stale trailer `/Size`:** pruning left a count beyond the highest surviving
   object. qpdf detected it although the earlier Ghostscript/Poppler tests passed.
2. **Hybrid cross-reference loading:** auxiliary `/XRefStm` sections in current or
   earlier revisions were missed. Same-revision stream entries now override table
   placeholders, while newer revisions retain precedence over `/Prev`.
3. **Stale compressed object versions:** the parser loaded old object-stream
   members without following the live container/index mapping. Updated form
   values and appearances were lost; initial page SSIM fell as low as 0.9598.
   Active references now select the correct version and do not resurrect freed
   members.
4. **Decimal precision loss:** parsing real numbers into `f32` rounded annotation
   coordinates, colors, and page boxes. The private parser fork stores `f64`;
   the affected corpus files now have exact pixels and unchanged semantic values.
5. **Test fixture validity:** the upstream modern writer omitted its own xref
   entry after allocating object streams. The fixture writer now includes it,
   and external tests validate their sources as well as their outputs.
6. **No-growth fallback:** legal whitespace after `%%EOF` no longer disqualifies
   otherwise verified originals.

The [MIT-licensed lopdf fork](https://github.com/gokayform/lopdf/blob/5eb09b13f57c8fb072297f16d75b5d4309cc7905/PATCHES.md) carries the
parser/writer patches this crate depends on.
Generated regressions cover the parser/writer cases; no downloaded binary fixture
is committed. Production compression uses Rust libraries without native codec or
PDF-engine FFI. Ghostscript, qpdf, Poppler, MuPDF, Python, and Swift are test tools.

## Known baseline compatibility failures

The eleven outputs retaining a failed baseline oracle are:

- `025-bitmap-composite-and-xnor-refine`
- `029-bitmap-composite-or-xor-replace-refine`
- `069-TAMReview`
- `085-annotation-text-without-popup`
- `115-bitmap-refine-customat-tpgron`
- `116-bitmap-refine-customat`
- `117-bitmap-refine-lossless`
- `120-bitmap-refine-refine`
- `121-ContentStreamCycleType3insideType3`
- `126-boundingBox_invalid`
- `127-checkbox-bad-appearance`

Some failures are reference-engine limitations, including Ghostscript's
unimplemented JBIG2 intermediate generic-region handling. Others are existing
structural or appearance defects, such as an unsorted name/number tree or invalid
page box. A failed baseline check is not automatically proof that a PDF violates
the specification. The compressor does not claim to repair arbitrary source
content or make unsupported features work in every viewer.

## Reproduction and evidence

**47 Rust tests passed**, including the ignored external qpdf/Ghostscript/Poppler
test. Package formatting and Clippy with `-D warnings` passed. The full public
proof runners intentionally exit nonzero because all selected negative and
baseline-problem cases remain in the denominator; those runs are not presented
as a green all-corpus test.

Use the [verification README](../tests/README.md) for acquisition,
Cargo tests, both presets, Ghostscript references, and macOS Quartz commands.
The [recorded protocol](pdf-compression-proof-protocol.md) defines the gates and
retention rules. The [compact machine-readable evidence](pdf-compression-proof-results.json)
contains every selected case, both presets, input/output hashes, failures,
structural outcomes, semantic results, renderer metrics, and reference sizes.

Full local artifacts:

- `target/corpus-proof-lossless-complete/`
- `target/corpus-proof-prepress-complete/`
- `target/corpus-quartz-lossless-complete/` and `target/corpus-quartz-prepress-complete/`
- `target/corpus-synthetic-prepress/` and `target/corpus-quartz-synthetic-complete/`
- `target/corpus-gs-lossless/` and `target/corpus-gs-prepress/`
- `target/pdf-compress-final-tests.log` and `target/pdf-compress-final-clippy.log`
- `target/proof-visual/` contains inspected form and texture renders.

Earlier structural triage, failed results, and the interrupted old-runner attempt
remain under `target/corpus-structural-triage/`, `target/corpus-proof-*-initial/`,
`target/corpus-proof-lossless-final/INTERRUPTED.json`, and
`target/corpus-proof-*-verified/`. Test-runner fixes included separating rendering
DPI from compression DPI, normalizing encoding-only stream metadata and pypdf's
synthetic `_States_` property, checking actual page sequences/counts, and enforcing
an absolute per-document deadline. Failure records were not silently discarded.

Toolchain: Ghostscript 10.06.0, qpdf 12.4.2, Poppler 25.11.0, MuPDF 1.28.5,
Quartz on macOS 26.6.2, pypdf 6.10.0. Every page was compared within its own
renderer; different engines need not agree on antialiasing.

- Manifest SHA-256: `a90c2c8d84049615724f44c143e8963066c65f3b719c07ced1b7cc6a7f732cfd`
- Final executable SHA-256: `33ab2dae213ed090ee5921f85c2a300e370713dfe02c151855bdd2a529045e17`
- Final verifier SHA-256: `766610a08102cc101c22a390e2054d68539a873d62f00bd8dcfa4c407ff5b1f3`

Quartz comparisons refer to the exact final output PDF hashes, verified against
both final corpus runs. The renderer and each compressor/verifier are copied and
hashed into their run directories so rebuilding during work cannot mix versions.
