use crate::support::{fixture, save, textured_fixture};
use lopdf::{dictionary, Document, Object, ObjectId, Stream};
use pdf_compress::{compress, Options, Preset};

fn image_id(d: &Document) -> ObjectId {
    *d.objects
        .iter()
        .find(|(_, o)| {
            o.as_stream().ok().is_some_and(|s| {
                s.dict.get(b"Subtype").and_then(Object::as_name).ok() == Some(b"Image")
            })
        })
        .unwrap()
        .0
}

#[test]
fn lossless_preserves_decode_arrays_and_soft_masks() {
    let mut d = Document::load_mem(&fixture()).unwrap();
    let image = image_id(&d);
    let mask=d.add_object(Stream::new(dictionary!{"Type"=>"XObject","Subtype"=>"Image","Width"=>600,"Height"=>600,"ColorSpace"=>"DeviceGray","BitsPerComponent"=>8},vec![128;600*600]));
    let s = d.get_object_mut(image).unwrap().as_stream_mut().unwrap();
    s.dict.set("SMask", mask);
    s.dict.set(
        "Decode",
        vec![1.into(), 0.into(), 1.into(), 0.into(), 1.into(), 0.into()],
    );
    let original = s.content.clone();
    for preset in [Preset::Lossless, Preset::Ebook] {
        let input = save(d.clone());
        let out = compress(&input, &Options::for_preset(preset)).unwrap();
        let after = Document::load_mem(&out.bytes).unwrap();
        let s = after.get_object(image).unwrap().as_stream().unwrap();
        assert!(s.dict.has(b"SMask"));
        assert!(s.dict.has(b"Decode"));
        let data = if s.dict.has(b"Filter") {
            s.decompressed_content().unwrap()
        } else {
            s.content.clone()
        };
        assert_eq!(data, original);
    }
}

#[test]
fn repeated_image_keeps_resolution_for_largest_placement() {
    let mut d = Document::load_mem(&textured_fixture()).unwrap();
    let id = image_id(&d);
    let page = *d.get_pages().get(&2).unwrap();
    let contents = d.get_page_contents(page)[0];
    d.get_object_mut(contents)
        .unwrap()
        .as_stream_mut()
        .unwrap()
        .set_content(b"q 288 0 0 288 20 20 cm /Im1 Do Q".to_vec());
    let out = compress(&save(d), &Options::for_preset(Preset::Ebook)).unwrap();
    let d = Document::load_mem(&out.bytes).unwrap();
    let image = d.get_object(id).unwrap().as_stream().unwrap();
    assert_eq!(
        image.dict.get(b"Width").unwrap().as_i64().unwrap(),
        600,
        "largest use is 4in at 150dpi"
    );
}

#[test]
fn user_unit_scales_required_image_resolution() {
    let mut d = Document::load_mem(&textured_fixture()).unwrap();
    let image = image_id(&d);
    for (_, id) in d.get_pages() {
        d.get_object_mut(id)
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .set("UserUnit", 2);
    }
    let out = compress(&save(d), &Options::for_preset(Preset::Ebook)).unwrap();
    let d = Document::load_mem(&out.bytes).unwrap();
    assert_eq!(
        d.get_object(image)
            .unwrap()
            .as_stream()
            .unwrap()
            .dict
            .get(b"Width")
            .unwrap()
            .as_i64()
            .unwrap(),
        600
    );
}

#[test]
fn unresolved_placement_does_not_downsample_reused_image() {
    let mut d = Document::load_mem(&textured_fixture()).unwrap();
    let image = image_id(&d);
    let page = *d.get_pages().get(&2).unwrap();
    let content = d.get_page_contents(page)[0];
    // A legitimate but unsupported content encoding prevents placement analysis.
    // The shared image cannot be safely downsized based only on the first page.
    let s = d.get_object_mut(content).unwrap().as_stream_mut().unwrap();
    let bytes = b"q 576 0 0 576 0 0 cm /Im1 Do Q";
    s.set_content(
        bytes
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
            .into_bytes(),
    );
    s.content.push(b'>');
    s.dict.set("Length", s.content.len() as i64);
    s.dict.set("Filter", "ASCIIHexDecode");
    let out = compress(&save(d), &Options::for_preset(Preset::Ebook)).unwrap();
    let d = Document::load_mem(&out.bytes).unwrap();
    assert_eq!(
        d.get_object(image)
            .unwrap()
            .as_stream()
            .unwrap()
            .dict
            .get(b"Width")
            .unwrap()
            .as_i64()
            .unwrap(),
        600
    );
    assert!(!out.report.warnings.is_empty());
}

#[test]
fn incremental_trailer_is_flattened_without_stale_offsets() {
    let mut d = Document::load_mem(&fixture()).unwrap();
    // Build a real incremental update using the parser/writer's append API.
    let original = save(d.clone());
    let parsed = Document::load_mem(&original).unwrap();
    let mut update = lopdf::IncrementalDocument::create_from(original, parsed);
    let id = update
        .new_document
        .add_object(dictionary! {"Title"=>Object::string_literal("latest revision")});
    update.new_document.trailer.set("Info", id);
    let mut input = Vec::new();
    update.save_to(&mut input).unwrap();
    let result = compress(
        &input,
        &Options {
            keep_if_larger: true,
            ..Options::default()
        },
    )
    .unwrap();
    d = Document::load_mem(&result.bytes).unwrap();
    assert!(!d.trailer.has(b"Prev"));
    assert!(!d.trailer.has(b"XRefStm"));
    let info = d
        .get_object(d.trailer.get(b"Info").unwrap().as_reference().unwrap())
        .unwrap()
        .as_dict()
        .unwrap();
    assert_eq!(
        info.get(b"Title").unwrap().as_str().unwrap(),
        b"latest revision"
    );
    crate::support::structure::classic_xref(&result.bytes).unwrap();
}

#[test]
fn missing_references_become_null_and_page_cycles_are_rejected() {
    let mut d = Document::load_mem(&fixture()).unwrap();
    d.catalog_mut()
        .unwrap()
        .set("Missing", Object::Reference((999999, 0)));
    let result = compress(
        &save(d),
        &Options {
            keep_if_larger: true,
            ..Options::default()
        },
    )
    .unwrap();
    let output = Document::load_mem(&result.bytes).unwrap();
    assert_eq!(
        output.catalog().unwrap().get(b"Missing").unwrap(),
        &Object::Null
    );
    let mut d = Document::load_mem(&fixture()).unwrap();
    let pages = d
        .catalog()
        .unwrap()
        .get(b"Pages")
        .unwrap()
        .as_reference()
        .unwrap();
    d.get_object_mut(pages)
        .unwrap()
        .as_dict_mut()
        .unwrap()
        .set("Kids", vec![Object::Reference(pages)]);
    assert!(compress(&save(d), &Options::default()).is_err());
}

#[test]
fn inconsistent_page_counts_and_parents_are_rejected() {
    let mut d = Document::load_mem(&fixture()).unwrap();
    let pages = d
        .catalog()
        .unwrap()
        .get(b"Pages")
        .unwrap()
        .as_reference()
        .unwrap();
    d.get_object_mut(pages)
        .unwrap()
        .as_dict_mut()
        .unwrap()
        .set("Count", 99);
    assert!(compress(&save(d), &Options::default()).is_err());
    let mut d = Document::load_mem(&fixture()).unwrap();
    let page = *d.get_pages().get(&1).unwrap();
    d.get_object_mut(page)
        .unwrap()
        .as_dict_mut()
        .unwrap()
        .remove(b"Parent");
    assert!(compress(&save(d), &Options::default()).is_err());
}

#[test]
fn never_returns_known_invalid_original_stream_length() {
    // Use valid, already compressed classic-table bytes so a rewrite is likely
    // to grow after corrupting /Length. Parser repair must never be hidden by
    // the no-growth fallback.
    let valid = compress(&fixture(), &Options::default()).unwrap().bytes;
    let d = Document::load_mem(&valid).unwrap();
    let id = d.get_page_contents(*d.get_pages().get(&1).unwrap())[0];
    let offsets = crate::support::structure::classic_xref(&valid).unwrap();
    let start = offsets[&id];
    let length_pos = start
        + valid[start..]
            .windows(7)
            .position(|w| w == b"/Length")
            .unwrap()
        + 7;
    let mut damaged = valid.clone();
    let mut index = length_pos;
    while damaged[index].is_ascii_whitespace() {
        index += 1;
    }
    let number_start = index;
    while damaged[index].is_ascii_digit() {
        index += 1;
    }
    damaged[number_start..index].fill(b'0');
    match compress(&damaged, &Options::default()) {
        Err(_) => {}
        Ok(result) => {
            assert!(!result.report.used_original);
            assert_ne!(result.bytes, damaged);
        }
    }
}

#[test]
fn object_stream_input_produces_valid_classic_output() {
    let (_, modern) = crate::support::corpus()
        .into_iter()
        .find(|(name, _)| *name == "object-streams")
        .unwrap();
    let out = compress(
        &modern,
        &Options {
            keep_if_larger: true,
            ..Options::default()
        },
    )
    .unwrap();
    crate::support::structure::classic_xref(&out.bytes).unwrap();
    let d = Document::load_mem(&out.bytes).unwrap();
    assert_eq!(d.get_pages().len(), 2);
    assert!(d
        .extract_text(&[1, 2])
        .unwrap()
        .contains("Compression reference page 2"));
}

#[test]
fn encrypted_pdfs_are_rejected_even_with_empty_user_password() {
    for user_password in ["", "secret"] {
        let mut d = Document::load_mem(&fixture()).unwrap();
        d.trailer.set(
            "ID",
            vec![
                Object::string_literal("0123456789abcdef"),
                Object::string_literal("0123456789abcdef"),
            ],
        );
        let state = lopdf::EncryptionState::try_from(lopdf::EncryptionVersion::V1 {
            document: &d,
            owner_password: "owner",
            user_password,
            permissions: lopdf::Permissions::PRINTABLE,
        })
        .unwrap();
        d.encrypt(&state).unwrap();
        assert!(
            compress(&save(d), &Options::default()).is_err(),
            "encrypted input with password {user_password:?} accepted"
        );
    }
}

#[test]
fn shared_image_in_annotation_appearance_is_not_downsampled() {
    let mut d = Document::load_mem(&textured_fixture()).unwrap();
    let image = image_id(&d);
    let appearance=d.add_object(Stream::new(dictionary!{"Type"=>"XObject","Subtype"=>"Form","BBox"=>vec![0.into(),0.into(),576.into(),576.into()],"Resources"=>dictionary!{"XObject"=>dictionary!{"Im1"=>image}}},b"q 576 0 0 576 0 0 cm /Im1 Do Q".to_vec()));
    let annotation=d.add_object(dictionary!{"Type"=>"Annot","Subtype"=>"Stamp","Rect"=>vec![0.into(),0.into(),576.into(),576.into()],"AP"=>dictionary!{"N"=>appearance}});
    let page = *d.get_pages().get(&1).unwrap();
    d.get_object_mut(page)
        .unwrap()
        .as_dict_mut()
        .unwrap()
        .get_mut(b"Annots")
        .unwrap()
        .as_array_mut()
        .unwrap()
        .push(annotation.into());
    let out = compress(&save(d), &Options::for_preset(Preset::Ebook)).unwrap();
    let d = Document::load_mem(&out.bytes).unwrap();
    assert_eq!(
        d.get_object(image)
            .unwrap()
            .as_stream()
            .unwrap()
            .dict
            .get(b"Width")
            .unwrap()
            .as_i64()
            .unwrap(),
        600
    );
}

#[test]
fn lossless_preserves_jpeg_bitstream() {
    let (_, input) = crate::support::corpus()
        .into_iter()
        .find(|(name, _)| *name == "jpeg")
        .unwrap();
    let before = Document::load_mem(&input).unwrap();
    let image = image_id(&before);
    let out = compress(&input, &Options::default()).unwrap();
    let after = Document::load_mem(&out.bytes).unwrap();
    assert_eq!(
        before
            .get_object(image)
            .unwrap()
            .as_stream()
            .unwrap()
            .content,
        after
            .get_object(image)
            .unwrap()
            .as_stream()
            .unwrap()
            .content
    );
}

fn mean_rgb(jpeg: &[u8]) -> [f64; 3] {
    let pixels = image::load_from_memory_with_format(jpeg, image::ImageFormat::Jpeg)
        .unwrap()
        .to_rgb8()
        .into_raw();
    let mut sums = [0.0; 3];
    for pixel in pixels.chunks_exact(3) {
        for (sum, &sample) in sums.iter_mut().zip(pixel) {
            *sum += f64::from(sample);
        }
    }
    sums.map(|sum| sum / (pixels.len() / 3) as f64)
}

/// With `/ColorTransform 0` the JPEG's YCbCr components are the RGB samples,
/// so the re-encoded image must carry those values as its colour.
#[test]
fn jpeg_color_transform_is_honoured_in_lossy_mode() {
    let (_, input) = crate::support::corpus()
        .into_iter()
        .find(|(name, _)| *name == "jpeg")
        .unwrap();
    let mut before = Document::load_mem(&input).unwrap();
    let image = image_id(&before);
    before
        .get_object_mut(image)
        .unwrap()
        .as_stream_mut()
        .unwrap()
        .dict
        .set("DecodeParms", dictionary! {"ColorTransform"=>0});
    let original = before
        .get_object(image)
        .unwrap()
        .as_stream()
        .unwrap()
        .content
        .clone();
    let out = compress(&save(before), &Options::for_preset(Preset::Ebook)).unwrap();
    let after = Document::load_mem(&out.bytes).unwrap();
    let stream = after.get_object(image).unwrap().as_stream().unwrap();
    assert!(stream.content.len() < original.len());
    assert!(!stream.dict.has(b"DecodeParms"));
    assert_eq!(
        stream.dict.get(b"Filter").unwrap().as_name().unwrap(),
        b"DCTDecode"
    );

    let [r, g, b] = mean_rgb(&original);
    let expected = [
        0.299 * r + 0.587 * g + 0.114 * b,
        128.0 - 0.168_736 * r - 0.331_264 * g + 0.5 * b,
        128.0 + 0.5 * r - 0.418_688 * g - 0.081_312 * b,
    ];
    let actual = mean_rgb(&stream.content);
    for (actual, expected) in actual.iter().zip(expected) {
        assert!(
            (actual - expected).abs() < 3.0,
            "{actual:?} vs {expected:?}"
        );
    }
}

#[test]
fn lossy_mode_actually_downsamples_textured_image() {
    let input = textured_fixture();
    let before = Document::load_mem(&input).unwrap();
    let image = image_id(&before);
    let out = compress(&input, &Options::for_preset(Preset::Ebook)).unwrap();
    let after = Document::load_mem(&out.bytes).unwrap();
    let stream = after.get_object(image).unwrap().as_stream().unwrap();
    assert_eq!(
        stream.dict.get(b"Filter").unwrap().as_name().unwrap(),
        b"DCTDecode"
    );
    assert_eq!(stream.dict.get(b"Width").unwrap().as_i64().unwrap(), 300);
    assert_eq!(stream.dict.get(b"Height").unwrap().as_i64().unwrap(), 300);
    let decoded =
        image::load_from_memory_with_format(&stream.content, image::ImageFormat::Jpeg).unwrap();
    assert_eq!((decoded.width(), decoded.height()), (300, 300));
}
