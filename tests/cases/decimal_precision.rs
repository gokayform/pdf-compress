use crate::support::{fixture, save};
use lopdf::{dictionary, Document, Object};
use pdf_compress::{compress, Options};

#[test]
fn page_boxes_and_annotation_coordinates_keep_decimal_precision() {
    let mut document = Document::load_mem(&fixture()).unwrap();
    let page_id = *document.get_pages().get(&1).unwrap();
    let rect = vec![
        Object::Real(56.5245036),
        Object::Real(703.059192),
        Object::Real(190.508256),
        Object::Real(691.058808),
    ];
    let annotation = document.add_object(dictionary! {
        "Type" => "Annot", "Subtype" => "Square", "Rect" => Object::Array(rect.clone()),
        "C" => vec![Object::Real(0.3333333333), Object::Real(0.6666666667), Object::Integer(0)],
    });
    let page = document
        .get_object_mut(page_id)
        .unwrap()
        .as_dict_mut()
        .unwrap();
    page.set("Annots", vec![Object::Reference(annotation)]);
    page.set(
        "MediaBox",
        vec![
            Object::Integer(0),
            Object::Integer(0),
            Object::Real(595.275574),
            Object::Real(841.889771),
        ],
    );
    let bytes = save(document);
    // Confirm the fixture actually contains the challenging decimal lexemes.
    assert!(bytes.windows(10).any(|w| w == b"56.5245036"));
    let result = compress(
        &bytes,
        &Options {
            keep_if_larger: true,
            ..Options::default()
        },
    )
    .unwrap();
    for literal in [
        "56.5245036",
        "703.059192",
        "0.3333333333",
        "595.275574",
        "841.889771",
    ] {
        assert!(
            result
                .bytes
                .windows(literal.len())
                .any(|w| w == literal.as_bytes()),
            "lost {literal}"
        );
    }
    let output = Document::load_mem(&result.bytes).unwrap();
    assert_eq!(
        output
            .get_object(annotation)
            .unwrap()
            .as_dict()
            .unwrap()
            .get(b"Rect")
            .unwrap(),
        &Object::Array(rect)
    );
}

#[test]
fn legal_trailing_whitespace_does_not_disable_no_growth_fallback() {
    let mut input = b"%PDF-1.4\n".to_vec();
    let mut offsets = vec![0];
    for (index, body) in [
        "<</Type/Catalog/Pages 2 0 R>>",
        "<</Type/Pages/Kids[3 0 R]/Count 1>>",
        "<</Type/Page/Parent 2 0 R/MediaBox[0 0 100 100]>>",
    ]
    .into_iter()
    .enumerate()
    {
        offsets.push(input.len());
        input.extend_from_slice(format!("{} 0 obj{}endobj\n", index + 1, body).as_bytes());
    }
    let xref = input.len();
    input.extend_from_slice(b"xref\n0 4\n0000000000 65535 f \n");
    for offset in &offsets[1..] {
        input.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    input.extend_from_slice(
        format!("trailer<</Root 1 0 R/Size 4>>\nstartxref\n{xref}\n%%EOF\r\n ").as_bytes(),
    );
    let rewritten = compress(
        &input,
        &Options {
            keep_if_larger: true,
            ..Options::default()
        },
    )
    .unwrap();
    assert!(
        rewritten.bytes.len() > input.len(),
        "fixture must exercise size fallback"
    );
    let result = compress(&input, &Options::default()).unwrap();
    assert!(result.report.used_original);
    assert_eq!(result.bytes, input);
}
