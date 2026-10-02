//! Conservative annotation compatibility checks and repairs.
//!
//! Annotation dictionaries are part of the interactive PDF model.  This module
//! deliberately does not flatten annotations, synthesize popup windows, or
//! discard an annotation whose appearance is difficult to interpret.  It only
//! removes `null` placeholders from page `/Annots` arrays and canonicalizes a
//! page-level null `/Annots` value to an absent optional key.  A widget
//! appearance that is not an actual appearance stream is rejected because
//! choosing a replacement would change what a viewer displays.

use std::collections::HashSet;

use lopdf::{Dictionary, Document, Object, ObjectId};

use crate::{Error, Report, MAX_REFERENCE_CHAIN};

/// Apply compatibility-safe annotation normalization and validate widget
/// appearance objects.
///
/// A page annotation array is defined as an array of annotation references;
/// `null` entries (including references that resolve to null) are accepted by
/// some permissive readers but make other PDF consumers fail while iterating
/// the page.  Removing those placeholders does not remove an annotation or
/// alter its identity.  PDF 32000-1:2008 7.3.7 also defines a null dictionary
/// value as equivalent to an absent key, so a page-level `/Annots null` and an
/// annotation `/AP null` are accepted as absent optional entries, as is a null
/// entry in an `/N` state dictionary.  In contrast, a non-null widget
/// appearance state must resolve to a Form XObject stream.  A direct
/// name such as `/Off /Off` is ambiguous (there is no stream data to preserve),
/// so it is rejected rather than silently replaced with a made-up appearance.
pub(crate) fn normalize_annotations(
    document: &mut Document,
    report: &mut Report,
) -> Result<(), Error> {
    let page_ids: Vec<ObjectId> = document.get_pages().into_values().collect();
    let mut repaired = 0usize;

    for page_id in page_ids {
        let annots = document
            .objects
            .get(&page_id)
            .and_then(|object| object.as_dict().ok())
            .and_then(|page| page.get(b"Annots").ok())
            .cloned();

        let Some(annots) = annots else {
            continue;
        };

        match annots {
            Object::Array(_) => {
                let null_entries = {
                    let page = document.objects.get(&page_id).ok_or_else(|| {
                        Error::InvalidInput(format!("page object {page_id:?} disappeared"))
                    })?;
                    let array = page
                        .as_dict()
                        .map_err(|_| {
                            Error::InvalidInput(format!(
                                "page object {page_id:?} is not a dictionary"
                            ))
                        })?
                        .get(b"Annots")
                        .map_err(|_| {
                            Error::InvalidInput(format!(
                                "page object {page_id:?} has no /Annots array"
                            ))
                        })?;
                    null_annotation_entries(document, array)?
                };
                let page = document.objects.get_mut(&page_id).ok_or_else(|| {
                    Error::InvalidInput(format!("page object {page_id:?} disappeared"))
                })?;
                let page = page.as_dict_mut().map_err(|_| {
                    Error::InvalidInput(format!("page object {page_id:?} is not a dictionary"))
                })?;
                let array = page.get_mut(b"Annots").map_err(|_| {
                    Error::InvalidInput(format!("page object {page_id:?} has no /Annots array"))
                })?;
                repaired += remove_null_annotation_entries(
                    array,
                    &null_entries,
                    &format!("page {page_id:?}"),
                )?;
            }
            Object::Reference(array_id) => {
                if resolves_to_null(document, &annots)? {
                    let page = document.objects.get_mut(&page_id).ok_or_else(|| {
                        Error::InvalidInput(format!("page object {page_id:?} disappeared"))
                    })?;
                    let page = page.as_dict_mut().map_err(|_| {
                        Error::InvalidInput(format!("page object {page_id:?} is not a dictionary"))
                    })?;
                    page.remove(b"Annots");
                    repaired += 1;
                } else {
                    // Keep cycle detection scoped to this page's reference
                    // chain.  A well-formed document may share one indirect
                    // annotation array between pages; revisiting it from a
                    // second page is sharing, not a cycle.
                    let mut visited_arrays = HashSet::new();
                    repaired += remove_null_annotation_entries_from_reference(
                        document,
                        array_id,
                        &mut visited_arrays,
                    )?;
                }
            }
            Object::Null => {
                let page = document.objects.get_mut(&page_id).ok_or_else(|| {
                    Error::InvalidInput(format!("page object {page_id:?} disappeared"))
                })?;
                let page = page.as_dict_mut().map_err(|_| {
                    Error::InvalidInput(format!("page object {page_id:?} is not a dictionary"))
                })?;
                // PDF 32000-1:2008 7.3.7 defines a null dictionary value as
                // equivalent to an absent key.  Removing this optional page
                // entry makes that equivalence explicit for strict readers.
                page.remove(b"Annots");
                repaired += 1;
            }
            other => {
                return Err(Error::InvalidInput(format!(
                    "page object {page_id:?} has a non-array /Annots value ({})",
                    other.enum_variant()
                )));
            }
        }
    }

    if repaired > 0 {
        report.compatibility_repairs = report
            .compatibility_repairs
            .checked_add(repaired)
            .ok_or_else(|| Error::LimitExceeded("annotation repair count overflow".into()))?;
        report.warnings.push(format!(
            "removed {repaired} null placeholder(s) from page /Annots arrays"
        ));
    }

    validate_widget_appearances(document)
}

fn null_annotation_entries(document: &Document, object: &Object) -> Result<Vec<bool>, Error> {
    let entries = object
        .as_array()
        .map_err(|_| Error::InvalidInput("page /Annots value is not an array".into()))?;
    entries
        .iter()
        .map(|entry| resolves_to_null(document, entry))
        .collect()
}

fn remove_null_annotation_entries(
    object: &mut Object,
    null_entries: &[bool],
    location: &str,
) -> Result<usize, Error> {
    let entries = object
        .as_array_mut()
        .map_err(|_| Error::InvalidInput(format!("{location} /Annots value is not an array")))?;
    if entries.len() != null_entries.len() {
        return Err(Error::InvalidInput(format!(
            "{location} /Annots array changed while it was being normalized"
        )));
    }
    let mut flags = null_entries.iter();
    let mut removed = 0usize;
    entries.retain(|_| {
        let remove = flags.next().copied().unwrap_or(false);
        if remove {
            removed += 1;
        }
        !remove
    });
    Ok(removed)
}

fn remove_null_annotation_entries_from_reference(
    document: &mut Document,
    mut object_id: ObjectId,
    visited: &mut HashSet<ObjectId>,
) -> Result<usize, Error> {
    loop {
        if visited.len() >= MAX_REFERENCE_CHAIN || !visited.insert(object_id) {
            return Err(Error::InvalidInput(format!(
                "cycle or overlong reference chain while resolving a page /Annots array at object {object_id:?}"
            )));
        }

        match document.objects.get(&object_id) {
            Some(Object::Reference(next)) => object_id = *next,
            Some(Object::Array(_)) => break,
            Some(Object::Null) => {
                return Ok(0);
            }
            Some(object) => {
                return Err(Error::InvalidInput(format!(
                    "page /Annots reference {object_id:?} resolves to {}, not an array",
                    object.enum_variant()
                )))
            }
            None => {
                return Err(Error::InvalidInput(format!(
                    "page /Annots references missing object {object_id:?}"
                )))
            }
        }
    }

    let null_entries = {
        let object = document.objects.get(&object_id).ok_or_else(|| {
            Error::InvalidInput(format!("page /Annots object {object_id:?} disappeared"))
        })?;
        null_annotation_entries(document, object)?
    };
    let object = document.objects.get_mut(&object_id).ok_or_else(|| {
        Error::InvalidInput(format!("page /Annots object {object_id:?} disappeared"))
    })?;
    remove_null_annotation_entries(object, &null_entries, &format!("object {object_id:?}"))
}

fn resolves_to_null(document: &Document, object: &Object) -> Result<bool, Error> {
    let mut current = object;
    let mut visited = HashSet::new();
    loop {
        match current {
            Object::Null => return Ok(true),
            Object::Reference(id) => {
                if visited.len() >= MAX_REFERENCE_CHAIN || !visited.insert(*id) {
                    return Err(Error::InvalidInput(format!(
                        "cycle or overlong reference chain while resolving annotation object {id:?}"
                    )));
                }
                current = document.objects.get(id).ok_or_else(|| {
                    Error::InvalidInput(format!(
                        "annotation reference {id:?} points to a missing object"
                    ))
                })?;
            }
            _ => return Ok(false),
        }
    }
}

fn validate_widget_appearances(document: &Document) -> Result<(), Error> {
    let widget_ids: Vec<ObjectId> = document
        .objects
        .iter()
        .filter_map(|(id, object)| {
            let dictionary = object.as_dict().ok()?;
            let subtype = dictionary.get(b"Subtype").ok()?.as_name().ok()?;
            (subtype == b"Widget").then_some(*id)
        })
        .collect();

    for widget_id in widget_ids {
        let appearance = document
            .objects
            .get(&widget_id)
            .and_then(|object| object.as_dict().ok())
            .and_then(|widget| widget.get(b"AP").ok());
        let Some(appearance) = appearance else {
            continue;
        };

        // A null dictionary value is equivalent to an absent key (PDF
        // 32000-1:2008 7.3.7).  In particular, `/AP null` must not be
        // mistaken for a malformed appearance that we should synthesize.
        if resolves_to_null(document, appearance)? {
            continue;
        }

        let appearance = resolve_dictionary(
            document,
            appearance,
            widget_id,
            "/AP",
            "appearance dictionary",
        )?;
        let Some(normal) = appearance.get(b"N").ok() else {
            // `/R` and `/D` may be supplied for specialized annotation
            // workflows.  There is nothing to validate when `/N` is absent.
            continue;
        };
        if resolves_to_null(document, normal)? {
            continue;
        }
        validate_normal_appearance(document, normal, widget_id)?;
    }

    Ok(())
}

fn validate_normal_appearance(
    document: &Document,
    normal: &Object,
    widget_id: ObjectId,
) -> Result<(), Error> {
    let normal = document
        .dereference(normal)
        .map_err(|error| {
            malformed_appearance(
                widget_id,
                "/AP /N",
                format!("could not be resolved: {error}"),
            )
        })?
        .1;

    match normal {
        Object::Stream(_) => Ok(()),
        Object::Dictionary(states) => {
            for (state, value) in states.iter() {
                let resolved = document
                    .dereference(value)
                    .map_err(|error| {
                        malformed_appearance(
                            widget_id,
                            &format!("/AP /N /{}", String::from_utf8_lossy(state)),
                            format!("could not be resolved: {error}"),
                        )
                    })?
                    .1;
                // A null state entry is an absent key (PDF 32000-1:2008 7.3.7).
                if matches!(resolved, Object::Null) {
                    continue;
                }
                if !matches!(resolved, Object::Stream(_)) {
                    return Err(malformed_appearance(
                        widget_id,
                        &format!("/AP /N /{}", String::from_utf8_lossy(state)),
                        format!(
                            "expected an appearance stream, found {}",
                            resolved.enum_variant()
                        ),
                    ));
                }
            }
            Ok(())
        }
        other => Err(malformed_appearance(
            widget_id,
            "/AP /N",
            format!(
                "expected a Form XObject stream or state dictionary, found {}",
                other.enum_variant()
            ),
        )),
    }
}

fn resolve_dictionary<'a>(
    document: &'a Document,
    object: &'a Object,
    widget_id: ObjectId,
    path: &str,
    expected: &str,
) -> Result<&'a Dictionary, Error> {
    let resolved = document
        .dereference(object)
        .map_err(|error| {
            malformed_appearance(widget_id, path, format!("could not be resolved: {error}"))
        })?
        .1;
    resolved.as_dict().map_err(|_| {
        malformed_appearance(
            widget_id,
            path,
            format!("expected {expected}, found {}", resolved.enum_variant()),
        )
    })
}

fn malformed_appearance(widget_id: ObjectId, path: &str, detail: String) -> Error {
    Error::Unsupported(format!(
        "widget annotation {widget_id:?} has malformed {path}: {detail}; refusing to invent an appearance"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::Stream;

    fn widget_document(normal: Object) -> Document {
        let mut document = Document::with_version("1.4");
        document.objects.insert(
            (4, 0),
            Object::Stream(Stream::new(Dictionary::new(), vec![])),
        );
        document.objects.insert((6, 0), Object::Null);
        document.objects.insert(
            (7, 0),
            Object::Dictionary(Dictionary::from_iter([
                (b"Subtype".to_vec(), Object::Name(b"Widget".to_vec())),
                (
                    b"AP".to_vec(),
                    Object::Dictionary(Dictionary::from_iter([(b"N".to_vec(), normal)])),
                ),
            ])),
        );
        document
    }

    fn states(entries: Vec<(&str, Object)>) -> Object {
        Object::Dictionary(Dictionary::from_iter(
            entries
                .into_iter()
                .map(|(name, value)| (name.as_bytes().to_vec(), value)),
        ))
    }

    #[test]
    fn null_appearance_states_are_treated_as_absent() {
        let document = widget_document(states(vec![
            ("On", Object::Reference((4, 0))),
            ("Off", Object::Null),
            ("Indirect", Object::Reference((6, 0))),
        ]));
        validate_widget_appearances(&document).unwrap();
    }

    #[test]
    fn non_stream_appearance_states_are_still_rejected() {
        for state in [Object::Name(b"Off".to_vec()), Object::Integer(5)] {
            let document = widget_document(states(vec![("Off", state)]));
            assert!(matches!(
                validate_widget_appearances(&document),
                Err(Error::Unsupported(_))
            ));
        }
        let document = widget_document(states(vec![("Off", Object::Reference((99, 0)))]));
        assert!(validate_widget_appearances(&document).is_err());
    }

    #[test]
    fn long_annots_reference_chain_is_an_error_not_recursion() {
        let mut document = Document::with_version("1.4");
        for id in 1..=10_000_u32 {
            document
                .objects
                .insert((id, 0), Object::Reference((id + 1, 0)));
        }
        document
            .objects
            .insert((10_001, 0), Object::Array(vec![Object::Null]));
        let mut visited = HashSet::new();
        assert!(
            remove_null_annotation_entries_from_reference(&mut document, (1, 0), &mut visited)
                .is_err()
        );
        assert!(resolves_to_null(&document, &Object::Reference((1, 0))).is_err());
    }
}
