//! Generated annotation compatibility regressions.
//!
//! These fixtures exercise the two structures behind the PDF.js corpus cases
//! `annotation-text-without-popup` and `checkbox-bad-appearance`.  The first
//! has a real text annotation with no `/Popup`; only its invalid `null` page
//! array placeholder is repaired.  The second has a state entry whose value is
//! the name `/Off`, not an appearance stream, and must be rejected rather than
//! silently changing the checkbox's visual state.

use lopdf::{dictionary, Document, Object, Stream};
use pdf_compress::{compress, Error, Options};

fn serialize(mut document: Document) -> Vec<u8> {
    let mut bytes = Vec::new();
    document.save_to(&mut bytes).unwrap();
    bytes
}

fn serialize_modern(mut document: Document) -> Vec<u8> {
    let mut bytes = Vec::new();
    document.save_modern(&mut bytes).unwrap();
    bytes
}

fn document_with_page(
    mut document: Document,
    annotation_array: Object,
    acroform: Option<Object>,
) -> Document {
    let pages = document.new_object_id();
    let page = document.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages,
        "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        "Annots" => annotation_array,
    });
    document.objects.insert(
        pages,
        dictionary! {
            "Type" => "Pages",
            "Kids" => vec![page.into()],
            "Count" => 1,
        }
        .into(),
    );
    let mut catalog = dictionary! {"Type" => "Catalog", "Pages" => pages};
    if let Some(acroform) = acroform {
        catalog.set("AcroForm", acroform);
    }
    let root = document.add_object(catalog);
    document.trailer.set("Root", root);
    document
}

fn text_without_popup_fixture() -> Vec<u8> {
    let mut document = Document::with_version("1.4");
    let appearance = document.add_object(Stream::new(
        dictionary! {
            "Type" => "XObject",
            "Subtype" => "Form",
            "BBox" => vec![0.into(), 0.into(), 20.into(), 18.into()],
        },
        Vec::new(),
    ));
    let text_annotation = document.add_object(dictionary! {
        "Type" => "Annot",
        "Subtype" => "Text",
        "Rect" => vec![10.into(), 10.into(), 30.into(), 28.into()],
        "Contents" => Object::string_literal("Text content without Popup annotation"),
        "AP" => dictionary! {"N" => appearance},
    });
    serialize(document_with_page(
        document,
        vec![text_annotation.into(), Object::Null].into(),
        None,
    ))
}

fn text_without_popup_indirect_annots_fixture(modern: bool) -> Vec<u8> {
    let mut document = Document::with_version("1.7");
    let appearance = document.add_object(Stream::new(
        dictionary! {
            "Type" => "XObject",
            "Subtype" => "Form",
            "BBox" => vec![0.into(), 0.into(), 20.into(), 18.into()],
        },
        Vec::new(),
    ));
    let text_annotation = document.add_object(dictionary! {
        "Type" => "Annot",
        "Subtype" => "Text",
        "Rect" => vec![10.into(), 10.into(), 30.into(), 28.into()],
        "Contents" => Object::string_literal("Text content without Popup annotation"),
        "AP" => dictionary! {"N" => appearance},
    });
    let indirect_null = document.add_object(Object::Null);
    let indirect_annots = document.add_object(Object::Array(vec![
        text_annotation.into(),
        indirect_null.into(),
    ]));
    let document = document_with_page(document, indirect_annots.into(), None);
    if modern {
        serialize_modern(document)
    } else {
        serialize(document)
    }
}

fn page_with_null_annots_fixture() -> Vec<u8> {
    let document = Document::with_version("1.4");
    serialize(document_with_page(document, Object::Null, None))
}

fn page_with_indirect_null_annots_fixture() -> Vec<u8> {
    let mut document = Document::with_version("1.4");
    let null_annots = document.add_object(Object::Null);
    serialize(document_with_page(document, null_annots.into(), None))
}

fn text_with_null_appearance_fixture() -> Vec<u8> {
    let mut document = Document::with_version("1.4");
    let text_annotation = document.add_object(dictionary! {
        "Type" => "Annot",
        "Subtype" => "Text",
        "Rect" => vec![10.into(), 10.into(), 30.into(), 28.into()],
        "Contents" => Object::string_literal("Text with absent-equivalent appearance"),
        "AP" => Object::Null,
    });
    serialize(document_with_page(
        document,
        vec![text_annotation.into()].into(),
        None,
    ))
}

fn widget_fixture(malformed_off_appearance: bool) -> Vec<u8> {
    let mut document = Document::with_version("1.4");
    let off = document.add_object(Stream::new(
        dictionary! {
            "Type" => "XObject",
            "Subtype" => "Form",
            "BBox" => vec![0.into(), 0.into(), 10.into(), 10.into()],
        },
        Vec::new(),
    ));
    let on = document.add_object(Stream::new(
        dictionary! {
            "Type" => "XObject",
            "Subtype" => "Form",
            "BBox" => vec![0.into(), 0.into(), 10.into(), 10.into()],
        },
        b"q 0 0 10 10 re f Q".to_vec(),
    ));
    let off_appearance = if malformed_off_appearance {
        Object::Name(b"Off".to_vec())
    } else {
        off.into()
    };
    let widget = document.add_object(dictionary! {
        "Type" => "Annot",
        "Subtype" => "Widget",
        "FT" => "Btn",
        "T" => Object::string_literal("choice"),
        "Rect" => vec![10.into(), 10.into(), 20.into(), 20.into()],
        "F" => 4,
        "AS" => "Off",
        "AP" => dictionary! {
            "N" => dictionary! {
                "Off" => off_appearance,
                "Yes" => on,
            },
        },
    });
    let acroform = dictionary! {
        "Fields" => vec![widget.into()],
        "NeedAppearances" => false,
    };
    serialize(document_with_page(
        document,
        vec![widget.into()].into(),
        Some(acroform.into()),
    ))
}

#[test]
fn text_without_popup_keeps_annotation_and_removes_only_null_placeholder() {
    let input = text_without_popup_fixture();
    let result = compress(
        &input,
        &Options {
            keep_if_larger: true,
            ..Options::default()
        },
    )
    .expect("a text annotation without /Popup is valid");
    assert_eq!(result.report.compatibility_repairs, 1);

    let output = Document::load_mem(&result.bytes).unwrap();
    let page_id = *output.get_pages().values().next().unwrap();
    let page = output.get_dictionary(page_id).unwrap();
    let annots_object = page.get(b"Annots").unwrap().clone();
    let annots = output
        .dereference(&annots_object)
        .unwrap()
        .1
        .as_array()
        .unwrap();
    assert_eq!(annots.len(), 1);
    let annotation_id = annots[0].as_reference().unwrap();
    let annotation = output.get_dictionary(annotation_id).unwrap();
    assert_eq!(
        annotation.get(b"Subtype").unwrap().as_name().unwrap(),
        b"Text"
    );
    assert!(annotation.get(b"Popup").is_err());
    assert_eq!(
        annotation.get(b"Contents").unwrap().as_str().unwrap(),
        b"Text content without Popup annotation"
    );
}

#[test]
fn indirect_null_annots_entry_is_removed_while_annotation_dictionary_is_preserved() {
    let input = text_without_popup_indirect_annots_fixture(false);
    let result = compress(
        &input,
        &Options {
            keep_if_larger: true,
            ..Options::default()
        },
    )
    .expect("an indirect null annotation placeholder is safely removable");
    assert_eq!(result.report.compatibility_repairs, 1);

    let output = Document::load_mem(&result.bytes).unwrap();
    let page_id = *output.get_pages().values().next().unwrap();
    let page = output.get_dictionary(page_id).unwrap();
    let annots_object = page.get(b"Annots").unwrap().clone();
    let annots = output
        .dereference(&annots_object)
        .unwrap()
        .1
        .as_array()
        .unwrap();
    assert_eq!(annots.len(), 1);
    let annotation = output
        .get_dictionary(annots[0].as_reference().unwrap())
        .unwrap();
    assert_eq!(
        annotation.get(b"Type").unwrap().as_name().unwrap(),
        b"Annot"
    );
    assert_eq!(
        annotation.get(b"Subtype").unwrap().as_name().unwrap(),
        b"Text"
    );
    assert!(annotation.get(b"Popup").is_err());
    assert_eq!(
        annotation.get(b"Contents").unwrap().as_str().unwrap(),
        b"Text content without Popup annotation"
    );
}

#[test]
fn null_page_annots_value_is_absent_equivalent() {
    let result = compress(&page_with_null_annots_fixture(), &Options::default())
        .expect("null /Annots is equivalent to an absent optional key");
    assert_eq!(result.report.compatibility_repairs, 1);
    let output = Document::load_mem(&result.bytes).unwrap();
    let page_id = *output.get_pages().values().next().unwrap();
    assert!(output
        .get_dictionary(page_id)
        .unwrap()
        .get(b"Annots")
        .is_err());
}

#[test]
fn indirect_null_page_annots_value_is_absent_equivalent() {
    let result = compress(
        &page_with_indirect_null_annots_fixture(),
        &Options::default(),
    )
    .expect("an indirect null /Annots value is equivalent to an absent key");
    assert_eq!(result.report.compatibility_repairs, 1);
    let output = Document::load_mem(&result.bytes).unwrap();
    let page_id = *output.get_pages().values().next().unwrap();
    assert!(output
        .get_dictionary(page_id)
        .unwrap()
        .get(b"Annots")
        .is_err());
}

#[test]
fn null_annotation_appearance_is_absent_equivalent_and_annotation_is_retained() {
    let result = compress(&text_with_null_appearance_fixture(), &Options::default())
        .expect("null /AP is equivalent to an absent optional key");
    let output = Document::load_mem(&result.bytes).unwrap();
    let page_id = *output.get_pages().values().next().unwrap();
    let page = output.get_dictionary(page_id).unwrap();
    let annots_object = page.get(b"Annots").unwrap().clone();
    let annots = output
        .dereference(&annots_object)
        .unwrap()
        .1
        .as_array()
        .unwrap();
    let annotation = output
        .get_dictionary(annots[0].as_reference().unwrap())
        .unwrap();
    assert_eq!(
        annotation.get(b"Subtype").unwrap().as_name().unwrap(),
        b"Text"
    );
    assert!(annotation.get(b"AP").unwrap().is_null());
}

#[test]
fn compatibility_repair_is_retained_when_default_rewrite_grows() {
    let input = text_without_popup_indirect_annots_fixture(true);
    let result = compress(&input, &Options::default())
        .expect("modern input with an indirect null placeholder is repairable");
    assert!(
        result.report.output_bytes > result.report.input_bytes,
        "fixture did not exercise the growth fallback: input={}, output={}",
        result.report.input_bytes,
        result.report.output_bytes
    );
    assert!(!result.report.used_original);
    assert_eq!(result.report.compatibility_repairs, 1);
    let output = Document::load_mem(&result.bytes).unwrap();
    let page_id = *output.get_pages().values().next().unwrap();
    let page = output.get_dictionary(page_id).unwrap();
    let annots_object = page.get(b"Annots").unwrap().clone();
    let annots = output
        .dereference(&annots_object)
        .unwrap()
        .1
        .as_array()
        .unwrap();
    assert_eq!(annots.len(), 1);
}

#[test]
fn valid_widget_appearance_states_are_retained() {
    let result = compress(
        &widget_fixture(false),
        &Options {
            keep_if_larger: true,
            ..Options::default()
        },
    )
    .expect("widget states backed by Form XObjects are valid");
    let output = Document::load_mem(&result.bytes).unwrap();
    let page_id = *output.get_pages().values().next().unwrap();
    let page = output.get_dictionary(page_id).unwrap();
    let annots_object = page.get(b"Annots").unwrap().clone();
    let annots = output
        .dereference(&annots_object)
        .unwrap()
        .1
        .as_array()
        .unwrap();
    let widget_id = annots[0].as_reference().unwrap();
    let widget = output.get_dictionary(widget_id).unwrap();
    let ap = widget.get(b"AP").unwrap().clone();
    let ap = output.dereference(&ap).unwrap().1.as_dict().unwrap();
    let normal = ap.get(b"N").unwrap().clone();
    let normal = output.dereference(&normal).unwrap().1.as_dict().unwrap();
    assert!(matches!(
        output.dereference(normal.get(b"Off").unwrap()).unwrap().1,
        Object::Stream(_)
    ));
    assert!(matches!(
        output.dereference(normal.get(b"Yes").unwrap()).unwrap().1,
        Object::Stream(_)
    ));
}

#[test]
fn malformed_checkbox_appearance_is_rejected_without_inventing_off_state() {
    let error = compress(
        &widget_fixture(true),
        &Options {
            keep_if_larger: true,
            ..Options::default()
        },
    )
    .expect_err("a name is not a checkbox appearance stream");
    match error {
        Error::Unsupported(message) => {
            assert!(message.contains("malformed /AP /N /Off"), "{message}");
            assert!(
                message.contains("refusing to invent an appearance"),
                "{message}"
            );
        }
        other => panic!("unexpected error: {other:?}"),
    }
}
