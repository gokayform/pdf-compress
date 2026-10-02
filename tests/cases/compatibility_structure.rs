use crate::support::save;
use lopdf::{dictionary, Document, Object, Stream};
use pdf_compress::{compress, Options};

fn base_document() -> Document {
    let mut document = Document::with_version("1.7");
    let pages = document.new_object_id();
    let contents = document.add_object(Stream::new(dictionary! {}, b"q Q".to_vec()));
    let page = document.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages,
        "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        "Contents" => contents,
    });
    document.objects.insert(
        pages,
        dictionary! {"Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1}.into(),
    );
    let root = document.add_object(dictionary! {"Type" => "Catalog", "Pages" => pages});
    document.trailer.set("Root", root);
    document
}

fn name_tree_document(names: Vec<Object>, limits: Vec<Object>) -> Vec<u8> {
    let mut document = base_document();
    let leaf = document.add_object(dictionary! {"Names" => names, "Limits" => limits});
    let dests = document.add_object(dictionary! {"Kids" => vec![leaf.into()]});
    let names_root = document.add_object(dictionary! {"Dests" => dests});
    document.catalog_mut().unwrap().set("Names", names_root);
    save(document)
}

#[test]
fn unsorted_name_tree_leaf_is_repaired_and_limits_preserved() {
    let input = name_tree_document(
        vec![
            Object::string_literal("z"),
            Object::string_literal("last"),
            Object::string_literal("a"),
            Object::string_literal("first"),
        ],
        vec![Object::string_literal("a"), Object::string_literal("z")],
    );
    let result = compress(&input, &Options::default()).unwrap();
    assert!(result.report.compatibility_repairs > 0);
    assert!(!result.report.used_original);
    let document = Document::load_mem(&result.bytes).unwrap();
    let names_root = document
        .catalog()
        .unwrap()
        .get(b"Names")
        .unwrap()
        .as_reference()
        .unwrap();
    let dests = document
        .get_object(names_root)
        .unwrap()
        .as_dict()
        .unwrap()
        .get(b"Dests")
        .unwrap()
        .as_reference()
        .unwrap();
    let leaf = document
        .get_object(dests)
        .unwrap()
        .as_dict()
        .unwrap()
        .get(b"Kids")
        .unwrap()
        .as_array()
        .unwrap()[0]
        .as_reference()
        .unwrap();
    let values = document
        .get_object(leaf)
        .unwrap()
        .as_dict()
        .unwrap()
        .get(b"Names")
        .unwrap()
        .as_array()
        .unwrap();
    assert_eq!(values[0].as_str().unwrap(), b"a");
    assert_eq!(values[2].as_str().unwrap(), b"z");
}

#[test]
fn duplicate_name_tree_keys_are_rejected() {
    let input = name_tree_document(
        vec![
            Object::string_literal("same"),
            Object::string_literal("one"),
            Object::string_literal("same"),
            Object::string_literal("two"),
        ],
        vec![
            Object::string_literal("same"),
            Object::string_literal("same"),
        ],
    );
    let error = compress(&input, &Options::default()).unwrap_err();
    assert!(error.to_string().contains("duplicate key"), "{error}");
}

#[test]
fn indirect_string_keys_in_name_trees_are_valid() {
    let mut document = base_document();
    let z = document.add_object(Object::string_literal("z"));
    let a = document.add_object(Object::string_literal("a"));
    let leaf = document.add_object(dictionary! {
        "Names" => vec![
            z.into(),
            Object::string_literal("last"),
            a.into(),
            Object::string_literal("first"),
        ]
    });
    let dests = document.add_object(dictionary! {"Kids" => vec![leaf.into()]});
    let names_root = document.add_object(dictionary! {"Dests" => dests});
    document.catalog_mut().unwrap().set("Names", names_root);

    let result = compress(&save(document), &Options::default()).unwrap();
    assert!(result.report.compatibility_repairs > 0);
}

#[test]
fn cyclic_and_malformed_name_tree_shapes_are_rejected() {
    let mut cyclic = base_document();
    let root_id = cyclic.new_object_id();
    cyclic.objects.insert(
        root_id,
        dictionary! {"Kids" => vec![Object::Reference(root_id)]}.into(),
    );
    let names = cyclic.add_object(dictionary! {"Dests" => root_id});
    cyclic.catalog_mut().unwrap().set("Names", names);
    assert!(compress(&save(cyclic), &Options::default())
        .unwrap_err()
        .to_string()
        .contains("cycle"));

    let malformed = name_tree_document(
        vec![Object::string_literal("a"), Object::string_literal("value")],
        vec![Object::string_literal("a")],
    );
    let mut document = Document::load_mem(&malformed).unwrap();
    let names_root = document
        .catalog()
        .unwrap()
        .get(b"Names")
        .unwrap()
        .as_reference()
        .unwrap();
    let dests = document
        .get_object(names_root)
        .unwrap()
        .as_dict()
        .unwrap()
        .get(b"Dests")
        .unwrap()
        .as_reference()
        .unwrap();
    let leaf = document
        .get_object(dests)
        .unwrap()
        .as_dict()
        .unwrap()
        .get(b"Kids")
        .unwrap()
        .as_array()
        .unwrap()[0]
        .as_reference()
        .unwrap();
    document
        .get_object_mut(leaf)
        .unwrap()
        .as_dict_mut()
        .unwrap()
        .set("Kids", vec![leaf.into()]);
    assert!(compress(&save(document), &Options::default())
        .unwrap_err()
        .to_string()
        .contains("both leaf values"));
}

#[test]
fn invalid_media_box_is_rejected_but_invalid_crop_box_is_preserved() {
    let mut media_zero = base_document();
    let page = *media_zero.get_pages().get(&1).unwrap();
    media_zero
        .get_object_mut(page)
        .unwrap()
        .as_dict_mut()
        .unwrap()
        .set("MediaBox", vec![0.into(), 0.into(), 0.into(), 792.into()]);
    let error = compress(&save(media_zero), &Options::default()).unwrap_err();
    assert!(error.to_string().contains("MediaBox"), "{error}");

    let mut crop_zero = base_document();
    let page = *crop_zero.get_pages().get(&1).unwrap();
    crop_zero
        .get_object_mut(page)
        .unwrap()
        .as_dict_mut()
        .unwrap()
        .set("CropBox", vec![0.into(), 0.into(), 0.into(), 792.into()]);
    let result = compress(&save(crop_zero), &Options::default()).unwrap();
    let output = Document::load_mem(&result.bytes).unwrap();
    let page = *output.get_pages().get(&1).unwrap();
    assert_eq!(
        output
            .get_dictionary(page)
            .unwrap()
            .get(b"CropBox")
            .unwrap(),
        &Object::Array(vec![0.into(), 0.into(), 0.into(), 792.into()])
    );
}

fn type3_document(pattern_font_is_type3: bool) -> Vec<u8> {
    let mut document = Document::with_version("1.7");
    let pages = document.new_object_id();
    let font_a = document.new_object_id();
    let font_b = document.new_object_id();
    let pattern = document.new_object_id();
    let plain_font = document.add_object(
        dictionary! {"Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica"},
    );

    let rect = document.add_object(Stream::new(
        dictionary! {},
        b"% Type3 glyph with a standard d0 operator\n1000 0 d0 BT /FType3B 50 Tf (ccc) Tj ET"
            .to_vec(),
    ));
    let triangle = document.add_object(Stream::new(
        dictionary! {},
        b"1000 0 d0 0 0 m 375 750 l 750 0 l s".to_vec(),
    ));
    let inside = document.add_object(Stream::new(
        dictionary! {},
        b"900 0 d0\n% select a pattern before text painting\n/Pattern cs /P1 scn BT /F1 60 Tf (ab) Tj ET".to_vec(),
    ));
    document.objects.insert(
        pattern,
        Stream::new(
            dictionary! {
                "Type" => "Pattern",
                "PatternType" => 1,
                "Resources" => dictionary! {"Font" => dictionary! {
                    "CyclicFont" => if pattern_font_is_type3 {
                        Object::Reference(font_a)
                    } else {
                        Object::Reference(plain_font)
                    }
                }},
                "BBox" => vec![0.into(), 0.into(), 60.into(), 60.into()],
                "Matrix" => vec![0.9.into(), 0.into(), 0.into(), 0.9.into(), 0.into(), 0.into()],
                "XStep" => 55,
                "YStep" => 32,
            },
            b"BT /CyclicFont 4 Tf (ba) Tj ET".to_vec(),
        )
        .into(),
    );
    document.objects.insert(
        font_b,
        dictionary! {
            "Type" => "Font",
            "Subtype" => "Type3",
            "Name" => "FType3B",
            "FirstChar" => 99,
            "LastChar" => 99,
            "Widths" => vec![900.into()],
            "FontBBox" => vec![0.into(), 0.into(), 750.into(), 750.into()],
            "FontMatrix" => vec![0.004.into(), 0.into(), 0.into(), 0.004.into(), 0.into(), 0.into()],
            "Encoding" => dictionary! {
                "Type" => "Encoding",
                "Differences" => vec![Object::Integer(99), Object::Name(b"fontinside".to_vec())]
            },
            "CharProcs" => dictionary! {"fontinside" => inside},
            "Resources" => dictionary! {
                "Font" => dictionary! {"F1" => plain_font},
                "Pattern" => dictionary! {"P1" => pattern}
            }
        }.into(),
    );
    document.objects.insert(
        font_a,
        dictionary! {
            "Type" => "Font",
            "Subtype" => "Type3",
            "Name" => "FType3A",
            "FirstChar" => 97,
            "LastChar" => 98,
            "Widths" => vec![1000.into(), 1000.into()],
            "FontBBox" => vec![0.into(), 0.into(), 750.into(), 750.into()],
            "FontMatrix" => vec![0.01.into(), 0.into(), 0.into(), 0.01.into(), 0.into(), 0.into()],
            "Encoding" => dictionary! {
                "Type" => "Encoding",
                "Differences" => vec![
                    Object::Integer(97),
                    Object::Name(b"rect".to_vec()),
                    Object::Name(b"triangle".to_vec())
                ]
            },
            "CharProcs" => dictionary! {"rect" => rect, "triangle" => triangle},
            "Resources" => dictionary! {"Font" => dictionary! {"FType3B" => font_b}}
        }
        .into(),
    );
    let page_content = document.add_object(Stream::new(
        dictionary! {},
        b"BT /FType3A 20 Tf (ab) Tj ET".to_vec(),
    ));
    let page = document.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages,
        "MediaBox" => vec![0.into(), 0.into(), 600.into(), 840.into()],
        "Resources" => dictionary! {"Font" => dictionary! {"FType3A" => font_a}},
        "Contents" => page_content,
    });
    document.objects.insert(
        pages,
        dictionary! {"Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1}.into(),
    );
    let root = document.add_object(dictionary! {"Type" => "Catalog", "Pages" => pages});
    document.trailer.set("Root", root);
    save(document)
}

#[test]
fn executed_type3_cycle_is_rejected_but_nested_noncycle_is_accepted() {
    let cycle_error = compress(&type3_document(true), &Options::default()).unwrap_err();
    assert!(
        cycle_error.to_string().contains("recursive Type3"),
        "{cycle_error}"
    );
    compress(&type3_document(false), &Options::default()).unwrap();
}

fn self_recursive_type3_document(filter: &str, char_proc: Vec<u8>) -> Vec<u8> {
    let mut document = Document::with_version("1.7");
    let pages = document.new_object_id();
    let font = document.new_object_id();
    let glyph = document.add_object(Stream::new(dictionary! {"Filter" => filter}, char_proc));
    document.objects.insert(
        font,
        dictionary! {
            "Type" => "Font",
            "Subtype" => "Type3",
            "FirstChar" => 65,
            "LastChar" => 65,
            "Widths" => vec![1000.into()],
            "FontBBox" => vec![0.into(), 0.into(), 750.into(), 750.into()],
            "FontMatrix" => vec![0.001.into(), 0.into(), 0.into(), 0.001.into(), 0.into(), 0.into()],
            "Encoding" => dictionary! {
                "Type" => "Encoding",
                "Differences" => vec![Object::Integer(65), Object::Name(b"A".to_vec())]
            },
            "CharProcs" => dictionary! {"A" => glyph},
            "Resources" => dictionary! {"Font" => dictionary! {"F3" => font}}
        }
        .into(),
    );
    let page_content = document.add_object(Stream::new(
        dictionary! {},
        b"BT /F3 24 Tf 100 700 Td (A) Tj ET".to_vec(),
    ));
    let page = document.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages,
        "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        "Resources" => dictionary! {"Font" => dictionary! {"F3" => font}},
        "Contents" => page_content,
    });
    document.objects.insert(
        pages,
        dictionary! {"Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1}.into(),
    );
    let root = document.add_object(dictionary! {"Type" => "Catalog", "Pages" => pages});
    document.trailer.set("Root", root);
    save(document)
}

#[test]
fn type3_cycle_is_rejected_through_full_and_abbreviated_flate_filter() {
    use flate2::{write::ZlibEncoder, Compression};
    use std::io::Write;

    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(b"1000 0 0 0 750 750 d1 /F3 1 Tf (A) Tj")
        .unwrap();
    let encoded = encoder.finish().unwrap();
    for filter in ["FlateDecode", "Fl"] {
        let input = self_recursive_type3_document(filter, encoded.clone());
        let error = compress(&input, &Options::default()).unwrap_err();
        assert!(
            error.to_string().contains("recursive Type3"),
            "/{filter}: {error}"
        );
    }

    let corrupt = self_recursive_type3_document("Fl", b"not zlib data".to_vec());
    compress(&corrupt, &Options::default()).unwrap();
}

#[test]
fn separate_known_name_trees_allow_duplicate_keys_and_private_names_are_untouched() {
    let mut document = base_document();
    let first_leaf = document.add_object(dictionary! {
        "Names" => vec![
            Object::string_literal("same"),
            Object::string_literal("first"),
            Object::string_literal("alpha"),
            Object::string_literal("a"),
        ],
        "Limits" => vec![Object::string_literal("alpha"), Object::string_literal("same")],
    });
    let second_leaf = document.add_object(dictionary! {
        "Names" => vec![
            Object::string_literal("same"),
            Object::string_literal("second"),
            Object::string_literal("beta"),
            Object::string_literal("b"),
        ],
        "Limits" => vec![Object::string_literal("beta"), Object::string_literal("same")],
    });
    let names_root = document.add_object(dictionary! {
        "Dests" => first_leaf,
        "EmbeddedFiles" => second_leaf,
    });
    let private_names = document.add_object(dictionary! {
        "Names" => vec![
            Object::string_literal("z"),
            Object::string_literal("private-z"),
            Object::string_literal("a"),
            Object::string_literal("private-a"),
        ]
    });
    let page = *document.get_pages().get(&1).unwrap();
    document
        .get_object_mut(page)
        .unwrap()
        .as_dict_mut()
        .unwrap()
        .set("PrivateData", private_names);
    document.catalog_mut().unwrap().set("Names", names_root);

    let result = compress(&save(document), &Options::default()).unwrap();
    assert!(result.report.compatibility_repairs >= 2);
    let output = Document::load_mem(&result.bytes).unwrap();
    let private_names = output
        .get_pages()
        .get(&1)
        .and_then(|page| output.get_object(*page).ok())
        .and_then(|object| object.as_dict().ok())
        .and_then(|page| page.get(b"PrivateData").ok())
        .and_then(|object| object.as_reference().ok())
        .and_then(|id| output.get_object(id).ok())
        .and_then(|object| object.as_dict().ok())
        .and_then(|dictionary| dictionary.get(b"Names").ok())
        .and_then(|object| object.as_array().ok())
        .unwrap();
    assert_eq!(private_names[0].as_str().unwrap(), b"z");

    let names_root = output
        .catalog()
        .unwrap()
        .get(b"Names")
        .unwrap()
        .as_reference()
        .unwrap();
    let names = output.get_object(names_root).unwrap().as_dict().unwrap();
    for (key, first_key) in [
        (b"Dests".as_slice(), b"alpha".as_slice()),
        (b"EmbeddedFiles".as_slice(), b"beta".as_slice()),
    ] {
        let leaf = names.get(key).unwrap().as_reference().unwrap();
        let values = output
            .get_object(leaf)
            .unwrap()
            .as_dict()
            .unwrap()
            .get(b"Names")
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(values[0].as_str().unwrap(), first_key);
        assert_eq!(values[2].as_str().unwrap(), b"same");
    }
}

#[test]
fn direct_and_empty_number_trees_are_handled_without_limits_panic() {
    let mut direct = base_document();
    direct.catalog_mut().unwrap().set(
        "PageLabels",
        dictionary! {
            "Nums" => vec![
                2.into(), dictionary! {"S" => "r"}.into(),
                0.into(), dictionary! {"S" => "D"}.into(),
            ]
        },
    );
    let result = compress(&save(direct), &Options::default()).unwrap();
    assert!(result.report.compatibility_repairs > 0);
    let output = Document::load_mem(&result.bytes).unwrap();
    let nums = output
        .catalog()
        .unwrap()
        .get(b"PageLabels")
        .unwrap()
        .as_dict()
        .unwrap()
        .get(b"Nums")
        .unwrap()
        .as_array()
        .unwrap();
    assert_eq!(nums[0].as_i64().unwrap(), 0);

    let mut empty = base_document();
    empty
        .catalog_mut()
        .unwrap()
        .set("PageLabels", dictionary! {"Nums" => Vec::<Object>::new()});
    compress(&save(empty), &Options::default()).unwrap();
}
