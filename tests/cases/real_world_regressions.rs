//! Standards-sensitive serialized forms that are easy for a byte scanner to
//! misidentify.  These stay independent of the compressor's private helpers:
//! success means the public API accepts the input and emits a parseable PDF.

use crate::support::{fixture, save};
use lopdf::{dictionary, Document, Object};
use pdf_compress::{compress, Options};
use std::io::Write;

#[test]
fn indirect_stream_length_and_nested_dictionary_markers_survive() {
    let mut document = Document::load_mem(&fixture()).unwrap();
    let page = *document.get_pages().get(&1).unwrap();
    let content_id = document.get_page_contents(page)[0];
    let content = document
        .get_object(content_id)
        .unwrap()
        .as_stream()
        .unwrap()
        .content
        .clone();
    let length_id = document.add_object(Object::Integer(content.len() as i64));
    let stream = document
        .get_object_mut(content_id)
        .unwrap()
        .as_stream_mut()
        .unwrap();
    stream.dict.set("Length", Object::Reference(length_id));
    stream.dict.set(
        "DecodeParms",
        dictionary! {
            "Marker" => Object::string_literal("stream >> endstream"),
            "Nested" => dictionary! { "Value" => Object::string_literal("<< >>") },
        },
    );

    let input = save(document);
    let result = compress(
        &input,
        &Options {
            keep_if_larger: true,
            ..Options::default()
        },
    )
    .unwrap();
    let output = Document::load_mem(&result.bytes).unwrap();
    assert_eq!(output.get_pages().len(), 2);
    let output_content = output.get_page_contents(*output.get_pages().get(&1).unwrap())[0];
    let output_stream = output
        .get_object(output_content)
        .unwrap()
        .as_stream()
        .unwrap();
    assert_eq!(output_stream.content, content);
    assert!(output_stream.dict.get(b"DecodeParms").is_ok());
}

#[test]
fn carriage_return_after_stream_keyword_is_accepted() {
    let input = fixture();
    let marker = b"stream\n";
    let position = input
        .windows(marker.len())
        .position(|window| window == marker)
        .unwrap();
    let mut with_cr = input;
    // Keep the byte count unchanged so all original xref offsets remain valid.
    with_cr[position + marker.len() - 1] = b'\r';

    let result = compress(
        &with_cr,
        &Options {
            keep_if_larger: true,
            ..Options::default()
        },
    )
    .unwrap();
    assert_eq!(
        Document::load_mem(&result.bytes).unwrap().get_pages().len(),
        2
    );
}

#[test]
fn compressed_xref_input_is_rewritten_to_classic_table() {
    let (_, input) = crate::support::corpus()
        .into_iter()
        .find(|(name, _)| *name == "object-streams")
        .unwrap();
    let result = compress(
        &input,
        &Options {
            keep_if_larger: true,
            ..Options::default()
        },
    )
    .unwrap();
    assert!(result.bytes.windows(4).any(|window| window == b"xref"));
    assert_eq!(
        Document::load_mem(&result.bytes).unwrap().get_pages().len(),
        2
    );
}

fn xref_entry(offset: usize) -> String {
    format!("{offset:010} 00000 n \n")
}

fn hybrid_incremental_fixture(standalone: bool) -> Vec<u8> {
    fn object(bytes: &mut Vec<u8>, id: u32, body: &[u8]) -> usize {
        let offset = bytes.len();
        bytes.extend_from_slice(format!("{id} 0 obj\n").as_bytes());
        bytes.extend_from_slice(body);
        bytes.extend_from_slice(b"\nendobj\n");
        offset
    }

    let mut bytes = b"%PDF-1.5\n".to_vec();
    let catalog = b"<< /Type /Catalog /Pages 2 0 R /StructTreeRoot 9 0 R >>";
    let pages = b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>";
    let page = b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Contents 4 0 R >>";
    let content = b"<< /Length 3 >>\nstream\nq Q\nendstream";
    let base_offsets = [
        object(&mut bytes, 1, catalog),
        object(&mut bytes, 2, pages),
        object(&mut bytes, 3, page),
        object(&mut bytes, 4, content),
    ];

    let base_xref = bytes.len();
    bytes.extend_from_slice(b"xref\n0 5\n");
    bytes.extend_from_slice(b"0000000000 65535 f \n");
    for offset in base_offsets {
        bytes.extend_from_slice(xref_entry(offset).as_bytes());
    }
    bytes.extend_from_slice(b"trailer\n<< /Size 5 /Root 1 0 R >>\nstartxref\n");
    bytes.extend_from_slice(base_xref.to_string().as_bytes());
    bytes.extend_from_slice(b"\n%%EOF\n");

    let mut object_stream_plain = b"9 0 ".to_vec();
    let first = object_stream_plain.len();
    object_stream_plain.extend_from_slice(b"<< /Type /StructTreeRoot /K [] >>");
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
    encoder.write_all(&object_stream_plain).unwrap();
    let object_stream_data = encoder.finish().unwrap();
    let object_stream_body = format!(
        "<< /Type /ObjStm /N 1 /First {first} /Filter /FlateDecode /Length {} >>\nstream\n",
        object_stream_data.len()
    );
    let object_stream_offset = bytes.len();
    bytes.extend_from_slice(format!("5 0 obj\n{object_stream_body}").as_bytes());
    bytes.extend_from_slice(&object_stream_data);
    bytes.extend_from_slice(b"\nendstream\nendobj\n");

    let xref_stream_offset = bytes.len();
    let xref_stream_data = [2_u8, 0, 0, 0, 5, 0, 0];
    let xref_stream_body = format!(
        "<< /Type /XRef /W [1 4 2] /Size 10 /Index [9 1] /Length {} >>\nstream\n",
        xref_stream_data.len()
    );
    bytes.extend_from_slice(format!("6 0 obj\n{xref_stream_body}").as_bytes());
    bytes.extend_from_slice(&xref_stream_data);
    bytes.extend_from_slice(b"\nendstream\nendobj\n");

    let current_xref = bytes.len();
    if standalone {
        bytes.extend_from_slice(b"xref\n0 7\n0000000000 65535 f \n");
        for offset in base_offsets {
            bytes.extend_from_slice(xref_entry(offset).as_bytes());
        }
        bytes.extend_from_slice(xref_entry(object_stream_offset).as_bytes());
        bytes.extend_from_slice(xref_entry(xref_stream_offset).as_bytes());
        // The classic table carries a free placeholder for the object whose
        // live entry is supplied by the same revision's auxiliary stream.
        bytes.extend_from_slice(b"9 1\n0000000000 00000 f \n");
    } else {
        bytes.extend_from_slice(b"xref\n0 1\n0000000000 65535 f \n5 2\n");
        bytes.extend_from_slice(xref_entry(object_stream_offset).as_bytes());
        bytes.extend_from_slice(xref_entry(xref_stream_offset).as_bytes());
        bytes.extend_from_slice(b"9 1\n0000000000 00000 f \n");
    }
    let previous = if standalone {
        String::new()
    } else {
        format!(" /Prev {base_xref}")
    };
    bytes.extend_from_slice(
        format!(
            "trailer\n<< /Size 10 /Root 1 0 R{previous} /XRefStm {xref_stream_offset} >>\nstartxref\n{current_xref}\n%%EOF"
        )
        .as_bytes(),
    );
    bytes
}

fn older_hybrid_fixture() -> Vec<u8> {
    let mut bytes = hybrid_incremental_fixture(false);
    let previous_xref = bytes
        .windows(b"startxref\n".len())
        .rposition(|window| window == b"startxref\n")
        .and_then(|position| {
            let start = position + b"startxref\n".len();
            let end = bytes[start..].iter().position(|byte| *byte == b'\n')? + start;
            std::str::from_utf8(&bytes[start..end])
                .ok()?
                .parse::<usize>()
                .ok()
        })
        .expect("previous xref offset");
    bytes.extend_from_slice(b"\n7 0 obj\n<< /Producer (latest) >>\nendobj\n");
    let object_offset = bytes
        .windows(b"7 0 obj\n".len())
        .rposition(|window| window == b"7 0 obj\n")
        .expect("latest object offset");
    let current_xref = bytes.len();
    bytes.extend_from_slice(b"xref\n0 1\n0000000000 65535 f \n7 1\n");
    bytes.extend_from_slice(xref_entry(object_offset).as_bytes());
    bytes.extend_from_slice(
        format!(
            "trailer\n<< /Size 10 /Root 1 0 R /Prev {previous_xref} >>\nstartxref\n{current_xref}\n%%EOF"
        )
        .as_bytes(),
    );
    bytes
}

/// A hybrid file whose classic table contains free placeholders for members
/// in an auxiliary xref stream.  The older object stream contains both a
/// stale copy of object 9 and object 11, while the live xref stream points at
/// object 9 in the newer stream and marks object 11 free.  A reader must use
/// the active container/index mapping rather than accepting every ObjStm
/// member it happens to decode.
fn updated_compressed_object_fixture() -> Vec<u8> {
    fn append_object(bytes: &mut Vec<u8>, id: u32, body: &[u8]) -> usize {
        let offset = bytes.len();
        bytes.extend_from_slice(format!("{id} 0 obj\n").as_bytes());
        bytes.extend_from_slice(body);
        bytes.extend_from_slice(b"\nendobj\n");
        offset
    }

    fn encode_object_stream(plain: &[u8]) -> Vec<u8> {
        let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
        encoder.write_all(plain).unwrap();
        encoder.finish().unwrap()
    }

    let mut bytes = b"%PDF-1.5\n".to_vec();
    let offsets = [
        append_object(
            &mut bytes,
            1,
            b"<< /Type /Catalog /Pages 2 0 R /StructTreeRoot 9 0 R >>",
        ),
        append_object(&mut bytes, 2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>"),
        append_object(
            &mut bytes,
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Contents 4 0 R >>",
        ),
        append_object(&mut bytes, 4, b"<< /Length 3 >>\nstream\nq Q\nendstream"),
    ];

    let old_nine = b"<< /V (old) /AP << /N << /State /Off >> >> >>";
    let old_eleven = b"<< /V (stale) >>";
    let old_header = format!("9 0 11 {} ", old_nine.len() + 1);
    let mut old_plain = old_header.into_bytes();
    let old_first = old_plain.len();
    old_plain.extend_from_slice(old_nine);
    old_plain.push(b' ');
    old_plain.extend_from_slice(old_eleven);
    let old_data = encode_object_stream(&old_plain);
    let old_body = format!(
        "<< /Type /ObjStm /N 2 /First {old_first} /Filter /FlateDecode /Length {} >>\nstream\n",
        old_data.len()
    );
    let old_offset = bytes.len();
    bytes.extend_from_slice(b"5 0 obj\n");
    bytes.extend_from_slice(old_body.as_bytes());
    bytes.extend_from_slice(&old_data);
    bytes.extend_from_slice(b"\nendstream\nendobj\n");

    let new_nine = b"<< /V (new) /AP << /N << /State /On >> >> >>";
    let new_plain = [b"9 0 ".as_slice(), new_nine].concat();
    let new_first = 4;
    let new_data = encode_object_stream(&new_plain);
    let new_body = format!(
        "<< /Type /ObjStm /N 1 /First {new_first} /Filter /FlateDecode /Length {} >>\nstream\n",
        new_data.len()
    );
    let new_offset = bytes.len();
    bytes.extend_from_slice(b"7 0 obj\n");
    bytes.extend_from_slice(new_body.as_bytes());
    bytes.extend_from_slice(&new_data);
    bytes.extend_from_slice(b"\nendstream\nendobj\n");

    // [type, four-byte container/offset, two-byte generation/index] for
    // object numbers 9, 10, and 11.  Object 9 is live in stream 7; object
    // 10 and object 11 are free in the final xref.
    let xref_stream_data = [
        2_u8, 0, 0, 0, 7, 0, 0, // 9: compressed in 7, member index 0
        0, 0, 0, 0, 0, 0, 0, // 10: free
        0, 0, 0, 0, 0, 0, 0, // 11: free
    ];
    let xref_stream_offset = bytes.len();
    let xref_body = format!(
        "<< /Type /XRef /W [1 4 2] /Size 12 /Index [9 3] /Length {} >>\nstream\n",
        xref_stream_data.len()
    );
    bytes.extend_from_slice(b"8 0 obj\n");
    bytes.extend_from_slice(xref_body.as_bytes());
    bytes.extend_from_slice(&xref_stream_data);
    bytes.extend_from_slice(b"\nendstream\nendobj\n");

    let xref_offset = bytes.len();
    bytes.extend_from_slice(b"xref\n0 9\n0000000000 65535 f \n");
    for offset in offsets {
        bytes.extend_from_slice(xref_entry(offset).as_bytes());
    }
    // Object 5 is the old stream, object 6 is intentionally free, object 7
    // is the live stream, and object 8 is the auxiliary xref stream.
    bytes.extend_from_slice(xref_entry(old_offset).as_bytes());
    bytes.extend_from_slice(b"0000000000 00000 f \n");
    bytes.extend_from_slice(xref_entry(new_offset).as_bytes());
    bytes.extend_from_slice(xref_entry(xref_stream_offset).as_bytes());
    bytes
        .extend_from_slice(b"9 3\n0000000000 00000 f \n0000000000 00000 f \n0000000000 00000 f \n");
    bytes.extend_from_slice(
        format!(
            "trailer\n<< /Size 12 /Root 1 0 R /XRefStm {xref_stream_offset} >>\nstartxref\n{xref_offset}\n%%EOF"
        )
        .as_bytes(),
    );
    bytes
}

#[test]
fn hybrid_incremental_xref_retains_compressed_catalog_references() {
    let input = hybrid_incremental_fixture(false);
    let before = Document::load_mem(&input).unwrap();
    assert!(before.get_object((9, 0)).is_ok());

    let result = compress(
        &input,
        &Options {
            keep_if_larger: true,
            ..Options::default()
        },
    )
    .unwrap();
    let after = Document::load_mem(&result.bytes).unwrap();
    assert!(after.get_object((9, 0)).is_ok());
    assert_eq!(after.get_pages().len(), before.get_pages().len());
}

#[test]
fn standalone_hybrid_xref_loads_auxiliary_object_stream() {
    let input = hybrid_incremental_fixture(true);
    let document = Document::load_mem(&input).unwrap();
    assert!(document.get_object((9, 0)).is_ok());
}

#[test]
fn older_hybrid_xref_loads_auxiliary_object_stream() {
    let input = older_hybrid_fixture();
    let document = Document::load_mem(&input).unwrap();
    assert!(document.get_object((9, 0)).is_ok());
}

#[test]
fn active_compressed_revision_wins_and_free_members_stay_absent() {
    let input = updated_compressed_object_fixture();
    let source = Document::load_mem(&input).unwrap();
    let object = source.get_object((9, 0)).unwrap().as_dict().unwrap();
    assert_eq!(object.get(b"V").unwrap().as_str().unwrap(), b"new");
    let appearance = object
        .get(b"AP")
        .unwrap()
        .as_dict()
        .unwrap()
        .get(b"N")
        .unwrap()
        .as_dict()
        .unwrap();
    assert_eq!(appearance.get(b"State").unwrap().as_name().unwrap(), b"On");
    assert!(source.get_object((11, 0)).is_err());

    let result = compress(
        &input,
        &Options {
            keep_if_larger: true,
            ..Options::default()
        },
    )
    .unwrap();
    let output = Document::load_mem(&result.bytes).unwrap();
    let object = output.get_object((9, 0)).unwrap().as_dict().unwrap();
    assert_eq!(object.get(b"V").unwrap().as_str().unwrap(), b"new");
    assert!(output.get_object((11, 0)).is_err());
}
