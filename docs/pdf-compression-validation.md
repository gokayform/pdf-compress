> Historical synthetic-only results. See [the broader proof report](pdf-compression-proof.md) for the final corrected implementation and public-corpus evidence.

# PDF compressor validation, 2026-10-01

The delivered implementation uses pure Rust PDF, Flate and JPEG dependencies.
Its library and CLI contain no native PDF/image bindings or subprocess calls.
The selected dependency tree contains no codec `-sys`, `cc`, `cmake`, or
`pkg-config` dependencies. Standard platform bindings used by Rust dependencies
are distinct from native PDF/image libraries.

## Automated checks

Result: 38 tests passed, including the explicitly enabled external reference test;
Clippy with warnings denied and formatting checks passed.

```sh
cargo test -p pdf-compress -- --include-ignored
cargo clippy -p pdf-compress --all-targets -- -D warnings
cargo fmt -p pdf-compress -- --check
```

The tests cover structural checks, stream/pixel preservation, text, forms, links,
page geometry, signatures/encryption, incremental revisions, object streams,
malformed inputs, decoded-size limits, deterministic output, image reuse,
UserUnit scaling, annotation appearances, JPEG ColorTransform preservation,
actual JPEG downsampling, CLI output protection, and raster-metric correctness.
The external test requires Ghostscript and Poppler and fails when tools are absent.

## Independent viewer and quality results

Six synthetic PDFs, two pages each, were evaluated with Ghostscript 10.06.0 and
Poppler at 96 DPI. Every Rust output passed Ghostscript `PDFSTOPONERROR` plus
`PDFSTOPONWARNING`. Poppler successfully rendered all originals, Rust outputs,
and Ghostscript reference outputs. Every lossless output had exact RGB pixel
equality on every page. Prepress met both MAE similarity and 8x8 luminance SSIM
>=0.99 on every page; its worst SSIM was 0.999887972 on the textured image.
The lossless sample and high-quality textured page were also visually inspected.

| Fixture | Input bytes | Rust lossless | Ghostscript lossless reference | Rust prepress |
| --- | ---: | ---: | ---: | ---: |
| RGB gradient | 1,106,802 | 4,545 | 17,144 | 4,545 |
| Grayscale | 386,806 | 4,206 | 9,703 | 4,206 |
| Existing JPEG | 70,499 | 45,740 | 49,575 | 45,740 |
| Soft mask | 1,466,970 | 5,691 | 19,710 | 5,691 |
| Object streams | 1,100,640 | 4,545 | 17,144 | 4,545 |
| Textured gradient | 1,106,807 | 746,910 | 758,778 | 303,550 |

The first five fixtures favor lossless predictors even under the prepress policy;
the textured fixture actually exercises JPEG. Ghostscript prepress produces
225,987 bytes for the textured fixture versus Rust's 303,550 bytes. This size
difference is reported rather than hidden or traded for unspecified quality loss.

Raw reports and PDF/raster artifacts are generated under
`target/validation-lossless-complete/` and `target/validation-prepress-complete/`.
Those build artifacts are not source-controlled. Reproduce with:

```sh
cargo run -p pdf-compress --example pdf-compress-verify -- --output target/a-new-lossless-run
cargo run -p pdf-compress --example pdf-compress-verify -- --preset prepress --output target/a-new-prepress-run
```

## Limits of the evidence

These are synthetic interoperability/regression tests, not a representative
production corpus or a proof of compatibility with every PDF viewer. No claim
of universal 99% Ghostscript parity is made. Fonts are preserved rather than
newly subset; arbitrary PostScript, advanced color conversion, and PDF/A or
PDF/X certification are outside the implementation. Signed/encrypted input is
rejected. The underlying PDF parser's allocations precede some stream limits,
so the library is not a process-wide CPU/memory sandbox. A representative local
corpus can be passed to the runner without altering its originals.
