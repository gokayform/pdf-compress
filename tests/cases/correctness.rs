use crate::support::{fixture, save};
use lopdf::{dictionary, Document, Object, Stream};
use pdf_compress::{compress, Options, Preset};

fn decoded(stream: &Stream) -> Vec<u8> {
    if stream.dict.has(b"Filter") {
        stream.decompressed_content().unwrap()
    } else {
        stream.content.clone()
    }
}

#[test]
fn lossless_preserves_content_images_forms_links_and_boxes() {
    let input = fixture();
    let result = compress(&input, &Options::default()).unwrap();
    assert!(
        result.bytes.len() < input.len() / 2,
        "compressible fixture did not shrink substantially"
    );
    assert_eq!(result.report.input_bytes, input.len());
    assert_eq!(result.report.output_bytes, result.bytes.len());
    let before = Document::load_mem(&input).unwrap();
    let after = Document::load_mem(&result.bytes).unwrap();
    assert_eq!(before.get_pages().len(), after.get_pages().len());
    assert_eq!(
        before.extract_text(&[1, 2]).unwrap(),
        after.extract_text(&[1, 2]).unwrap()
    );
    for ((_, a), (_, b)) in before.get_pages().iter().zip(after.get_pages().iter()) {
        assert_eq!(
            before.get_page_content(*a).unwrap(),
            after.get_page_content(*b).unwrap()
        );
        for key in [b"MediaBox".as_slice(), b"CropBox"] {
            assert_eq!(
                before
                    .get_object(*a)
                    .unwrap()
                    .as_dict()
                    .unwrap()
                    .get(key)
                    .unwrap(),
                after
                    .get_object(*b)
                    .unwrap()
                    .as_dict()
                    .unwrap()
                    .get(key)
                    .unwrap()
            );
        }
        let aa = before.get_page_annotations(*a).unwrap();
        let bb = after.get_page_annotations(*b).unwrap();
        assert_eq!(aa.len(), bb.len());
        for (a, b) in aa.iter().zip(bb.iter()) {
            for key in [b"Subtype".as_slice(), b"Rect", b"T", b"V", b"FT", b"A"] {
                assert_eq!(a.get(key).ok(), b.get(key).ok(), "annotation {key:?}");
            }
        }
    }
    let imgs = |d: &Document| {
        d.objects
            .values()
            .filter_map(|o| o.as_stream().ok())
            .filter(|s| s.dict.get(b"Subtype").and_then(Object::as_name).ok() == Some(b"Image"))
            .map(decoded)
            .collect::<Vec<_>>()
    };
    assert_eq!(imgs(&before), imgs(&after));
    let form = after
        .catalog()
        .unwrap()
        .get(b"AcroForm")
        .unwrap()
        .as_dict()
        .unwrap();
    let fields = form.get(b"Fields").unwrap().as_array().unwrap();
    assert_eq!(fields.len(), 1);
    let field = after
        .get_object(fields[0].as_reference().unwrap())
        .unwrap()
        .as_dict()
        .unwrap();
    assert_eq!(field.get(b"V").unwrap().as_str().unwrap(), b"Preserve me");
    assert!(field.has(b"AP"));
}

#[test]
fn deterministic_and_never_larger() {
    let input = fixture();
    let a = compress(&input, &Options::default()).unwrap();
    let b = compress(&input, &Options::default()).unwrap();
    assert_eq!(a.bytes, b.bytes);
    let c = compress(&a.bytes, &Options::default()).unwrap();
    assert!(c.bytes.len() <= a.bytes.len());
    Document::load_mem(&c.bytes).unwrap();
}

#[test]
fn unknown_filter_and_binary_payload_survive() {
    let mut d = Document::load_mem(&fixture()).unwrap();
    let payload = b"\0endstream\nendobj\xff\x01 /Prev /XRefStm /Length 123 fake binary".repeat(100);
    let id = d.add_object(Stream::new(
        dictionary! {"Filter"=>"FutureFilter"},
        payload.clone(),
    ));
    d.catalog_mut().unwrap().set("PrivateData", id);
    let out = compress(&save(d), &Options::default()).unwrap();
    let d = Document::load_mem(&out.bytes).unwrap();
    let id = d
        .catalog()
        .unwrap()
        .get(b"PrivateData")
        .unwrap()
        .as_reference()
        .unwrap();
    let stream = d.get_object(id).unwrap().as_stream().unwrap();
    assert_eq!(stream.content, payload);
    assert_eq!(
        stream.dict.get(b"Filter").unwrap().as_name().unwrap(),
        b"FutureFilter"
    );
}

#[test]
fn rejects_bad_input_options_and_input_limits() {
    for bytes in [
        b"".as_slice(),
        b"not a PDF",
        b"%PDF-1.7\n1 0 obj << /Root 9 0 R >>",
    ] {
        assert!(compress(bytes, &Options::default()).is_err());
    }
    let bytes = fixture();
    let options = Options {
        max_input_bytes: bytes.len() - 1,
        ..Options::default()
    };
    assert!(compress(&bytes, &options).is_err());
    let options = Options {
        jpeg_quality: 0,
        ..Options::default()
    };
    assert!(compress(&bytes, &options).is_err());
    let options = Options {
        target_dpi: Some(0),
        ..Options::default()
    };
    assert!(compress(&bytes, &options).is_err());
}

#[test]
fn signed_document_is_not_silently_invalidated() {
    let mut d = Document::load_mem(&fixture()).unwrap();
    let sig=d.add_object(dictionary!{"Type"=>"Sig", "ByteRange"=>vec![0.into(),100.into(),200.into(),100.into()],"Contents"=>Object::string_literal("signature bytes")});
    d.catalog_mut()
        .unwrap()
        .set("Perms", dictionary! {"DocMDP"=>sig});
    assert!(compress(&save(d), &Options::default()).is_err());
}

#[test]
fn byte_range_text_in_content_stream_is_not_a_signature() {
    let mut d = Document::load_mem(&fixture()).unwrap();
    let page = *d.get_pages().get(&1).unwrap();
    let content = d.add_object(Stream::new(
        dictionary! {},
        b"BT /F1 12 Tf 100 700 Td (This PDF discusses /ByteRange signatures) Tj ET".to_vec(),
    ));
    d.get_object_mut(page)
        .unwrap()
        .as_dict_mut()
        .unwrap()
        .set("Contents", content);
    let input = save(d);
    assert!(input
        .windows(b"/ByteRange".len())
        .any(|window| window == b"/ByteRange"));
    let out = compress(&input, &Options::default()).unwrap();
    let d = Document::load_mem(&out.bytes).unwrap();
    assert!(d
        .extract_text(&[1])
        .unwrap()
        .contains("This PDF discusses /ByteRange signatures"));
}

#[test]
fn signature_in_superseded_incremental_revision_is_rejected() {
    let mut d = Document::load_mem(&fixture()).unwrap();
    let sig = d.add_object(dictionary! {"Type"=>"Sig", "ByteRange"=>vec![0.into(),100.into(),200.into(),100.into()], "Contents"=>Object::string_literal("signature bytes")});
    let root = d.trailer.get(b"Root").unwrap().as_reference().unwrap();
    let size = d.max_id + 1;
    let mut input = save(d);

    let tail = String::from_utf8_lossy(&input[input.len() - 64..]).into_owned();
    let prev: usize = tail
        .rsplit("startxref")
        .next()
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let object_offset = input.len() + 1;
    input
        .extend_from_slice(format!("\n{} 0 obj\n<< /Replaced true >>\nendobj\n", sig.0).as_bytes());
    let xref_offset = input.len();
    input.extend_from_slice(
        format!(
            "xref\n0 1\n0000000000 65535 f \n{} 1\n{object_offset:010} 00000 n \ntrailer\n<< /Size {size} /Root {} 0 R /Prev {prev} >>\nstartxref\n{xref_offset}\n%%EOF\n",
            sig.0, root.0
        )
        .as_bytes(),
    );

    let current = Document::load_mem(&input).unwrap();
    let replaced = current.get_object(sig).unwrap().as_dict().unwrap();
    assert!(replaced.get(b"ByteRange").is_err());
    assert!(replaced.has(b"Replaced"));

    let error = compress(&input, &Options::default()).unwrap_err();
    assert!(
        error.to_string().contains("signed documents"),
        "unexpected error: {error}"
    );
}

fn corrupt_flate_payload() -> Vec<u8> {
    b"\x78\x9c this is definitely not a deflate stream \xff\xfe\xfd".repeat(4)
}

#[test]
fn corrupt_flate_form_in_page_resources_is_preserved_with_warning() {
    let mut d = Document::load_mem(&fixture()).unwrap();
    let payload = corrupt_flate_payload();
    let form = d.add_object(Stream::new(
        dictionary! {"Type"=>"XObject", "Subtype"=>"Form", "BBox"=>vec![0.into(),0.into(),10.into(),10.into()], "Filter"=>"FlateDecode"},
        payload.clone(),
    ));
    let page = *d.get_pages().get(&1).unwrap();
    d.get_object_mut(page)
        .unwrap()
        .as_dict_mut()
        .unwrap()
        .get_mut(b"Resources")
        .unwrap()
        .as_dict_mut()
        .unwrap()
        .get_mut(b"XObject")
        .unwrap()
        .as_dict_mut()
        .unwrap()
        .set("Broken", form);
    let out = compress(&save(d), &Options::default()).unwrap();
    assert!(
        out.report
            .warnings
            .iter()
            .any(|warning| warning.contains("could not be decoded")),
        "missing warning: {:?}",
        out.report.warnings
    );
    let d = Document::load_mem(&out.bytes).unwrap();
    let page = *d.get_pages().get(&1).unwrap();
    let form = d
        .get_dictionary(page)
        .unwrap()
        .get(b"Resources")
        .and_then(Object::as_dict)
        .unwrap()
        .get(b"XObject")
        .and_then(Object::as_dict)
        .unwrap()
        .get(b"Broken")
        .and_then(Object::as_reference)
        .unwrap();
    let stream = d.get_object(form).unwrap().as_stream().unwrap();
    assert_eq!(stream.content, payload);
    assert_eq!(
        stream.dict.get(b"Filter").unwrap().as_name().unwrap(),
        b"FlateDecode"
    );
}

#[test]
fn unreachable_corrupt_flate_stream_does_not_abort_compression() {
    let mut d = Document::load_mem(&fixture()).unwrap();
    d.add_object(Stream::new(
        dictionary! {"Type"=>"XObject", "Subtype"=>"Form", "BBox"=>vec![0.into(),0.into(),10.into(),10.into()], "Filter"=>"FlateDecode"},
        corrupt_flate_payload(),
    ));
    let out = compress(&save(d), &Options::default()).unwrap();
    let d = Document::load_mem(&out.bytes).unwrap();
    assert_eq!(d.get_pages().len(), 2);
    assert!(out.report.objects_removed > 0);
}

#[test]
fn abbreviated_flate_filter_is_recompressed() {
    use std::io::Write;
    let mut d = Document::load_mem(&fixture()).unwrap();
    let data = b"abbreviated Flate filter data ".repeat(400);
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::none());
    encoder.write_all(&data).unwrap();
    let id = d.add_object(Stream::new(
        dictionary! {"Filter"=>"Fl"},
        encoder.finish().unwrap(),
    ));
    d.catalog_mut().unwrap().set("AbbreviatedData", id);
    let out = compress(
        &save(d),
        &Options {
            keep_if_larger: true,
            ..Options::default()
        },
    )
    .unwrap();
    let d = Document::load_mem(&out.bytes).unwrap();
    let id = d
        .catalog()
        .unwrap()
        .get(b"AbbreviatedData")
        .unwrap()
        .as_reference()
        .unwrap();
    let stream = d.get_object(id).unwrap().as_stream().unwrap();
    assert_eq!(
        stream.dict.get(b"Filter").unwrap().as_name().unwrap(),
        b"Fl"
    );
    assert!(stream.content.len() < data.len() / 4);
    let mut decoded = Vec::new();
    std::io::Read::read_to_end(
        &mut flate2::read::ZlibDecoder::new(stream.content.as_slice()),
        &mut decoded,
    )
    .unwrap();
    assert_eq!(decoded, data);
}

#[test]
fn ebook_reduces_image_and_preserves_text_and_forms() {
    let input = fixture();
    let options = Options::for_preset(Preset::Ebook);
    let out = compress(&input, &options).unwrap();
    assert!(
        out.report.images_optimized > 0,
        "ebook did not optimize the supported RGB image"
    );
    let d = Document::load_mem(&out.bytes).unwrap();
    assert_eq!(d.get_pages().len(), 2);
    assert!(d
        .extract_text(&[1, 2])
        .unwrap()
        .contains("Compression reference page 2"));
    assert!(d.catalog().unwrap().has(b"AcroForm"));
    assert!(out.bytes.len() < input.len() / 4);
}

#[test]
fn decode_bomb_is_bounded() {
    use std::io::Write;
    let mut d = Document::load_mem(&fixture()).unwrap();
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
    encoder.write_all(&vec![b' '; 1024 * 1024]).unwrap();
    let id = d.add_object(Stream::new(
        dictionary! {"Filter"=>"FlateDecode"},
        encoder.finish().unwrap(),
    ));
    d.catalog_mut().unwrap().set("BoundedData", id);
    let input = save(d);
    let options = Options {
        max_decoded_stream_bytes: 4096,
        ..Options::default()
    };
    // Explicit rejection or safe skip is acceptable; unbounded decoding is not.
    match compress(&input, &options) {
        Err(_) => {}
        Ok(out) => {
            assert!(
                !out.report.warnings.is_empty(),
                "skipped decode limit must be reported"
            );
            assert!(out.bytes.len() <= input.len());
        }
    }
}

#[test]
fn malformed_inputs_do_not_panic() {
    let mut seed = 0x1357_2468_u32;
    for len in [1, 8, 32, 128, 1024] {
        for _ in 0..20 {
            let mut bytes = b"%PDF-1.7\n".to_vec();
            for _ in 0..len {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                bytes.push(seed as u8);
            }
            assert!(std::panic::catch_unwind(|| compress(&bytes, &Options::default())).is_ok());
        }
    }
}

#[test]
fn output_xref_offsets_lengths_and_references_are_consistent() {
    let result = compress(&fixture(), &Options::default()).unwrap();
    assert!(!result.report.used_original);
    let offsets = crate::support::structure::classic_xref(&result.bytes).unwrap();
    let doc = Document::load_mem(&result.bytes).unwrap();
    assert_eq!(
        doc.trailer.get(b"Size").unwrap().as_i64().unwrap(),
        offsets.keys().map(|id| id.0 as i64).max().unwrap() + 1
    );
    assert!(!doc.trailer.has(b"Prev"));
    assert!(!doc.trailer.has(b"XRefStm"));
    let mut pending = doc.objects.values().collect::<Vec<_>>();
    pending.extend(doc.trailer.iter().map(|(_, v)| v));
    while let Some(obj) = pending.pop() {
        match obj {
            Object::Reference(id) => {
                assert!(doc.objects.contains_key(id), "dangling reference {id:?}")
            }
            Object::Array(a) => pending.extend(a),
            Object::Dictionary(d) => pending.extend(d.iter().map(|(_, v)| v)),
            Object::Stream(s) => pending.extend(s.dict.iter().map(|(_, v)| v)),
            _ => {}
        }
    }
    for (id, object) in &doc.objects {
        assert!(offsets.contains_key(id), "object absent from xref: {id:?}");
        if let Object::Stream(s) = object {
            let length = s.dict.get(b"Length").unwrap().as_i64().unwrap() as usize;
            assert_eq!(length, s.content.len());
            let object_start = offsets[id];
            let rest = &result.bytes[object_start..];
            let start = rest.windows(7).position(|w| w == b"stream\n").unwrap() + 7;
            assert_eq!(&rest[start..start + length], s.content);
            assert!(rest[start + length..].starts_with(b"\nendstream"));
        }
    }
}
