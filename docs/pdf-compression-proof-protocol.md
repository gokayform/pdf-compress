# Broader validation protocol

This protocol is recorded before inspecting public-corpus results. It expands the
initial six synthetic fixtures; those fixtures alone were insufficient evidence of
broad compatibility.

## Frozen inputs and outcomes

Use a pinned public-source revision and record each URL, byte count, SHA-256,
category and intended role before compression results are known. Downloaded PDFs
stay in `target/`, outside source control. Retain the complete input manifest and
every outcome: downloads that fail, rejected inputs, timeouts, source warnings,
output warnings, semantic differences and pixel differences. Do not replace failed
documents with easier ones or count untested pages as passes.

The corpus is a reproducible compatibility sample, not a random draw from every
possible PDF. An observed percentage applies only to its stated denominator. It
does not establish a universal guarantee or a statistical population estimate.

## Independent checks

1. Run qpdf `--check` on both source and output. Exit 3 is a warning, not a clean
   pass. Record baseline defects independently from newly introduced defects.
2. Run Ghostscript with `PDFSTOPONERROR` and `PDFSTOPONWARNING` on both files.
3. Render every page of both files with Poppler and MuPDF at 150 DPI. Compare
   source versus output within each renderer; different rendering engines need
   not produce the same antialiasing or font rasterization as each other.
4. Lossless requires exact pixel equality in both engines. High-quality prepress
   requires MAE similarity >=0.99 and local luminance SSIM >=0.99 on every page,
   with unchanged page count and dimensions.
5. Compare extracted text, page boxes, interactive field values and annotations.
   Distinguish baseline extraction failures from new semantic regressions.
6. Record actual tool versions, executable hash, options, elapsed time and output
   sizes. Compare Ghostscript sizes under explicit lossless/prepress settings.
   Returning the original is recorded as an unchanged result, not compression.

## Denominators and fixes

Report all attempted documents first. Also report separately the clean-source
subset (baseline validation and rendering succeed), explicit negative fixtures,
and baseline-invalid or unsupported documents. A failure in a clean-source
document counts against compatibility even if other files pass. Size, appearance,
semantic preservation and acceptance are separate metrics.

Fix concrete failures and add small regression tests. Preserve the first failing
run and rerun the frozen corpus with a newly hashed executable after code changes.
Use subprocess timeouts and bounded concurrency; resource-limit outcomes remain
visible. Keep failure artifacts and machine-readable evidence under `target/`.

## Initial independent finding

Adding qpdf exposed a stale trailer `/Size` after pruning objects. The earlier
Ghostscript/Poppler tests did not flag it. The writer now recomputes its highest
object ID before serialization, and the cross-reference regression test asserts
the trailer count. The repaired synthetic output passes qpdf without warnings.
