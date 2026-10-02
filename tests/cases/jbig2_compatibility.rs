//! Focused regression coverage for the pure-Rust JBIG2 compatibility repair.
//!
//! The fixtures are part of the downloaded proof corpus and are intentionally
//! kept as an ignored integration test: ordinary tests must not
//! require the optional corpus checkout, while the proof runner can execute
//! this test explicitly with `--ignored`.

use std::path::{Path, PathBuf};

use lopdf::Document;
use pdf_compress::{jbig2, Options, Report};

const FIXTURES: &[&str] = &[
    "025-bitmap-composite-and-xnor-refine.pdf",
    "029-bitmap-composite-or-xor-replace-refine.pdf",
    "115-bitmap-refine-customat-tpgron.pdf",
    "116-bitmap-refine-customat.pdf",
    "117-bitmap-refine-lossless.pdf",
    "120-bitmap-refine-refine.pdf",
];

fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target/pdf-compress-corpus/positive")
        .join(name)
}

fn image_stream(document: &Document) -> (&lopdf::Stream, u32, u32) {
    document
        .objects
        .values()
        .find_map(|object| {
            let stream = object.as_stream().ok()?;
            let filter = stream.dict.get(b"Filter").ok()?.as_name().ok()?;
            if filter != b"JBIG2Decode" {
                return None;
            }
            let width = stream.dict.get(b"Width").ok()?.as_i64().ok()?;
            let height = stream.dict.get(b"Height").ok()?.as_i64().ok()?;
            Some((
                stream,
                u32::try_from(width).ok()?,
                u32::try_from(height).ok()?,
            ))
        })
        .expect("fixture should contain a direct JBIG2 image stream")
}

#[test]
#[ignore = "requires the downloaded JBIG2 proof corpus"]
fn known_intermediate_generic_regions_decode_exactly() {
    for name in FIXTURES {
        let path = fixture_path(name);
        assert!(path.is_file(), "missing corpus fixture: {}", path.display());
        let document = Document::load(&path).expect("fixture PDF should parse");
        let (stream, width, height) = image_stream(&document);
        let image = jbig2::decode_embedded(&stream.content, None, width, height, 128 * 1024 * 1024)
            .unwrap_or_else(|error| panic!("{name}: JBIG2 decode failed: {error}"));
        assert_eq!(image.width(), 399, "{name}");
        assert_eq!(image.height(), 400, "{name}");
        assert_eq!(image.samples().len(), 20_000, "{name}");
        assert_eq!(
            image
                .samples()
                .iter()
                .map(|byte| byte.count_ones())
                .sum::<u32>(),
            10_950,
            "{name}"
        );
    }
}

#[test]
#[ignore = "requires the downloaded JBIG2 proof corpus"]
fn normalization_retains_pdf_image_semantics() {
    let path = fixture_path(FIXTURES[0]);
    assert!(path.is_file(), "missing corpus fixture: {}", path.display());
    let mut document = Document::load(&path).expect("fixture PDF should parse");
    let options = Options::default();
    let mut report = Report::default();
    jbig2::normalize_images(&mut document, &options, &mut report)
        .expect("normalization should not fail the document");
    assert_eq!(report.compatibility_repairs, 1);

    let (stream, width, height) = image_stream_after_repair(&document);
    assert_eq!((width, height), (399, 400));
    assert_eq!(
        stream.dict.get(b"Filter").unwrap().as_name().unwrap(),
        b"FlateDecode"
    );
    assert!(stream.dict.get(b"DecodeParms").is_err());
    let mut decoded = flate2::read::ZlibDecoder::new(stream.content.as_slice());
    let mut samples = Vec::new();
    std::io::Read::read_to_end(&mut decoded, &mut samples).unwrap();
    assert_eq!(samples.len(), 20_000);
}

#[test]
#[ignore = "requires the downloaded JBIG2 proof corpus"]
fn normalization_preserves_immediate_generic_fixture() {
    let path = fixture_path("118-bitmap-refine-page-subrect.pdf");
    assert!(path.is_file(), "missing corpus fixture: {}", path.display());
    let original = Document::load(&path).expect("fixture PDF should parse");
    let (original_stream, _, _) = image_stream(&original);
    let original_content = original_stream.content.clone();

    let mut document = Document::load(&path).expect("fixture PDF should parse");
    let mut report = Report::default();
    jbig2::normalize_images(&mut document, &Options::default(), &mut report)
        .expect("normalization should not fail the document");

    let (stream, _, _) = image_stream(&document);
    assert_eq!(report.compatibility_repairs, 0);
    assert_eq!(
        stream.dict.get(b"Filter").unwrap().as_name().unwrap(),
        b"JBIG2Decode"
    );
    assert_eq!(stream.content, original_content);
}

fn image_stream_after_repair(document: &Document) -> (&lopdf::Stream, u32, u32) {
    document
        .objects
        .values()
        .find_map(|object| {
            let stream = object.as_stream().ok()?;
            let filter = stream.dict.get(b"Filter").ok()?.as_name().ok()?;
            if filter != b"FlateDecode"
                || stream.dict.get(b"Subtype").ok()?.as_name().ok()? != b"Image"
            {
                return None;
            }
            let width = stream.dict.get(b"Width").ok()?.as_i64().ok()?;
            let height = stream.dict.get(b"Height").ok()?.as_i64().ok()?;
            Some((
                stream,
                u32::try_from(width).ok()?,
                u32::try_from(height).ok()?,
            ))
        })
        .expect("normalization should replace the fixture image stream")
}

#[test]
fn malformed_or_oversized_jbig2_is_rejected_before_decode() {
    let error = jbig2::decode_embedded(&[], None, 399, 400, 128 * 1024 * 1024)
        .expect_err("empty JBIG2 data must be rejected");
    assert!(matches!(error, jbig2::Jbig2Error::MalformedStructure));

    let mut wrong_reference = page_segment(1, 1);
    wrong_reference.extend(refinement_segment(1, 0));
    let error = jbig2::decode_embedded(&wrong_reference, None, 1, 1, 128 * 1024 * 1024)
        .expect_err("a refinement reference to page information must be rejected");
    assert!(matches!(error, jbig2::Jbig2Error::UnsupportedStructure));

    let mut missing_reference = page_segment(1, 1);
    missing_reference.extend(refinement_segment(8, 7));
    let error = jbig2::decode_embedded(&missing_reference, None, 1, 1, 128 * 1024 * 1024)
        .expect_err("a missing refinement reference must be rejected");
    assert!(matches!(error, jbig2::Jbig2Error::MalformedStructure));

    let error = jbig2::decode_embedded(&[], None, 399, 400, 1)
        .expect_err("packed output over the caller limit must be rejected");
    assert!(matches!(error, jbig2::Jbig2Error::OutputTooLarge { .. }));

    let mut oversized = page_segment(1, 1);
    oversized.extend(region_segment(65_535, 65_535));
    let error = jbig2::decode_embedded(&oversized, None, 1, 1, 128 * 1024 * 1024)
        .expect_err("an oversized retained region must be rejected during preflight");
    assert!(matches!(
        error,
        jbig2::Jbig2Error::WorkingSetTooLarge { .. }
    ));

    let error = jbig2::decode_embedded(&[0, 0, 0, 0, 0x30], None, 1, 1, 128 * 1024 * 1024)
        .expect_err("a truncated segment header must be rejected");
    assert!(matches!(error, jbig2::Jbig2Error::MalformedStructure));
}

fn page_segment(width: u32, height: u32) -> Vec<u8> {
    let mut payload = Vec::with_capacity(19);
    payload.extend(width.to_be_bytes());
    payload.extend(height.to_be_bytes());
    payload.extend(0_u32.to_be_bytes());
    payload.extend(0_u32.to_be_bytes());
    payload.push(1);
    payload.extend(0_u16.to_be_bytes());
    segment(0, 0x30, 1, payload)
}

fn region_segment(width: u32, height: u32) -> Vec<u8> {
    let mut payload = Vec::with_capacity(17);
    payload.extend(width.to_be_bytes());
    payload.extend(height.to_be_bytes());
    payload.extend(0_u32.to_be_bytes());
    payload.extend(0_u32.to_be_bytes());
    payload.push(0);
    segment(1, 0x24, 1, payload)
}

fn refinement_segment(number: u32, reference: u8) -> Vec<u8> {
    let mut payload = Vec::with_capacity(18);
    payload.extend(1_u32.to_be_bytes());
    payload.extend(1_u32.to_be_bytes());
    payload.extend(0_u32.to_be_bytes());
    payload.extend(0_u32.to_be_bytes());
    payload.push(0);
    payload.push(0);

    let mut bytes = Vec::with_capacity(18 + payload.len());
    bytes.extend(number.to_be_bytes());
    bytes.push(0x2A);
    bytes.push(0x20);
    bytes.push(reference);
    bytes.push(1);
    bytes.extend((payload.len() as u32).to_be_bytes());
    bytes.extend(payload);
    bytes
}

fn segment(number: u32, flags: u8, page: u8, payload: Vec<u8>) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(15 + payload.len());
    bytes.extend(number.to_be_bytes());
    bytes.push(flags);
    bytes.push(0);
    bytes.push(page);
    bytes.extend((payload.len() as u32).to_be_bytes());
    bytes.extend(payload);
    bytes
}
