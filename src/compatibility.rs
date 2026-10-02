//! Conservative repairs and checks for structures that permissive PDF readers
//! commonly repair while opening a file.  This pass runs before rewriting so
//! that a repaired name tree is serialized deliberately and malformed page
//! geometry or executable Type 3 cycles are rejected instead of being guessed.

use crate::{Error, Options, Report};
use lopdf::content::Content;
use lopdf::{Dictionary, Document, Object, ObjectId, Stream};
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;

const MAX_TREE_DEPTH: usize = 256;
const MAX_PAGE_ANCESTOR_DEPTH: usize = 256;
const MAX_EXECUTION_DEPTH: usize = 256;

/// Normalize structures whose repair is unambiguous and reject structures for
/// which a rewrite could change what a viewer executes.
pub(crate) fn normalize_and_validate(
    document: &mut Document,
    options: &Options,
    report: &mut Report,
) -> Result<(), Error> {
    normalize_name_and_number_trees(document, report)?;
    validate_page_boxes(document)?;
    validate_type3_execution(document, options.max_decoded_stream_bytes)?;
    Ok(())
}

fn compatibility_repair(report: &mut Report, message: impl Into<String>) {
    report.compatibility_repairs += 1;
    report.warnings.push(message.into());
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum TreeKind {
    Names,
    Nums,
}

impl TreeKind {
    fn key_name(self) -> &'static str {
        match self {
            Self::Names => "Names",
            Self::Nums => "Nums",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum TreeKey {
    Name(Vec<u8>),
    Number(i64),
}

#[derive(Debug)]
struct TreePlan {
    id: ObjectId,
    kind: TreeKind,
    pairs: Option<Vec<Object>>,
    kids: Option<Vec<Object>>,
    limits: Option<Vec<Object>>,
}

#[derive(Debug)]
enum TreeRoot {
    Indirect {
        id: ObjectId,
        kind: TreeKind,
    },
    Direct {
        owner: ObjectId,
        key: Vec<u8>,
        kind: TreeKind,
        value: Object,
    },
}

#[derive(Clone, Debug)]
struct TreeRange {
    first: TreeKey,
    last: TreeKey,
}

fn normalize_name_and_number_trees(
    document: &mut Document,
    report: &mut Report,
) -> Result<(), Error> {
    let roots = find_tree_roots(document);
    let mut plans = Vec::new();
    let mut path = Vec::new();
    let mut roots_seen = HashSet::new();
    let mut direct_trees = Vec::new();

    for root in roots {
        let identity = match &root {
            TreeRoot::Indirect { id, kind } => (*id, Vec::new(), *kind),
            TreeRoot::Direct {
                owner, key, kind, ..
            } => (*owner, key.clone(), *kind),
        };
        if !roots_seen.insert(identity) {
            continue;
        }
        let mut keys = BTreeMap::new();
        match root {
            TreeRoot::Indirect { id, kind } => {
                inspect_tree_node(document, id, kind, &mut path, &mut keys, &mut plans)?;
            }
            TreeRoot::Direct {
                owner,
                key,
                kind,
                value,
            } => {
                let temporary_id = document.new_object_id();
                document.objects.insert(temporary_id, value);
                inspect_tree_node(
                    document,
                    temporary_id,
                    kind,
                    &mut path,
                    &mut keys,
                    &mut plans,
                )?;
                direct_trees.push((temporary_id, owner, key));
            }
        }
    }

    let mut changed_ids = HashSet::new();
    for plan in &plans {
        if plan.pairs.is_some() || plan.kids.is_some() || plan.limits.is_some() {
            changed_ids.insert(plan.id);
        }
    }

    for plan in plans {
        let object = document.objects.get_mut(&plan.id).ok_or_else(|| {
            Error::InvalidInput(format!(
                "name/number tree references missing object {:?}",
                plan.id
            ))
        })?;
        let dictionary = object.as_dict_mut().map_err(|_| {
            Error::InvalidInput(format!(
                "name/number tree node {:?} is not a dictionary",
                plan.id
            ))
        })?;

        if let Some(pairs) = plan.pairs {
            dictionary.set(
                match plan.kind {
                    TreeKind::Names => "Names",
                    TreeKind::Nums => "Nums",
                },
                pairs,
            );
            compatibility_repair(
                report,
                format!(
                    "sorted unsorted {:?} tree leaf at object {:?}",
                    plan.kind, plan.id
                ),
            );
        }
        if let Some(kids) = plan.kids {
            dictionary.set("Kids", kids);
            compatibility_repair(
                report,
                format!("sorted name/number tree children at object {:?}", plan.id),
            );
        }
        if let Some(limits) = plan.limits {
            dictionary.set("Limits", limits);
            compatibility_repair(
                report,
                format!("updated name/number tree limits at object {:?}", plan.id),
            );
        }
    }
    for (temporary_id, owner, key) in direct_trees {
        let value = document.objects.remove(&temporary_id);
        if changed_ids.contains(&temporary_id) {
            let value = value.ok_or_else(|| {
                Error::InvalidInput(format!(
                    "temporary name/number tree object {:?} disappeared",
                    temporary_id
                ))
            })?;
            let owner_object = document.objects.get_mut(&owner).ok_or_else(|| {
                Error::InvalidInput(format!("tree owner object {:?} is missing", owner))
            })?;
            owner_object
                .as_dict_mut()
                .map_err(|_| {
                    Error::InvalidInput(format!("tree owner {:?} is not a dictionary", owner))
                })?
                .set(key, value);
        }
    }
    Ok(())
}

/// Find only the standard name/number-tree entry points.  Looking for every
/// dictionary containing `/Names` or `/Nums` is unsafe: those keys are legal
/// in private application dictionaries and have no tree semantics there.
fn find_tree_roots(document: &Document) -> Vec<TreeRoot> {
    let mut roots = Vec::new();
    let Some(catalog_id) = document
        .trailer
        .get(b"Root")
        .ok()
        .and_then(|value| value.as_reference().ok())
    else {
        return roots;
    };
    let Some(catalog) = document
        .objects
        .get(&catalog_id)
        .and_then(object_dictionary)
    else {
        return roots;
    };

    if let Ok(value) = catalog.get(b"Names") {
        collect_names_container(document, catalog_id, b"Names", value, &mut roots);
    }
    if let Ok(value) = catalog.get(b"Dests") {
        add_tree_root(
            document,
            catalog_id,
            b"Dests",
            value,
            TreeKind::Names,
            &mut roots,
        );
    }
    if let Ok(value) = catalog.get(b"PageLabels") {
        add_tree_root(
            document,
            catalog_id,
            b"PageLabels",
            value,
            TreeKind::Nums,
            &mut roots,
        );
    }

    if let Ok(struct_root_value) = catalog.get(b"StructTreeRoot") {
        let Some(struct_root_id) = struct_root_value.as_reference().ok() else {
            return roots;
        };
        let Some(struct_root) = document
            .objects
            .get(&struct_root_id)
            .and_then(object_dictionary)
        else {
            return roots;
        };
        if let Ok(value) = struct_root.get(b"ParentTree") {
            add_tree_root(
                document,
                struct_root_id,
                b"ParentTree",
                value,
                TreeKind::Nums,
                &mut roots,
            );
        }
        if let Ok(value) = struct_root.get(b"IDTree") {
            add_tree_root(
                document,
                struct_root_id,
                b"IDTree",
                value,
                TreeKind::Names,
                &mut roots,
            );
        }
    }
    roots
}

fn collect_names_container(
    document: &Document,
    owner: ObjectId,
    key: &[u8],
    value: &Object,
    roots: &mut Vec<TreeRoot>,
) {
    let Some(container) = resolve_dictionary(document, value) else {
        add_tree_root(document, owner, key, value, TreeKind::Names, roots);
        return;
    };
    if is_tree_candidate(container, TreeKind::Names) {
        add_tree_root(document, owner, key, value, TreeKind::Names, roots);
        return;
    }
    // A catalog /Names dictionary is a container whose entries (Dests,
    // EmbeddedFiles, JavaScript, ...) are each separate name trees.
    for (entry_key, entry_value) in container.iter() {
        if entry_value.as_reference().is_ok() {
            add_tree_root(
                document,
                owner,
                entry_key,
                entry_value,
                TreeKind::Names,
                roots,
            );
        }
    }
}

fn add_tree_root(
    document: &Document,
    owner: ObjectId,
    key: &[u8],
    value: &Object,
    kind: TreeKind,
    roots: &mut Vec<TreeRoot>,
) {
    if let Ok(id) = value.as_reference() {
        let Some(dictionary) = document.objects.get(&id).and_then(object_dictionary) else {
            return;
        };
        if is_tree_candidate(dictionary, kind) && !is_empty_tree_leaf(dictionary, kind) {
            roots.push(TreeRoot::Indirect { id, kind });
        }
        return;
    }
    let Some(dictionary) = object_dictionary(value) else {
        return;
    };
    if is_tree_candidate(dictionary, kind) && !is_empty_tree_leaf(dictionary, kind) {
        roots.push(TreeRoot::Direct {
            owner,
            key: key.to_vec(),
            kind,
            value: value.clone(),
        });
    }
}

fn is_tree_candidate(dictionary: &Dictionary, kind: TreeKind) -> bool {
    dictionary.has(kind.key_name().as_bytes()) || dictionary.has(b"Kids")
}

fn is_empty_tree_leaf(dictionary: &Dictionary, kind: TreeKind) -> bool {
    dictionary
        .get(kind.key_name().as_bytes())
        .ok()
        .and_then(|object| object.as_array().ok())
        .is_some_and(|values| values.is_empty())
        && !dictionary.has(b"Kids")
}

fn inspect_tree_node(
    document: &Document,
    id: ObjectId,
    kind: TreeKind,
    path: &mut Vec<ObjectId>,
    keys: &mut BTreeMap<TreeKey, Object>,
    plans: &mut Vec<TreePlan>,
) -> Result<TreeRange, Error> {
    if path.len() >= MAX_TREE_DEPTH {
        return Err(Error::InvalidInput(
            "name/number tree exceeds the supported nesting depth".to_string(),
        ));
    }
    if path.contains(&id) {
        return Err(Error::InvalidInput(format!(
            "cycle in name/number tree at object {id:?}"
        )));
    }
    let dictionary = document
        .objects
        .get(&id)
        .and_then(object_dictionary)
        .ok_or_else(|| {
            Error::InvalidInput(format!("name/number tree node {id:?} is not a dictionary"))
        })?;

    let has_names = dictionary.has(b"Names");
    let has_nums = dictionary.has(b"Nums");
    let values_key = match kind {
        TreeKind::Names if has_names && !has_nums => b"Names".as_slice(),
        TreeKind::Nums if has_nums && !has_names => b"Nums".as_slice(),
        TreeKind::Names if has_nums => {
            return Err(Error::InvalidInput(format!(
                "name tree node {id:?} contains /Nums"
            )))
        }
        TreeKind::Nums if has_names => {
            return Err(Error::InvalidInput(format!(
                "number tree node {id:?} contains /Names"
            )))
        }
        _ => &[][..],
    };
    let has_values = !values_key.is_empty();
    let kids = dictionary.get(b"Kids").ok();
    if has_values && kids.is_some() {
        return Err(Error::InvalidInput(format!(
            "name/number tree node {id:?} has both leaf values and /Kids"
        )));
    }
    if !has_values && kids.is_none() {
        return Err(Error::InvalidInput(format!(
            "name/number tree node {id:?} has neither leaf values nor /Kids"
        )));
    }

    path.push(id);
    let result = if has_values {
        inspect_tree_leaf(document, id, kind, dictionary, values_key, keys, plans)
    } else {
        inspect_tree_children(document, id, kind, dictionary, path, keys, plans)
    };
    path.pop();
    result
}

fn inspect_tree_leaf(
    document: &Document,
    id: ObjectId,
    kind: TreeKind,
    dictionary: &Dictionary,
    values_key: &[u8],
    keys: &mut BTreeMap<TreeKey, Object>,
    plans: &mut Vec<TreePlan>,
) -> Result<TreeRange, Error> {
    let values = dictionary
        .get(values_key)
        .and_then(Object::as_array)
        .map_err(|_| {
            Error::InvalidInput(format!("tree leaf {id:?} has invalid /{}", kind.key_name()))
        })?;
    if values.is_empty() || values.len() % 2 != 0 {
        return Err(Error::InvalidInput(format!(
            "tree leaf {id:?} has an odd or empty /{} array",
            kind.key_name()
        )));
    }

    let mut pairs = Vec::with_capacity(values.len());
    let mut keyed = Vec::with_capacity(values.len() / 2);
    for pair in values.chunks_exact(2) {
        let [key_object, value_object] = pair else {
            continue;
        };
        let key = tree_key(document, kind, key_object)
            .ok_or_else(|| Error::InvalidInput(format!("tree leaf {id:?} has an invalid key")))?;
        if let Some(existing) = keys.get(&key) {
            if existing == value_object {
                // Repeating an identical key/value pair is a harmless
                // producer defect.  Coalesce it while retaining the value.
                continue;
            }
            return Err(Error::InvalidInput(format!(
                "duplicate key in name/number tree at object {id:?}"
            )));
        }
        keys.insert(key.clone(), value_object.clone());
        keyed.push((key, key_object.clone(), value_object.clone()));
    }
    keyed.sort_by(|left, right| left.0.cmp(&right.0));
    for (_, key, value) in &keyed {
        pairs.push(key.clone());
        pairs.push(value.clone());
    }
    // Every pair of this leaf may duplicate one already seen in another leaf.
    let (Some(first), Some(last)) = (keyed.first(), keyed.last()) else {
        return Err(Error::InvalidInput(format!(
            "tree leaf {id:?} only repeats keys of other leaves"
        )));
    };
    let range = TreeRange {
        first: first.0.clone(),
        last: last.0.clone(),
    };

    let pairs_changed = keyed
        .iter()
        .flat_map(|(_, key, value)| [key, value])
        .ne(values.iter());
    let limits = checked_limits(document, dictionary, kind, id, &range)?;
    plans.push(TreePlan {
        id,
        kind,
        pairs: pairs_changed.then_some(pairs),
        kids: None,
        limits,
    });
    Ok(range)
}

fn inspect_tree_children(
    document: &Document,
    id: ObjectId,
    kind: TreeKind,
    dictionary: &Dictionary,
    path: &mut Vec<ObjectId>,
    keys: &mut BTreeMap<TreeKey, Object>,
    plans: &mut Vec<TreePlan>,
) -> Result<TreeRange, Error> {
    let kids = dictionary
        .get(b"Kids")
        .and_then(Object::as_array)
        .map_err(|_| Error::InvalidInput(format!("tree node {id:?} has invalid /Kids")))?;
    if kids.is_empty() {
        return Err(Error::InvalidInput(format!(
            "tree node {id:?} has an empty /Kids array"
        )));
    }

    let mut child_ranges = Vec::with_capacity(kids.len());
    for child in kids {
        let child_id = child.as_reference().map_err(|_| {
            Error::InvalidInput(format!("tree node {id:?} has a non-reference /Kids entry"))
        })?;
        let range = inspect_tree_node(document, child_id, kind, path, keys, plans)?;
        child_ranges.push((range, child.clone()));
    }
    child_ranges.sort_by(|left, right| left.0.first.cmp(&right.0.first));
    for pair in child_ranges.windows(2) {
        if let [previous, next] = pair {
            if previous.0.last >= next.0.first {
                return Err(Error::InvalidInput(format!(
                    "overlapping or duplicate ranges in name/number tree at object {id:?}"
                )));
            }
        }
    }
    let (Some(first), Some(last)) = (child_ranges.first(), child_ranges.last()) else {
        return Err(Error::InvalidInput(format!(
            "tree node {id:?} has no usable /Kids"
        )));
    };
    let range = TreeRange {
        first: first.0.first.clone(),
        last: last.0.last.clone(),
    };
    let sorted_kids: Vec<_> = child_ranges
        .iter()
        .map(|(_, child)| child.clone())
        .collect();
    let kids_changed = sorted_kids != *kids;
    let limits = checked_limits(document, dictionary, kind, id, &range)?;
    plans.push(TreePlan {
        id,
        kind,
        pairs: None,
        kids: kids_changed.then_some(sorted_kids),
        limits,
    });
    Ok(range)
}

/// Name-tree keys are strings; some producers write names instead, which
/// readers treat as the same byte sequence.
fn tree_key(document: &Document, kind: TreeKind, object: &Object) -> Option<TreeKey> {
    let object = resolve_object(document, object)?;
    match kind {
        TreeKind::Names => match object {
            Object::String(value, _) | Object::Name(value) => Some(TreeKey::Name(value.clone())),
            _ => None,
        },
        TreeKind::Nums => object.as_i64().ok().map(TreeKey::Number),
    }
}

fn checked_limits(
    document: &Document,
    dictionary: &Dictionary,
    kind: TreeKind,
    id: ObjectId,
    range: &TreeRange,
) -> Result<Option<Vec<Object>>, Error> {
    let Some(existing) = dictionary.get(b"Limits").ok() else {
        return Ok(None);
    };
    let limits = existing.as_array().map_err(|_| {
        Error::InvalidInput(format!("name/number tree node {id:?} has invalid /Limits"))
    })?;
    let [low, high] = limits.as_slice() else {
        return Err(Error::InvalidInput(format!(
            "name/number tree node {id:?} has an invalid /Limits array"
        )));
    };
    let (Some(low), Some(high)) = (
        tree_key(document, kind, low),
        tree_key(document, kind, high),
    ) else {
        return Err(Error::InvalidInput(format!(
            "name/number tree node {id:?} has invalid /Limits keys"
        )));
    };
    if low == range.first && high == range.last {
        Ok(None)
    } else {
        Ok(Some(vec![
            tree_key_object(&range.first),
            tree_key_object(&range.last),
        ]))
    }
}

fn tree_key_object(key: &TreeKey) -> Object {
    match key {
        TreeKey::Name(value) => Object::string_literal(value.clone()),
        TreeKey::Number(value) => Object::Integer(*value),
    }
}

fn object_dictionary(object: &Object) -> Option<&Dictionary> {
    match object {
        Object::Dictionary(dictionary) => Some(dictionary),
        Object::Stream(stream) => Some(&stream.dict),
        _ => None,
    }
}

fn resolve_dictionary<'a>(document: &'a Document, object: &'a Object) -> Option<&'a Dictionary> {
    let mut current = object;
    let mut seen = HashSet::new();
    loop {
        match current {
            Object::Reference(id) => {
                if !seen.insert(*id) {
                    return None;
                }
                current = document.objects.get(id)?;
            }
            _ => return object_dictionary(current),
        }
    }
}

fn validate_page_boxes(document: &Document) -> Result<(), Error> {
    // Viewers default a missing /MediaBox to Letter and fall back to the
    // MediaBox for an unusable /CropBox, so only a present MediaBox is checked.
    for (page_number, page_id) in document.get_pages() {
        if let Some(media) = inherited_page_value(document, page_id, b"MediaBox")? {
            validate_box(document, media, page_number, b"MediaBox")?;
        }
    }
    Ok(())
}

fn inherited_page_value<'a>(
    document: &'a Document,
    page_id: ObjectId,
    key: &[u8],
) -> Result<Option<&'a Object>, Error> {
    let mut current = page_id;
    let mut seen = HashSet::new();
    for _ in 0..MAX_PAGE_ANCESTOR_DEPTH {
        if !seen.insert(current) {
            return Err(Error::InvalidInput(format!(
                "cycle in page parent chain at object {current:?}"
            )));
        }
        let dictionary = document
            .objects
            .get(&current)
            .and_then(|object| object.as_dict().ok())
            .ok_or_else(|| {
                Error::InvalidInput(format!("page object {current:?} is not a dictionary"))
            })?;
        if let Ok(value) = dictionary.get(key) {
            // A null page attribute does not terminate inheritance; continue
            // with the parent as required by the page-tree rules.
            if !matches!(value, Object::Null) {
                return Ok(Some(value));
            }
        }
        let Some(parent) = dictionary
            .get(b"Parent")
            .ok()
            .and_then(|value| value.as_reference().ok())
        else {
            return Ok(None);
        };
        current = parent;
    }
    Err(Error::InvalidInput(
        "page parent chain exceeds the supported depth".to_string(),
    ))
}

fn validate_box(
    document: &Document,
    object: &Object,
    page_number: u32,
    name: &[u8],
) -> Result<(), Error> {
    let name = String::from_utf8_lossy(name);
    let object = resolve_object(document, object).ok_or_else(|| {
        Error::InvalidInput(format!("page {page_number} has an unresolved /{name}"))
    })?;
    let values = object
        .as_array()
        .map_err(|_| Error::InvalidInput(format!("page {page_number} has a non-array /{name}")))?;
    if values.len() != 4 {
        return Err(Error::InvalidInput(format!(
            "page {page_number} has a /{name} with {} coordinates",
            values.len()
        )));
    }
    let mut coordinates = [0.0_f64; 4];
    for (slot, value) in coordinates.iter_mut().zip(values.iter()) {
        let value = resolve_object(document, value).ok_or_else(|| {
            Error::InvalidInput(format!(
                "page {page_number} has an unresolved /{name} coordinate"
            ))
        })?;
        *slot = match value {
            Object::Integer(value) => *value as f64,
            Object::Real(value) => *value,
            _ => {
                return Err(Error::InvalidInput(format!(
                    "page {page_number} has a non-numeric /{name} coordinate"
                )))
            }
        };
        if !slot.is_finite() {
            return Err(Error::InvalidInput(format!(
                "page {page_number} has a non-finite /{name} coordinate"
            )));
        }
    }
    let [left, bottom, right, top] = coordinates;
    if right == left || top == bottom {
        return Err(Error::InvalidInput(format!(
            "page {page_number} has a zero-size /{name}"
        )));
    }
    Ok(())
}

fn resolve_object<'a>(document: &'a Document, object: &'a Object) -> Option<&'a Object> {
    let mut current = object;
    let mut seen = HashSet::new();
    loop {
        match current {
            Object::Reference(id) => {
                if !seen.insert(*id) {
                    return None;
                }
                current = document.objects.get(id)?;
            }
            _ => return Some(current),
        }
    }
}

/// Whole-pass ceilings for the Type 3 scan.  Each object is scanned at most
/// once, so these only bound documents that are genuinely huge.
const MAX_SCAN_BYTES: usize = 256 * 1024 * 1024;
const MAX_SCAN_OPERATIONS: usize = 20_000_000;

#[derive(Clone, Copy, Debug)]
struct PdfValue<'a> {
    id: Option<ObjectId>,
    object: &'a Object,
}

#[derive(Debug, Default)]
struct ResourceScope<'a> {
    fonts: BTreeMap<&'a [u8], PdfValue<'a>>,
    patterns: BTreeMap<&'a [u8], PdfValue<'a>>,
    xobjects: BTreeMap<&'a [u8], PdfValue<'a>>,
}

#[derive(Clone, Debug, Default)]
struct PaintState<'a> {
    font: Option<PdfValue<'a>>,
    nonstroking_pattern: Option<PdfValue<'a>>,
    stroking_pattern: Option<PdfValue<'a>>,
}

#[derive(Debug)]
struct Type3Info<'a> {
    key: usize,
    encoding: BTreeMap<u8, &'a [u8]>,
    charprocs: BTreeMap<&'a [u8], PdfValue<'a>>,
    resources: ResourceScope<'a>,
}

/// A form, pattern or glyph program always executes against its own resources
/// and a fresh graphics state, so a completed cycle-free scan of an object is
/// valid for every later caller and is recorded once.
struct Type3Scanner<'a> {
    document: &'a Document,
    fonts: HashMap<usize, Option<Rc<Type3Info<'a>>>>,
    active_glyphs: Vec<(usize, &'a [u8])>,
    active_patterns: Vec<ObjectId>,
    active_xobjects: Vec<ObjectId>,
    done_glyphs: HashSet<(usize, &'a [u8])>,
    done_patterns: HashSet<ObjectId>,
    done_xobjects: HashSet<ObjectId>,
    depth: usize,
    max_stream_bytes: usize,
    scanned_bytes: usize,
    scanned_operations: usize,
}

fn validate_type3_execution(
    document: &Document,
    max_decoded_stream_bytes: usize,
) -> Result<(), Error> {
    // This pass is only needed for documents that actually contain Type3
    // fonts.  Avoid decoding all page streams for ordinary PDFs.
    if !document.objects.values().any(|object| {
        object_dictionary(object)
            .and_then(|dictionary| dictionary.get(b"Subtype").ok())
            .and_then(|value| value.as_name().ok())
            == Some(b"Type3")
    }) {
        return Ok(());
    }
    let mut scanner = Type3Scanner {
        document,
        fonts: HashMap::new(),
        active_glyphs: Vec::new(),
        active_patterns: Vec::new(),
        active_xobjects: Vec::new(),
        done_glyphs: HashSet::new(),
        done_patterns: HashSet::new(),
        done_xobjects: HashSet::new(),
        depth: 0,
        max_stream_bytes: max_decoded_stream_bytes,
        scanned_bytes: 0,
        scanned_operations: 0,
    };
    for page_id in document.get_pages().values().copied() {
        let Some(page) = document
            .objects
            .get(&page_id)
            .and_then(|object| object.as_dict().ok())
        else {
            continue;
        };
        let scope = inherited_page_value(document, page_id, b"Resources")?
            .map(|value| scanner.resource_scope(value))
            .unwrap_or_default();
        if let Ok(contents) = page.get(b"Contents") {
            let mut state = PaintState::default();
            scanner.scan_page_contents(contents, &scope, &mut state)?;
        }
    }
    Ok(())
}

fn object_key(object: &Object) -> usize {
    // The document is immutable for the whole scan, so an object's address is
    // a stable identity even for direct (unnumbered) dictionaries.
    std::ptr::from_ref(object) as usize
}

impl<'a> Type3Scanner<'a> {
    fn resource_scope(&self, object: &'a Object) -> ResourceScope<'a> {
        let document = self.document;
        let Some(dictionary) = resolve_object(document, object).and_then(object_dictionary) else {
            return ResourceScope::default();
        };
        let mut scope = ResourceScope::default();
        for (key, target) in [
            (b"Font".as_slice(), &mut scope.fonts),
            (b"Pattern".as_slice(), &mut scope.patterns),
            (b"XObject".as_slice(), &mut scope.xobjects),
        ] {
            let Ok(value) = dictionary.get(key) else {
                continue;
            };
            let Some(entries) =
                resolve_value(document, value).and_then(|value| value.object.as_dict().ok())
            else {
                continue;
            };
            for (name, object) in entries.iter() {
                if let Some(value) = resolve_value(document, object) {
                    target.insert(name.as_slice(), value);
                }
            }
        }
        scope
    }

    fn charge_bytes(&mut self, bytes: usize) -> Result<(), Error> {
        self.scanned_bytes = self.scanned_bytes.saturating_add(bytes);
        if self.scanned_bytes > MAX_SCAN_BYTES {
            return Err(Error::LimitExceeded(format!(
                "Type3 execution scan exceeds {MAX_SCAN_BYTES} content bytes"
            )));
        }
        Ok(())
    }

    fn charge_operations(&mut self, operations: usize) -> Result<(), Error> {
        self.scanned_operations = self.scanned_operations.saturating_add(operations);
        if self.scanned_operations > MAX_SCAN_OPERATIONS {
            return Err(Error::LimitExceeded(format!(
                "Type3 execution scan exceeds {MAX_SCAN_OPERATIONS} operations"
            )));
        }
        Ok(())
    }

    /// Page `/Contents` is a stream or a flat array of streams.  Nested arrays
    /// are not valid and are ignored, which also keeps self-referencing arrays
    /// from recursing.
    fn scan_page_contents(
        &mut self,
        object: &'a Object,
        scope: &ResourceScope<'a>,
        state: &mut PaintState<'a>,
    ) -> Result<(), Error> {
        let document = self.document;
        let Some(value) = resolve_value(document, object) else {
            return Ok(());
        };
        match value.object {
            Object::Array(values) => {
                for entry in values {
                    if let Some(PdfValue {
                        object: Object::Stream(stream),
                        ..
                    }) = resolve_value(document, entry)
                    {
                        self.scan_stream(stream, scope, state)?;
                    }
                }
                Ok(())
            }
            Object::Stream(stream) => self.scan_stream(stream, scope, state),
            _ => Ok(()),
        }
    }

    /// Decode only the filters this scanner can interpret, and always enforce
    /// the caller's decoded-byte budget.  Unsupported filter chains and streams
    /// over the budget are deliberately skipped because a false cycle rejection
    /// is worse than a bounded false negative in this conservative
    /// compatibility pass.
    fn bounded_stream_content(&self, stream: &'a Stream) -> Option<Cow<'a, [u8]>> {
        let Some(filter) = stream.dict.get(b"Filter").ok() else {
            if stream.content.len() > self.max_stream_bytes {
                return None;
            }
            return Some(Cow::Borrowed(stream.content.as_slice()));
        };
        let filter = resolve_object(self.document, filter)?;
        let name = match filter {
            Object::Name(name) => name.as_slice(),
            Object::Array(filters) => {
                let [filter] = filters.as_slice() else {
                    return None;
                };
                resolve_object(self.document, filter)?.as_name().ok()?
            }
            _ => return None,
        };
        if !matches!(name, b"FlateDecode" | b"Fl") || self.has_predictor(stream) {
            return None;
        }
        // A corrupt or oversized stream cannot prove a cycle; leave it to the
        // reader.
        crate::decode_flate_bounded(&stream.content, self.max_stream_bytes)
            .ok()
            .map(Cow::Owned)
    }

    /// Predictor output is not content-stream syntax, so scanning it could
    /// only ever produce noise.
    fn has_predictor(&self, stream: &Stream) -> bool {
        let Some(params) = stream
            .dict
            .get(b"DecodeParms")
            .ok()
            .and_then(|params| resolve_object(self.document, params))
        else {
            return false;
        };
        let params = match params {
            Object::Array(entries) if entries.len() == 1 => entries
                .first()
                .and_then(|entry| resolve_object(self.document, entry)),
            Object::Array(_) => return true,
            other => Some(other),
        };
        match params {
            None | Some(Object::Null) => false,
            Some(Object::Dictionary(dict)) => {
                !matches!(dict.get(b"Predictor").ok(), None | Some(Object::Integer(1)))
            }
            Some(_) => true,
        }
    }

    fn scan_stream(
        &mut self,
        stream: &'a Stream,
        scope: &ResourceScope<'a>,
        state: &mut PaintState<'a>,
    ) -> Result<(), Error> {
        if self.depth >= MAX_EXECUTION_DEPTH {
            return Err(Error::InvalidInput(
                "Type3/content execution exceeds the supported depth".to_string(),
            ));
        }
        let Some(bytes) = self.bounded_stream_content(stream) else {
            return Ok(());
        };
        self.charge_bytes(bytes.len())?;
        let scanner_bytes = normalize_type3_content_tokens(&bytes);
        let Ok(content) = Content::decode(&scanner_bytes) else {
            return Ok(());
        };
        self.depth += 1;
        let result = self.scan_operations(&content.operations, scope, state);
        self.depth -= 1;
        result
    }

    fn scan_operations(
        &mut self,
        operations: &[lopdf::content::Operation],
        scope: &ResourceScope<'a>,
        state: &mut PaintState<'a>,
    ) -> Result<(), Error> {
        self.charge_operations(operations.len())?;
        let mut graphics_state = Vec::new();
        for operation in operations {
            match operation.operator.as_str() {
                "q" => graphics_state.push(state.clone()),
                "Q" => {
                    if let Some(saved) = graphics_state.pop() {
                        *state = saved;
                    }
                }
                "Tf" => {
                    if let Some(Object::Name(name)) = operation.operands.first() {
                        state.font = scope.fonts.get(name.as_slice()).copied();
                    }
                }
                "Tj" | "'" | "\"" => {
                    if let Some(text) = operation
                        .operands
                        .last()
                        .and_then(|value| value.as_str().ok())
                    {
                        self.scan_text(state.font, text)?;
                        if let Some(pattern) = state.nonstroking_pattern {
                            self.scan_pattern(pattern)?;
                        }
                    }
                }
                "TJ" => {
                    if let Some(Object::Array(items)) = operation.operands.last() {
                        for item in items {
                            if let Ok(text) = item.as_str() {
                                self.scan_text(state.font, text)?;
                            }
                        }
                        if let Some(pattern) = state.nonstroking_pattern {
                            self.scan_pattern(pattern)?;
                        }
                    }
                }
                "scn" => {
                    state.nonstroking_pattern = None;
                    if let Some(Object::Name(name)) = operation.operands.last() {
                        state.nonstroking_pattern = scope.patterns.get(name.as_slice()).copied();
                    }
                }
                "SCN" => {
                    state.stroking_pattern = None;
                    if let Some(Object::Name(name)) = operation.operands.last() {
                        state.stroking_pattern = scope.patterns.get(name.as_slice()).copied();
                    }
                }
                "cs" | "g" | "rg" | "k" => state.nonstroking_pattern = None,
                "CS" | "G" | "RG" | "K" => state.stroking_pattern = None,
                "f" | "F" | "f*" => {
                    if let Some(pattern) = state.nonstroking_pattern {
                        self.scan_pattern(pattern)?;
                    }
                }
                "S" | "s" => {
                    if let Some(pattern) = state.stroking_pattern {
                        self.scan_pattern(pattern)?;
                    }
                }
                "B" | "B*" | "b" | "b*" => {
                    if let Some(pattern) = state.nonstroking_pattern {
                        self.scan_pattern(pattern)?;
                    }
                    if let Some(pattern) = state.stroking_pattern {
                        self.scan_pattern(pattern)?;
                    }
                }
                "Do" => {
                    if let Some(Object::Name(name)) = operation.operands.last() {
                        if let Some(xobject) = scope.xobjects.get(name.as_slice()).copied() {
                            self.scan_xobject(xobject)?;
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn scan_text(&mut self, font: Option<PdfValue<'a>>, text: &[u8]) -> Result<(), Error> {
        let Some(font) = font else {
            return Ok(());
        };
        let Some(info) = self.type3_info(font) else {
            return Ok(());
        };
        self.charge_operations(text.len())?;
        for code in text {
            let Some(glyph) = info.encoding.get(code).copied() else {
                continue;
            };
            self.scan_glyph(&info, glyph)?;
        }
        Ok(())
    }

    fn scan_glyph(&mut self, font: &Type3Info<'a>, glyph: &'a [u8]) -> Result<(), Error> {
        let identity = (font.key, glyph);
        if self.done_glyphs.contains(&identity) {
            return Ok(());
        }
        if self.active_glyphs.contains(&identity) {
            return Err(Error::InvalidInput(format!(
                "recursive Type3 glyph execution detected at /{}",
                String::from_utf8_lossy(glyph)
            )));
        }
        let Some(value) = font.charprocs.get(glyph).copied() else {
            return Ok(());
        };
        let Object::Stream(stream) = value.object else {
            return Err(Error::InvalidInput(format!(
                "Type3 CharProc /{} is not a stream",
                String::from_utf8_lossy(glyph)
            )));
        };
        self.active_glyphs.push(identity);
        let mut state = PaintState::default();
        let result = self.scan_stream(stream, &font.resources, &mut state);
        self.active_glyphs.pop();
        if result.is_ok() {
            self.done_glyphs.insert(identity);
        }
        result
    }

    fn scan_pattern(&mut self, value: PdfValue<'a>) -> Result<(), Error> {
        let Some(id) = value.id else {
            return Ok(());
        };
        if self.done_patterns.contains(&id) {
            return Ok(());
        }
        if self.active_patterns.contains(&id) {
            return Err(Error::InvalidInput(format!(
                "recursive Pattern execution detected at object {id:?}"
            )));
        }
        let Object::Stream(stream) = value.object else {
            return Ok(());
        };
        self.active_patterns.push(id);
        let resources = stream
            .dict
            .get(b"Resources")
            .ok()
            .map(|value| self.resource_scope(value))
            .unwrap_or_default();
        let mut state = PaintState::default();
        let result = self.scan_stream(stream, &resources, &mut state);
        self.active_patterns.pop();
        if result.is_ok() {
            self.done_patterns.insert(id);
        }
        result
    }

    fn scan_xobject(&mut self, value: PdfValue<'a>) -> Result<(), Error> {
        let Some(id) = value.id else {
            return Ok(());
        };
        if self.done_xobjects.contains(&id) {
            return Ok(());
        }
        if self.active_xobjects.contains(&id) {
            return Err(Error::InvalidInput(format!(
                "recursive Form XObject execution detected at object {id:?}"
            )));
        }
        let Object::Stream(stream) = value.object else {
            return Ok(());
        };
        if stream.dict.get(b"Subtype").and_then(Object::as_name).ok() != Some(b"Form") {
            return Ok(());
        }
        self.active_xobjects.push(id);
        let resources = stream
            .dict
            .get(b"Resources")
            .ok()
            .map(|value| self.resource_scope(value))
            .unwrap_or_default();
        let mut state = PaintState::default();
        let result = self.scan_stream(stream, &resources, &mut state);
        self.active_xobjects.pop();
        if result.is_ok() {
            self.done_xobjects.insert(id);
        }
        result
    }

    fn type3_info(&mut self, font: PdfValue<'a>) -> Option<Rc<Type3Info<'a>>> {
        let key = object_key(font.object);
        if let Some(cached) = self.fonts.get(&key) {
            return cached.clone();
        }
        let info = self.build_type3_info(font, key).map(Rc::new);
        self.fonts.insert(key, info.clone());
        info
    }

    fn build_type3_info(&self, font: PdfValue<'a>, key: usize) -> Option<Type3Info<'a>> {
        let document = self.document;
        let dictionary = resolve_object(document, font.object).and_then(object_dictionary)?;
        if dictionary.get(b"Subtype").and_then(Object::as_name).ok() != Some(b"Type3") {
            return None;
        }
        let charprocs = dictionary
            .get(b"CharProcs")
            .ok()
            .and_then(|value| resolve_object(document, value))
            .and_then(|object| object.as_dict().ok())?;
        let mut charproc_values = BTreeMap::new();
        for (name, object) in charprocs.iter() {
            if let Some(value) = resolve_value(document, object) {
                charproc_values.insert(name.as_slice(), value);
            }
        }
        let encoding = parse_type3_encoding(document, dictionary)?;
        let resources = dictionary
            .get(b"Resources")
            .ok()
            .map(|value| self.resource_scope(value))
            .unwrap_or_default();
        Some(Type3Info {
            key,
            encoding,
            charprocs: charproc_values,
            resources,
        })
    }
}

fn parse_type3_encoding<'a>(
    document: &'a Document,
    dictionary: &'a Dictionary,
) -> Option<BTreeMap<u8, &'a [u8]>> {
    let encoding = dictionary.get(b"Encoding").ok()?;
    let encoding = resolve_object(document, encoding)?;
    let encoding = encoding.as_dict().ok()?;
    let differences = encoding.get(b"Differences").ok()?;
    let differences = resolve_object(document, differences)?.as_array().ok()?;
    let mut current = None;
    let mut result = BTreeMap::new();
    for item in differences {
        if let Ok(value) = item.as_i64() {
            current = u8::try_from(value).ok();
        } else if let Ok(name) = item.as_name() {
            let code = current?;
            result.insert(code, name);
            current = code.checked_add(1);
        } else {
            return None;
        }
    }
    Some(result)
}

/// lopdf's content parser accepts alphabetic operators, while PDF Type3
/// charprocs conventionally use the standard `d0`/`d1` operators.  It stops
/// at those operators because their names contain a digit.  Replace only
/// standalone `d0`/`d1` tokens with the harmless alphabetic operator `d` so
/// the scanner can continue to the following text/pattern operations.  Keep
/// comments, literal strings, and hex strings byte-for-byte intact.
fn normalize_type3_content_tokens(bytes: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    let mut literal_depth = 0usize;
    let mut literal_escape = false;
    let mut in_hex = false;
    let mut in_comment = false;
    while let Some(&byte) = bytes.get(index) {
        let next = bytes.get(index + 1).copied();
        if in_comment {
            index += 1;
            if byte == b'\r' || byte == b'\n' {
                output.push(byte);
                in_comment = false;
            }
            continue;
        }
        if literal_depth > 0 {
            output.push(byte);
            index += 1;
            if literal_escape {
                literal_escape = false;
            } else if byte == b'\\' {
                literal_escape = true;
            } else if byte == b'(' {
                literal_depth += 1;
            } else if byte == b')' {
                literal_depth -= 1;
            }
            continue;
        }
        if in_hex {
            output.push(byte);
            index += 1;
            if byte == b'>' {
                in_hex = false;
            }
            continue;
        }
        if byte == b'%' {
            in_comment = true;
            // Keep token separation while dropping comment text.  lopdf's
            // content parser intentionally accepts partial input and can stop
            // at an inline comment, so comments are removed before decoding.
            output.push(b' ');
            index += 1;
            continue;
        }
        if byte == b'(' {
            literal_depth = 1;
            output.push(byte);
            index += 1;
            continue;
        }
        if (byte == b'<' || byte == b'>') && next == Some(byte) {
            output.push(byte);
            output.push(byte);
            index += 2;
            continue;
        }
        if byte == b'<' {
            in_hex = true;
            output.push(byte);
            index += 1;
            continue;
        }
        if byte == b'd'
            && matches!(next, Some(b'0' | b'1'))
            && index
                .checked_sub(1)
                .and_then(|previous| bytes.get(previous))
                .is_none_or(|&previous| previous != b'/' && is_content_boundary(previous))
            && bytes
                .get(index + 2)
                .is_none_or(|&following| is_content_boundary(following))
        {
            output.push(b'd');
            index += 2;
            continue;
        }
        output.push(byte);
        index += 1;
    }
    output
}

fn is_content_boundary(byte: u8) -> bool {
    byte.is_ascii_whitespace() || b"()<>[]{}/%".contains(&byte)
}

fn resolve_value<'a>(document: &'a Document, object: &'a Object) -> Option<PdfValue<'a>> {
    let mut current = object;
    let mut id = None;
    let mut seen = HashSet::new();
    loop {
        match current {
            Object::Reference(object_id) => {
                if !seen.insert(*object_id) {
                    return None;
                }
                id.get_or_insert(*object_id);
                current = document.objects.get(object_id)?;
            }
            _ => {
                return Some(PdfValue {
                    id,
                    object: current,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    use lopdf::{dictionary, StringFormat};
    use std::io::Write;
    use std::time::{Duration, Instant};

    struct Fixture {
        document: Document,
        catalog_id: ObjectId,
        pages_id: ObjectId,
    }

    fn ints(values: &[i64]) -> Object {
        Object::Array(values.iter().copied().map(Object::Integer).collect())
    }

    fn text(value: &str) -> Object {
        Object::string_literal(value)
    }

    /// A string that serializes as `<61>` for `"a"`.
    fn hex(value: &str) -> Object {
        Object::String(value.as_bytes().to_vec(), StringFormat::Hexadecimal)
    }

    impl Fixture {
        fn new() -> Self {
            let mut document = Document::with_version("1.5");
            let pages_id = document.add_object(dictionary! {
                "Type" => "Pages",
                "Kids" => Vec::<Object>::new(),
                "Count" => Object::Integer(0),
            });
            let catalog_id = document.add_object(dictionary! {
                "Type" => "Catalog",
                "Pages" => Object::Reference(pages_id),
            });
            document.trailer.set("Root", Object::Reference(catalog_id));
            Self {
                document,
                catalog_id,
                pages_id,
            }
        }

        fn add_page(&mut self, entries: Vec<(&str, Object)>) -> ObjectId {
            let mut page = dictionary! {
                "Type" => "Page",
                "Parent" => Object::Reference(self.pages_id),
            };
            for (key, value) in entries {
                page.set(key, value);
            }
            let page_id = self.document.add_object(page);
            let pages = self
                .document
                .get_object_mut(self.pages_id)
                .unwrap()
                .as_dict_mut()
                .unwrap();
            let mut kids = pages.get(b"Kids").unwrap().as_array().unwrap().clone();
            kids.push(Object::Reference(page_id));
            pages.set("Count", Object::Integer(kids.len() as i64));
            pages.set("Kids", kids);
            page_id
        }

        fn set_catalog(&mut self, key: &str, value: Object) {
            self.document
                .get_object_mut(self.catalog_id)
                .unwrap()
                .as_dict_mut()
                .unwrap()
                .set(key, value);
        }

        fn add_stream(&mut self, dictionary: Dictionary, content: &str) -> ObjectId {
            self.document
                .add_object(Stream::new(dictionary, content.as_bytes().to_vec()))
        }

        fn add_tree_node(
            &mut self,
            key: &str,
            entries: Vec<Object>,
            limits: Option<Vec<Object>>,
        ) -> ObjectId {
            let mut node = Dictionary::new();
            node.set(key, entries);
            if let Some(limits) = limits {
                node.set("Limits", limits);
            }
            self.document.add_object(node)
        }

        fn add_tree_parent(&mut self, kids: &[ObjectId]) -> ObjectId {
            let kids: Vec<Object> = kids.iter().copied().map(Object::Reference).collect();
            self.document.add_object(dictionary! { "Kids" => kids })
        }

        fn point_dests_at(&mut self, root: ObjectId) {
            self.set_catalog(
                "Names",
                dictionary! { "Dests" => Object::Reference(root) }.into(),
            );
        }

        fn normalize(&mut self) -> Result<Report, Error> {
            let mut report = Report::default();
            normalize_and_validate(&mut self.document, &Options::default(), &mut report)?;
            Ok(report)
        }

        /// A Type3 font whose glyphs `a`, `b`, ... run the given programs.  Its
        /// own resources expose the font as `/F1`, so glyphs can show glyphs.
        fn add_type3_font(&mut self, programs: &[String]) -> ObjectId {
            let font_id = self.document.new_object_id();
            let mut charprocs = Dictionary::new();
            let mut differences = vec![Object::Integer(97)];
            for (index, program) in programs.iter().enumerate() {
                let name = format!("g{index}");
                let id = self.add_stream(Dictionary::new(), program);
                charprocs.set(name.as_str(), Object::Reference(id));
                differences.push(Object::Name(name.into_bytes()));
            }
            let font = dictionary! {
                "Type" => "Font",
                "Subtype" => "Type3",
                "FontBBox" => ints(&[0, 0, 1, 1]),
                "FontMatrix" => Object::Array(vec![
                    Object::Real(0.001),
                    Object::Integer(0),
                    Object::Integer(0),
                    Object::Real(0.001),
                    Object::Integer(0),
                    Object::Integer(0),
                ]),
                "CharProcs" => charprocs,
                "Encoding" => dictionary! {
                    "Type" => "Encoding",
                    "Differences" => differences,
                },
                "Resources" => dictionary! {
                    "Font" => dictionary! { "F1" => Object::Reference(font_id) },
                },
            };
            self.document.objects.insert(font_id, font.into());
            font_id
        }

        /// Forms `F0..=F{depth}` where each level runs the next one `fanout`
        /// times.  Returns the outermost form.
        fn add_form_chain(&mut self, depth: usize, fanout: usize) -> ObjectId {
            let mut next: Option<ObjectId> = None;
            for _ in 0..=depth {
                let mut form = dictionary! {
                    "Type" => "XObject",
                    "Subtype" => "Form",
                    "BBox" => ints(&[0, 0, 10, 10]),
                };
                let content = match next {
                    Some(next) => {
                        form.set(
                            "Resources",
                            dictionary! {
                                "XObject" => dictionary! { "X" => Object::Reference(next) },
                            },
                        );
                        "/X Do ".repeat(fanout)
                    }
                    None => "q Q".to_string(),
                };
                next = Some(self.add_stream(form, &content));
            }
            next.unwrap()
        }

        fn add_type3_page(&mut self, font: ObjectId, contents: &str, xobject: Option<ObjectId>) {
            let mut resources = dictionary! {
                "Font" => dictionary! { "F1" => Object::Reference(font) },
            };
            if let Some(xobject) = xobject {
                resources.set("XObject", dictionary! { "X" => Object::Reference(xobject) });
            }
            let contents = self.add_stream(Dictionary::new(), contents);
            self.add_page(vec![
                ("MediaBox", ints(&[0, 0, 612, 792])),
                ("Resources", resources.into()),
                ("Contents", Object::Reference(contents)),
            ]);
        }
    }

    #[test]
    fn leaf_repeating_only_keys_of_another_leaf_is_an_error() {
        let mut fixture = Fixture::new();
        let a = fixture.add_tree_node("Names", vec![text("a"), Object::Integer(1)], None);
        let b = fixture.add_tree_node("Names", vec![text("a"), Object::Integer(1)], None);
        let root = fixture.add_tree_parent(&[a, b]);
        fixture.point_dests_at(root);

        assert!(matches!(fixture.normalize(), Err(Error::InvalidInput(_))));
    }

    #[test]
    fn hex_string_limits_are_not_repaired() {
        let mut fixture = Fixture::new();
        let a = fixture.add_tree_node(
            "Names",
            vec![text("a"), Object::Integer(1), text("b"), Object::Integer(2)],
            Some(vec![hex("a"), hex("b")]),
        );
        let b = fixture.add_tree_node(
            "Names",
            vec![text("c"), Object::Integer(3)],
            Some(vec![hex("c"), hex("c")]),
        );
        let root = fixture.add_tree_parent(&[a, b]);
        fixture.point_dests_at(root);

        let report = fixture.normalize().unwrap();
        assert_eq!(report.compatibility_repairs, 0, "{:?}", report.warnings);
    }

    #[test]
    fn indirect_limits_are_not_repaired() {
        let mut fixture = Fixture::new();
        let low = fixture.document.add_object(hex("a"));
        let high = fixture.document.add_object(text("b"));
        let leaf = fixture.add_tree_node(
            "Names",
            vec![text("a"), Object::Integer(1), text("b"), Object::Integer(2)],
            Some(vec![Object::Reference(low), Object::Reference(high)]),
        );
        let other = fixture.add_tree_node(
            "Names",
            vec![text("c"), Object::Integer(3)],
            Some(vec![text("c"), text("c")]),
        );
        let root = fixture.add_tree_parent(&[leaf, other]);
        fixture.point_dests_at(root);

        let report = fixture.normalize().unwrap();
        assert_eq!(report.compatibility_repairs, 0);
    }

    #[test]
    fn wrong_limits_are_still_repaired() {
        let mut fixture = Fixture::new();
        let a = fixture.add_tree_node(
            "Names",
            vec![text("a"), Object::Integer(1), text("b"), Object::Integer(2)],
            Some(vec![hex("a"), text("z")]),
        );
        let b = fixture.add_tree_node("Names", vec![text("c"), Object::Integer(3)], None);
        let root = fixture.add_tree_parent(&[a, b]);
        fixture.point_dests_at(root);

        let report = fixture.normalize().unwrap();
        assert_eq!(report.compatibility_repairs, 1);
        let limits = fixture
            .document
            .get_object(a)
            .unwrap()
            .as_dict()
            .unwrap()
            .get(b"Limits")
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(limits, &vec![text("a"), text("b")]);
    }

    #[test]
    fn name_keyed_name_tree_is_accepted_and_keeps_key_objects() {
        let mut fixture = Fixture::new();
        let leaf = fixture.add_tree_node(
            "Names",
            vec![
                Object::Name(b"b".to_vec()),
                Object::Integer(2),
                Object::Name(b"a".to_vec()),
                Object::Integer(1),
            ],
            None,
        );
        fixture.set_catalog("Dests", Object::Reference(leaf));

        let report = fixture.normalize().unwrap();
        assert_eq!(report.compatibility_repairs, 1);
        let names = fixture
            .document
            .get_object(leaf)
            .unwrap()
            .as_dict()
            .unwrap()
            .get(b"Names")
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(names.first(), Some(&Object::Name(b"a".to_vec())));
        assert_eq!(names.get(2), Some(&Object::Name(b"b".to_vec())));
    }

    #[test]
    fn fanned_out_nested_forms_are_scanned_once() {
        let mut fixture = Fixture::new();
        let font = fixture.add_type3_font(&["0 0 d0".to_string()]);
        let chain = fixture.add_form_chain(8, 20);
        fixture.add_type3_page(font, "BT /F1 12 Tf (a) Tj ET /X Do", Some(chain));

        let started = Instant::now();
        validate_type3_execution(&fixture.document, 1024 * 1024).unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));

        let mut bytes = Vec::new();
        fixture.document.save_to(&mut bytes).unwrap();
        let started = Instant::now();
        crate::compress(&bytes, &Options::default()).unwrap();
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn fanned_out_nested_glyph_programs_are_scanned_once() {
        let depth = 8;
        let programs: Vec<String> = (0..=depth)
            .map(|index| {
                if index == depth {
                    "0 0 d0".to_string()
                } else {
                    let next = char::from(b'a' + index as u8 + 1);
                    format!(
                        "0 0 d0 BT /F1 1 Tf {} ET",
                        format!("({next}) Tj ").repeat(20)
                    )
                }
            })
            .collect();
        let mut fixture = Fixture::new();
        let font = fixture.add_type3_font(&programs);
        fixture.add_type3_page(font, "BT /F1 12 Tf (a) Tj ET", None);

        let started = Instant::now();
        validate_type3_execution(&fixture.document, 1024 * 1024).unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn recursive_form_is_rejected() {
        let mut fixture = Fixture::new();
        let font = fixture.add_type3_font(&["0 0 d0".to_string()]);
        let form_id = fixture.document.new_object_id();
        let form = Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Form",
                "BBox" => ints(&[0, 0, 10, 10]),
                "Resources" => dictionary! {
                    "XObject" => dictionary! { "X" => Object::Reference(form_id) },
                },
            },
            b"/X Do".to_vec(),
        );
        fixture.document.objects.insert(form_id, form.into());
        fixture.add_type3_page(font, "BT /F1 12 Tf (a) Tj ET /X Do", Some(form_id));

        assert!(matches!(
            validate_type3_execution(&fixture.document, 1024 * 1024),
            Err(Error::InvalidInput(_))
        ));
    }

    #[test]
    fn deepest_supported_form_nesting_does_not_overflow_the_stack() {
        let mut fixture = Fixture::new();
        let font = fixture.add_type3_font(&["0 0 d0".to_string()]);
        let chain = fixture.add_form_chain(MAX_EXECUTION_DEPTH - 8, 1);
        fixture.add_type3_page(font, "BT /F1 12 Tf (a) Tj ET /X Do", Some(chain));
        validate_type3_execution(&fixture.document, 1024 * 1024).unwrap();

        let mut fixture = Fixture::new();
        let font = fixture.add_type3_font(&["0 0 d0".to_string()]);
        let chain = fixture.add_form_chain(MAX_EXECUTION_DEPTH * 2, 1);
        fixture.add_type3_page(font, "BT /F1 12 Tf (a) Tj ET /X Do", Some(chain));
        assert!(matches!(
            validate_type3_execution(&fixture.document, 1024 * 1024),
            Err(Error::InvalidInput(_))
        ));
    }

    #[test]
    fn self_referencing_contents_array_terminates() {
        let mut fixture = Fixture::new();
        let font = fixture.add_type3_font(&["0 0 d0".to_string()]);
        let contents_id = fixture.document.new_object_id();
        fixture.document.objects.insert(
            contents_id,
            Object::Array(vec![Object::Reference(contents_id)]),
        );
        fixture.add_page(vec![
            ("MediaBox", ints(&[0, 0, 612, 792])),
            (
                "Resources",
                dictionary! {
                    "Font" => dictionary! { "F1" => Object::Reference(font) },
                }
                .into(),
            ),
            ("Contents", Object::Reference(contents_id)),
        ]);
        validate_type3_execution(&fixture.document, 1024 * 1024).unwrap();
    }

    #[test]
    fn stream_over_the_decoded_limit_is_skipped() {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&vec![b' '; 4096]).unwrap();
        let compressed = encoder.finish().unwrap();

        let mut fixture = Fixture::new();
        let font = fixture.add_type3_font(&["0 0 d0".to_string()]);
        let contents = fixture.document.add_object(Stream::new(
            dictionary! { "Filter" => "FlateDecode" },
            compressed,
        ));
        fixture.add_page(vec![
            ("MediaBox", ints(&[0, 0, 612, 792])),
            (
                "Resources",
                dictionary! {
                    "Font" => dictionary! { "F1" => Object::Reference(font) },
                }
                .into(),
            ),
            ("Contents", Object::Reference(contents)),
        ]);
        validate_type3_execution(&fixture.document, 16).unwrap();
    }

    #[test]
    fn dictionary_delimiters_are_two_byte_tokens() {
        assert_eq!(
            normalize_type3_content_tokens(b"<< /K 1 d0 >> d0"),
            b"<< /K 1 d >> d".to_vec()
        );
        assert_eq!(
            normalize_type3_content_tokens(b"/P << /A <6430> /B (d0) >> BDC 1 0 d1"),
            b"/P << /A <6430> /B (d0) >> BDC 1 0 d".to_vec()
        );
    }

    #[test]
    fn missing_media_box_and_invalid_crop_box_are_accepted() {
        let mut fixture = Fixture::new();
        fixture.add_page(vec![]);
        fixture.add_page(vec![("CropBox", ints(&[0, 0, 0, 0]))]);
        fixture.add_page(vec![
            ("MediaBox", ints(&[0, 0, 612, 792])),
            ("CropBox", ints(&[0, 0, 0, 0])),
        ]);
        fixture.add_page(vec![
            ("MediaBox", ints(&[0, 0, 612, 792])),
            ("CropBox", Object::Integer(7)),
        ]);
        validate_page_boxes(&fixture.document).unwrap();
        fixture.normalize().unwrap();
    }

    #[test]
    fn present_but_invalid_media_box_is_rejected() {
        let mut fixture = Fixture::new();
        fixture.add_page(vec![("MediaBox", ints(&[0, 0, 0, 792]))]);
        assert!(matches!(
            validate_page_boxes(&fixture.document),
            Err(Error::InvalidInput(_))
        ));
    }
}
