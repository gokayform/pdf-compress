//! A small, in-process PDF compressor.
//!
//! The compressor deliberately focuses on transformations whose semantics can be
//! established from the PDF object model.  It does not invoke Ghostscript or any
//! other external program.  Image transformations live in [`images`]; the core
//! keeps unknown filters and dictionaries intact.

#![forbid(unsafe_code)]
// Compression is optional for callers, and the release profile aborts on
// panic, so every failure in library code must surface as an `Error`.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing
    )
)]

use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use lopdf::xref::XrefEntry;
use lopdf::xref::XrefType;
use lopdf::{Dictionary, Document, Object, ObjectId, Stream};
use std::collections::hash_map::RandomState;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::hash::{BuildHasher, Hash, Hasher};
use std::io::{self, Read, Write};

/// Direct-object nesting accepted by the structural validators.
const MAX_DIRECT_OBJECT_DEPTH: usize = 1024;
/// Direct-object nesting accepted in the input bytes.  `lopdf`'s parser is
/// recursive and aborts the process on stack exhaustion, so the bound is
/// enforced on the raw bytes before parsing.
const MAX_PARSE_NESTING: usize = 64;
/// Longest chain of indirect references followed while resolving one value
/// (the same bound `lopdf` applies in `Document::dereference`).
const MAX_REFERENCE_CHAIN: usize = 32;

mod annotations;
mod compatibility;
pub mod images;
pub mod jbig2;
mod jpeg;

/// Compression policy presets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Preset {
    /// Preserve image data and apply only lossless stream/object rewrites.
    #[default]
    Lossless,
    /// A small screen-oriented output.  Image conversion is opt-in through
    /// [`Options::allow_lossy`], which is enabled by this preset.
    Screen,
    /// An ebook-oriented output.
    Ebook,
    /// A printer-oriented output.
    Printer,
    /// A prepress-oriented output.
    Prepress,
}

/// Options accepted by [`compress`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    pub preset: Preset,
    pub jpeg_quality: u8,
    pub target_dpi: Option<u32>,
    pub allow_lossy: bool,
    pub max_input_bytes: usize,
    pub max_decoded_stream_bytes: usize,
    pub keep_if_larger: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            preset: Preset::Lossless,
            jpeg_quality: 100,
            target_dpi: None,
            allow_lossy: false,
            max_input_bytes: 256 * 1024 * 1024,
            max_decoded_stream_bytes: 128 * 1024 * 1024,
            keep_if_larger: false,
        }
    }
}

impl Options {
    /// Construct the options normally associated with a preset.
    ///
    /// The values are intentionally explicit and stable.  A caller can use the
    /// returned value as a base and override any public field before calling
    /// [`compress`].
    pub fn for_preset(preset: Preset) -> Self {
        let mut options = Self {
            preset,
            ..Self::default()
        };

        match preset {
            Preset::Lossless => {}
            Preset::Screen => {
                options.jpeg_quality = 60;
                options.target_dpi = Some(72);
                options.allow_lossy = true;
            }
            Preset::Ebook => {
                options.jpeg_quality = 75;
                options.target_dpi = Some(150);
                options.allow_lossy = true;
            }
            Preset::Printer => {
                options.jpeg_quality = 85;
                options.target_dpi = Some(300);
                options.allow_lossy = true;
            }
            Preset::Prepress => {
                options.jpeg_quality = 95;
                options.target_dpi = Some(300);
                options.allow_lossy = true;
            }
        }

        options
    }

    fn validate(&self) -> Result<(), Error> {
        if self.jpeg_quality == 0 || self.jpeg_quality > 100 {
            return Err(Error::InvalidOptions(
                "jpeg_quality must be between 1 and 100".to_string(),
            ));
        }
        if self.target_dpi == Some(0) {
            return Err(Error::InvalidOptions(
                "target_dpi must be greater than zero when supplied".to_string(),
            ));
        }
        if self.max_input_bytes == 0 {
            return Err(Error::InvalidOptions(
                "max_input_bytes must be greater than zero".to_string(),
            ));
        }
        if self.max_decoded_stream_bytes == 0 {
            return Err(Error::InvalidOptions(
                "max_decoded_stream_bytes must be greater than zero".to_string(),
            ));
        }
        Ok(())
    }
}

/// Summary of transformations performed by [`compress`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub input_bytes: usize,
    pub output_bytes: usize,
    pub streams_recompressed: usize,
    pub images_optimized: usize,
    pub objects_removed: usize,
    pub objects_deduplicated: usize,
    /// Lossless compatibility repairs retained even when the output is larger.
    pub compatibility_repairs: usize,
    pub used_original: bool,
    pub warnings: Vec<String>,
}

/// Result of a compression attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompressionResult {
    pub bytes: Vec<u8>,
    pub report: Report,
}

/// Errors returned by the compressor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    InvalidInput(String),
    Unsupported(String),
    LimitExceeded(String),
    InvalidOptions(String),
    Pdf(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput(message) => write!(f, "invalid PDF input: {message}"),
            Self::Unsupported(message) => write!(f, "unsupported PDF feature: {message}"),
            Self::LimitExceeded(message) => write!(f, "PDF resource limit exceeded: {message}"),
            Self::InvalidOptions(message) => write!(f, "invalid compression options: {message}"),
            Self::Pdf(message) => write!(f, "PDF processing error: {message}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<lopdf::Error> for Error {
    fn from(error: lopdf::Error) -> Self {
        Self::Pdf(error.to_string())
    }
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Pdf(error.to_string())
    }
}

/// Compress a PDF in memory.
///
/// The input is parsed before any transformation.  Signed and encrypted files
/// are rejected because rewriting either one invalidates its security model.
/// If an optimized serialization is larger than the input, the original bytes
/// are returned unless `options.keep_if_larger` is set or a compatibility repair
/// requires retaining the rewritten document.
pub fn compress(input: &[u8], options: &Options) -> Result<CompressionResult, Error> {
    options.validate()?;
    if input.len() > options.max_input_bytes {
        return Err(Error::LimitExceeded(format!(
            "input is {} bytes, maximum is {}",
            input.len(),
            options.max_input_bytes
        )));
    }
    if input.is_empty() {
        return Err(Error::InvalidInput("input is empty".to_string()));
    }

    preflight_raw_structure(input, options, false)?;
    let mut document = Document::load_mem(input)
        .map_err(|error| Error::InvalidInput(format!("could not parse PDF: {error}")))?;
    reject_unsafe_document(input, &document)?;
    let dangling_references = nullify_dangling_references(&mut document);
    validate_document_structure(&document)?;

    let source_structure_sound = serialized_structure_is_sound(input, &document);
    let original_page_count = document.get_pages().len();
    let mut report = Report {
        input_bytes: input.len(),
        ..Report::default()
    };
    if dangling_references > 0 {
        // Deliberately not a compatibility repair: a null reference is what
        // every reader already sees, so it must not force a larger rewrite.
        report.warnings.push(format!(
            "replaced {dangling_references} reference(s) to missing objects with null"
        ));
    }

    compatibility::normalize_and_validate(&mut document, options, &mut report)?;
    annotations::normalize_annotations(&mut document, &mut report)?;
    jbig2::normalize_images(&mut document, options, &mut report)?;

    // Images own their colour-space and placement policy. The image module is
    // called first; the generic pass below only applies the same lossless
    // Flate rewrite to raw image bytes, while preserving all image dictionaries
    // and any existing non-Flate filter.
    images::optimize_images(&mut document, options, &mut report)?;

    // Pruning starts at the trailer and therefore retains all objects reachable
    // through Root, Info, ID, and any other trailer entry.  It also handles page
    // trees and resource graphs without guessing at PDF-specific keys.  Running
    // it before stream recompression keeps orphans from costing decode work or
    // warnings.
    report.objects_removed += prune_unreachable_objects(&mut document);
    recompress_streams(&mut document, options, &mut report)?;
    report.objects_deduplicated += deduplicate_objects(&mut document);
    // Deduplication can make another object unreachable if a malformed input
    // had a reference cycle.  A second pass is cheap and keeps serialization
    // deterministic.
    report.objects_removed += prune_unreachable_objects(&mut document);

    // This is a complete rewrite, so stale incremental-update pointers must not
    // survive into the new trailer.  Use the classic xref table for the output:
    // it is valid for every PDF version and is accepted by viewers that do not
    // implement PDF 1.5 cross-reference streams.
    document.trailer.remove(b"Prev");
    document.trailer.remove(b"XRefStm");
    // These keys belong to an xref stream dictionary.  A classic trailer has
    // no use for them, and retaining them can make a writer emit a hybrid
    // trailer that older viewers interpret inconsistently.
    for key in [
        b"Type".as_slice(),
        b"W".as_slice(),
        b"Index".as_slice(),
        b"Length".as_slice(),
        b"Filter".as_slice(),
        b"DecodeParms".as_slice(),
    ] {
        document.trailer.remove(key);
    }
    document.reference_table.cross_reference_type = XrefType::CrossReferenceTable;
    // Pruning can remove the highest IDs (including the old xref stream).
    // lopdf derives trailer /Size from max_id, not the surviving object map.
    document.max_id = document.objects.keys().map(|id| id.0).max().unwrap_or(0);
    if document.max_id == u32::MAX {
        return Err(Error::LimitExceeded(
            "object number exceeds writer capacity".into(),
        ));
    }
    // The writer emits one xref entry per number up to max_id, so a sparse
    // numbering such as a single object 4000000000 would allocate gigabytes.
    let max_sparse_id = document.objects.len().saturating_mul(4).max(1 << 20);
    if !usize::try_from(document.max_id).is_ok_and(|max_id| max_id <= max_sparse_id) {
        return Err(Error::LimitExceeded(
            "object numbers are too sparse to write a cross-reference table".into(),
        ));
    }

    let mut output = Vec::new();
    document
        .save_to(&mut output)
        .map_err(|error| Error::Pdf(format!("could not serialize PDF: {error}")))?;

    // The writer should always generate a readable document.  Reparse it here
    // before returning so callers never receive an unverified byte sequence.
    let written = Document::load_mem(&output)
        .map_err(|error| Error::Pdf(format!("serialized PDF could not be read: {error}")))?;
    validate_document_structure(&written)?;
    if !serialized_structure_is_sound(&output, &written) {
        return Err(Error::Pdf(
            "serialized PDF failed independent xref/stream integrity checks".to_string(),
        ));
    }
    if written.get_pages().len() != original_page_count {
        return Err(Error::Pdf(format!(
            "page count changed during compression ({} -> {})",
            original_page_count,
            written.get_pages().len()
        )));
    }
    if written.trailer.get(b"Encrypt").is_ok() {
        return Err(Error::Pdf(
            "serialized document unexpectedly contains encryption".to_string(),
        ));
    }

    if output.len() > input.len()
        && !options.keep_if_larger
        && source_structure_sound
        && report.compatibility_repairs == 0
    {
        report.warnings.push(format!(
            "optimized output ({} bytes) was larger than input ({} bytes); original returned",
            output.len(),
            input.len()
        ));
        report.streams_recompressed = 0;
        report.images_optimized = 0;
        report.objects_removed = 0;
        report.objects_deduplicated = 0;
        report.used_original = true;
        report.output_bytes = input.len();
        return Ok(CompressionResult {
            bytes: input.to_vec(),
            report,
        });
    }

    if output.len() > input.len() && !options.keep_if_larger && report.compatibility_repairs > 0 {
        report.warnings.push(
            "output was larger, but lossless compatibility repairs require retaining the rewrite"
                .to_string(),
        );
    }

    if output.len() > input.len() && !options.keep_if_larger && !source_structure_sound {
        report.warnings.push(
            "optimized output was larger, but the source structure was not independently sound; valid rewrite retained"
                .to_string(),
        );
    }

    report.output_bytes = output.len();
    Ok(CompressionResult {
        bytes: output,
        report,
    })
}

/// Reject document features for which a full rewrite would be unsafe.
fn reject_unsafe_document(input: &[u8], document: &Document) -> Result<(), Error> {
    if document.trailer.get(b"Encrypt").is_ok()
        || document.is_encrypted()
        || document.encryption_state.is_some()
    {
        return Err(Error::Unsupported(
            "encrypted documents are not rewritten".to_string(),
        ));
    }

    // `/ByteRange` is part of every standard PDF signature dictionary.  Keep a
    // raw-byte check because signatures can live in an older incremental update
    // which a high-level reader may not expose as a current object.
    if contains_raw_byte_range_entry(input) {
        return Err(Error::Unsupported(
            "signed documents are not rewritten".to_string(),
        ));
    }

    let mut signature_object = false;
    for object in document.objects.values() {
        if object_contains_signature(object) {
            signature_object = true;
            break;
        }
    }
    if signature_object {
        return Err(Error::Unsupported(
            "signature fields and signature dictionaries are not rewritten".to_string(),
        ));
    }
    Ok(())
}

/// Lexically scan serialized PDF bytes for inputs that would crash `lopdf`'s
/// recursive parser or its cross-reference/decryption code before this crate
/// gets a chance to validate anything: direct objects nested deeper than
/// [`MAX_PARSE_NESTING`], `/Encrypt` entries, cross-reference stream widths
/// that make the reader loop or allocate without bound, and object or
/// cross-reference streams whose Flate payload exceeds the decoded-size limit.
///
/// Stream payloads are skipped by searching for `endstream`.  Object stream
/// members are only reachable after inflation, so their payload is inflated
/// within the decoded-stream limit and scanned with `in_object_stream` set.
fn preflight_raw_structure(
    bytes: &[u8],
    options: &Options,
    in_object_stream: bool,
) -> Result<(), Error> {
    #[derive(Default)]
    struct ObjectScan {
        object_stream: bool,
        xref_stream: bool,
        filtered: bool,
        widths: Option<Vec<i64>>,
    }

    let too_deep = || {
        Error::LimitExceeded(format!(
            "direct object nesting exceeds {MAX_PARSE_NESTING} levels"
        ))
    };
    let mut scan = ObjectScan::default();
    let mut depth = 0usize;
    let mut cursor = 0usize;
    while let Some(&byte) = bytes.get(cursor) {
        match byte {
            b'%' => {
                while bytes
                    .get(cursor)
                    .is_some_and(|b| !matches!(b, b'\r' | b'\n'))
                {
                    cursor += 1;
                }
            }
            b'(' => cursor = skip_literal_string(bytes, cursor),
            b'<' if bytes.get(cursor + 1) == Some(&b'<') => {
                depth += 1;
                if depth > MAX_PARSE_NESTING {
                    return Err(too_deep());
                }
                cursor += 2;
            }
            b'<' => {
                cursor = bytes
                    .get(cursor..)
                    .and_then(|rest| rest.iter().position(|b| *b == b'>'))
                    .map_or(bytes.len(), |offset| cursor + offset + 1);
            }
            b'>' if bytes.get(cursor + 1) == Some(&b'>') => {
                depth = depth.saturating_sub(1);
                cursor += 2;
            }
            b'[' => {
                depth += 1;
                if depth > MAX_PARSE_NESTING {
                    return Err(too_deep());
                }
                cursor += 1;
            }
            b']' => {
                depth = depth.saturating_sub(1);
                cursor += 1;
            }
            b'/' => {
                let start = cursor + 1;
                let mut end = start;
                while bytes.get(end).is_some_and(|b| is_pdf_regular(*b)) {
                    end += 1;
                }
                let name = bytes.get(start..end).unwrap_or_default();
                cursor = end;
                match name {
                    b"ObjStm" => scan.object_stream = true,
                    b"XRef" => scan.xref_stream = true,
                    b"Filter" if depth == 1 => scan.filtered = true,
                    b"W" if depth == 1 => scan.widths = raw_integer_array(bytes, end),
                    b"Encrypt" if !in_object_stream && depth >= 1 => {
                        let mut value = end;
                        skip_pdf_space(bytes, &mut value);
                        if bytes
                            .get(value)
                            .is_some_and(|b| b.is_ascii_digit() || *b == b'<')
                        {
                            return Err(Error::Unsupported(
                                "encrypted documents are not rewritten".to_string(),
                            ));
                        }
                    }
                    _ => {}
                }
            }
            byte if !is_pdf_regular(byte) => cursor += 1,
            _ => {
                let start = cursor;
                while bytes.get(cursor).is_some_and(|b| is_pdf_regular(*b)) {
                    cursor += 1;
                }
                match bytes.get(start..cursor) {
                    Some(b"obj" | b"endobj") => scan = ObjectScan::default(),
                    Some(b"stream") if !in_object_stream && depth == 0 => {
                        let mut body = cursor;
                        while bytes.get(body).is_some_and(|b| matches!(b, b' ' | b'\t')) {
                            body += 1;
                        }
                        if bytes.get(body..body + 2) == Some(b"\r\n") {
                            body += 2;
                        } else if bytes.get(body).is_some_and(|b| matches!(b, b'\r' | b'\n')) {
                            body += 1;
                        }
                        let end = bytes
                            .get(body..)
                            .and_then(|rest| {
                                rest.windows(b"endstream".len())
                                    .position(|window| window == b"endstream")
                            })
                            .map_or(bytes.len(), |offset| body + offset);
                        if scan.xref_stream {
                            check_xref_stream_widths(scan.widths.as_deref())?;
                        }
                        if scan.object_stream || scan.xref_stream {
                            let payload = bytes.get(body..end).unwrap_or_default();
                            inspect_stream_payload(
                                payload,
                                scan.object_stream,
                                scan.filtered,
                                options,
                            )?;
                        }
                        scan = ObjectScan::default();
                        cursor = end;
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

fn is_pdf_regular(byte: u8) -> bool {
    byte != 0 && !is_pdf_name_boundary(byte)
}

/// Return the index just past a literal string that starts at `start`.
fn skip_literal_string(bytes: &[u8], start: usize) -> usize {
    let mut cursor = start + 1;
    let mut nesting = 1usize;
    while nesting > 0 {
        match bytes.get(cursor) {
            None => return bytes.len(),
            Some(b'\\') => cursor = cursor.saturating_add(2),
            Some(b'(') => {
                nesting += 1;
                cursor += 1;
            }
            Some(b')') => {
                nesting -= 1;
                cursor += 1;
            }
            Some(_) => cursor += 1,
        }
    }
    cursor
}

/// Read the first few integers of an array that follows `from`.
fn raw_integer_array(bytes: &[u8], from: usize) -> Option<Vec<i64>> {
    let mut cursor = from;
    skip_pdf_space(bytes, &mut cursor);
    if bytes.get(cursor) != Some(&b'[') {
        return None;
    }
    cursor += 1;
    let mut values = Vec::new();
    while values.len() < 8 {
        let Some(value) = raw_integer(bytes, &mut cursor) else {
            break;
        };
        values.push(value);
    }
    Some(values)
}

/// `lopdf` allocates `/W` bytes per field and, when every width is zero,
/// iterates `/Index` counts without consuming any data.
fn check_xref_stream_widths(widths: Option<&[i64]>) -> Result<(), Error> {
    let Some(widths) = widths.filter(|widths| widths.len() >= 3) else {
        return Ok(());
    };
    let first_three = widths.iter().take(3);
    if first_three.clone().any(|width| !(0..=8).contains(width)) || first_three.sum::<i64>() == 0 {
        return Err(Error::InvalidInput(
            "cross-reference stream has unsupported /W field widths".to_string(),
        ));
    }
    Ok(())
}

fn inspect_stream_payload(
    payload: &[u8],
    object_stream: bool,
    filtered: bool,
    options: &Options,
) -> Result<(), Error> {
    if !filtered {
        return if object_stream {
            preflight_raw_structure(payload, options, true)
        } else {
            Ok(())
        };
    }
    match decode_flate_bounded(payload, options.max_decoded_stream_bytes) {
        Ok(decoded) if object_stream => preflight_raw_structure(&decoded, options, true),
        Ok(_) => Ok(()),
        Err(error @ Error::LimitExceeded(_)) => Err(error),
        // `lopdf` inflates these streams without a size bound, so a payload
        // that cannot be measured here must not reach the reader.
        Err(_) => Err(Error::Unsupported(
            "object or cross-reference stream is not bounded Flate data".to_string(),
        )),
    }
}

/// Replace references to objects that do not exist with `null`
/// (PDF 32000-1:2008 7.3.10) everywhere except the trailer `/Root`, whose
/// absence must stay an error.  Iterative, so nesting depth cannot exhaust the
/// stack.  Returns the number of references replaced.
fn nullify_dangling_references(document: &mut Document) -> usize {
    fn nullify(root: &mut Object, existing: &HashSet<ObjectId>, replaced: &mut usize) {
        let mut stack = vec![root];
        while let Some(object) = stack.pop() {
            if let Object::Reference(id) = &*object {
                if !existing.contains(id) {
                    *object = Object::Null;
                    *replaced += 1;
                }
                continue;
            }
            match object {
                Object::Array(array) => stack.extend(array.iter_mut()),
                Object::Dictionary(dictionary) => {
                    stack.extend(dictionary.iter_mut().map(|(_, value)| value));
                }
                Object::Stream(stream) => {
                    stack.extend(stream.dict.iter_mut().map(|(_, value)| value));
                }
                _ => {}
            }
        }
    }

    let existing: HashSet<ObjectId> = document.objects.keys().copied().collect();
    let mut replaced = 0usize;
    for object in document.objects.values_mut() {
        nullify(object, &existing, &mut replaced);
    }
    for (key, value) in document.trailer.iter_mut() {
        if key.as_slice() != b"Root" {
            nullify(value, &existing, &mut replaced);
        }
    }
    replaced
}

/// Remove every object not reachable from the trailer and return how many were
/// removed.  Equivalent to `Document::prune_objects`, which recurses over
/// direct objects and scans its visited list linearly.
fn prune_unreachable_objects(document: &mut Document) -> usize {
    let mut reachable: HashSet<ObjectId> = HashSet::new();
    let mut pending: Vec<&Object> = document.trailer.iter().map(|(_, value)| value).collect();
    while let Some(object) = pending.pop() {
        match object {
            Object::Reference(id) => {
                if reachable.insert(*id) {
                    pending.extend(document.objects.get(id));
                }
            }
            Object::Array(array) => pending.extend(array.iter()),
            Object::Dictionary(dictionary) => {
                pending.extend(dictionary.iter().map(|(_, value)| value));
            }
            Object::Stream(stream) => {
                pending.extend(stream.dict.iter().map(|(_, value)| value));
            }
            _ => {}
        }
    }
    let before = document.objects.len();
    document.objects.retain(|id, _| reachable.contains(id));
    before.saturating_sub(document.objects.len())
}

/// Match `/ByteRange` only when it is shaped like a dictionary entry whose
/// value is an integer array, so prose such as `(/ByteRange signatures)` in an
/// uncompressed content stream does not look like a signature.
fn contains_raw_byte_range_entry(input: &[u8]) -> bool {
    const NAME: &[u8] = b"/ByteRange";
    input
        .windows(NAME.len())
        .enumerate()
        .filter(|(_, window)| *window == NAME)
        .any(|(index, _)| {
            let mut cursor = index.saturating_add(NAME.len());
            if !input
                .get(cursor)
                .is_some_and(|byte| is_pdf_name_boundary(*byte))
            {
                return false;
            }
            skip_pdf_space(input, &mut cursor);
            if input.get(cursor) != Some(&b'[') {
                return false;
            }
            cursor += 1;
            skip_pdf_space(input, &mut cursor);
            if input.get(cursor) == Some(&b'+') {
                cursor += 1;
            }
            input.get(cursor).is_some_and(u8::is_ascii_digit)
        })
}

fn object_contains_signature(object: &Object) -> bool {
    object_contains_signature_inner(object, 0)
}

fn object_contains_signature_inner(object: &Object, depth: usize) -> bool {
    if depth > MAX_DIRECT_OBJECT_DEPTH {
        // A deeply nested direct value is not a valid reason to recurse until
        // the process stack is exhausted.  The structural validator rejects
        // such values before rewriting, so this conservative answer is safe.
        return true;
    }
    match object {
        // Every indirect object is scanned independently by the caller. Do not
        // follow references here: page trees intentionally contain parent
        // cycles, and following every edge makes this check needlessly
        // expensive on documents with many shared resources.
        Object::Array(array) => array
            .iter()
            .any(|value| object_contains_signature_inner(value, depth.saturating_add(1))),
        Object::Dictionary(dictionary) => dictionary_contains_signature_inner(dictionary, depth),
        Object::Stream(stream) => dictionary_contains_signature_inner(&stream.dict, depth),
        _ => false,
    }
}

fn dictionary_contains_signature_inner(dictionary: &Dictionary, depth: usize) -> bool {
    if dictionary.get(b"ByteRange").is_ok()
        || dictionary
            .get(b"Type")
            .and_then(Object::as_name)
            .map(|name| name == b"Sig")
            .unwrap_or(false)
        || dictionary
            .get(b"FT")
            .and_then(Object::as_name)
            .map(|name| name == b"Sig")
            .unwrap_or(false)
    {
        return true;
    }
    dictionary
        .iter()
        .any(|(_, value)| object_contains_signature_inner(value, depth.saturating_add(1)))
}

/// Check the small set of structural invariants needed before a full rewrite.
/// `lopdf` is intentionally permissive while parsing malformed files; rejecting
/// a missing catalog, dangling reference, or broken page tree here prevents the
/// writer from manufacturing a file that only one permissive parser can open.
fn validate_document_structure(document: &Document) -> Result<(), Error> {
    let root = document
        .trailer
        .get(b"Root")
        .map_err(|_| Error::InvalidInput("PDF trailer has no /Root catalog".to_string()))?;
    if root.as_reference().is_err() {
        return Err(Error::InvalidInput(
            "PDF trailer /Root must be an indirect reference".to_string(),
        ));
    }
    let root_dictionary = resolve_dictionary_object(document, root)
        .map_err(|_| Error::InvalidInput("/Root is not a dictionary".to_string()))?;
    let type_name = root_dictionary
        .get(b"Type")
        .and_then(Object::as_name)
        .map_err(|_| Error::InvalidInput("/Root dictionary has no /Catalog Type".to_string()))?;
    if type_name != b"Catalog" {
        return Err(Error::InvalidInput(
            "/Root dictionary is not a /Catalog".to_string(),
        ));
    }

    let pages = root_dictionary
        .get(b"Pages")
        .map_err(|_| Error::InvalidInput("catalog has no /Pages tree".to_string()))?;
    let pages_dictionary = resolve_dictionary_object(document, pages)
        .map_err(|_| Error::InvalidInput("catalog /Pages entry is not a dictionary".to_string()))?;
    if pages_dictionary.get(b"Type").and_then(Object::as_name).ok() != Some(b"Pages".as_slice()) {
        return Err(Error::InvalidInput(
            "catalog /Pages entry is not a /Pages node".to_string(),
        ));
    }
    let mut page_tree_stack = Vec::new();
    let mut page_tree_nodes = std::collections::HashSet::new();
    let mut page_nodes = std::collections::HashSet::new();
    let page_count = validate_page_tree_node(
        document,
        pages,
        0,
        &mut page_tree_stack,
        None,
        &mut page_tree_nodes,
        &mut page_nodes,
    )?;
    if page_count != document.get_pages().len() {
        return Err(Error::InvalidInput(format!(
            "page tree count ({page_count}) disagrees with the parsed page tree ({})",
            document.get_pages().len()
        )));
    }

    for object in document.objects.values() {
        validate_direct_references(object, document)?;
    }
    validate_direct_references(&document.trailer.clone().into(), document)?;
    Ok(())
}

fn validate_page_tree_node(
    document: &Document,
    object: &Object,
    depth: usize,
    stack: &mut Vec<ObjectId>,
    expected_parent: Option<ObjectId>,
    seen_nodes: &mut std::collections::HashSet<ObjectId>,
    seen_pages: &mut std::collections::HashSet<ObjectId>,
) -> Result<usize, Error> {
    const MAX_PAGE_TREE_DEPTH: usize = 256;
    // Direct dictionaries never enter `stack`, so nesting is bounded separately.
    if depth > MAX_PAGE_TREE_DEPTH {
        return Err(Error::InvalidInput(
            "page tree exceeds the supported nesting depth".to_string(),
        ));
    }
    let (dictionary, object_id, pushed) = match object {
        Object::Reference(id) => {
            if stack.len() >= MAX_PAGE_TREE_DEPTH {
                return Err(Error::InvalidInput(
                    "page tree exceeds the supported nesting depth".to_string(),
                ));
            }
            if stack.contains(id) {
                return Err(Error::InvalidInput(format!(
                    "cycle in page tree at object {id:?}"
                )));
            }
            let target = document.objects.get(id).ok_or_else(|| {
                Error::InvalidInput(format!("page tree references missing object {id:?}"))
            })?;
            if !seen_nodes.insert(*id) {
                return Err(Error::InvalidInput(format!(
                    "page tree object {id:?} is referenced more than once"
                )));
            }
            stack.push(*id);
            let dictionary = target.as_dict().map_err(|_| {
                Error::InvalidInput(format!("page tree object {id:?} is not a dictionary"))
            })?;
            (dictionary, Some(*id), true)
        }
        Object::Dictionary(dictionary) => (dictionary, None, false),
        _ => {
            return Err(Error::InvalidInput(
                "page tree node is not a dictionary".to_string(),
            ))
        }
    };

    let type_name = dictionary
        .get(b"Type")
        .and_then(Object::as_name)
        .map_err(|_| Error::InvalidInput("page tree node has no valid /Type".to_string()))?;
    let result = match type_name {
        b"Page" => {
            let parent = dictionary
                .get(b"Parent")
                .and_then(Object::as_reference)
                .map_err(|_| Error::InvalidInput("/Page node has no valid /Parent".to_string()))?;
            let expected = expected_parent.ok_or_else(|| {
                Error::InvalidInput("a page cannot be the root of the page tree".to_string())
            })?;
            if parent != expected {
                return Err(Error::InvalidInput(format!(
                    "page {object_id:?} has /Parent {parent:?}, expected {expected:?}"
                )));
            }
            if let Some(id) = object_id {
                if !seen_pages.insert(id) {
                    return Err(Error::InvalidInput(format!(
                        "page object {id:?} is referenced more than once"
                    )));
                }
            }
            Ok(1)
        }
        b"Pages" => {
            let parent = dictionary.get(b"Parent");
            match expected_parent {
                Some(expected) => {
                    let parent = parent.and_then(Object::as_reference).map_err(|_| {
                        Error::InvalidInput("/Pages node has no valid /Parent".to_string())
                    })?;
                    if parent != expected {
                        return Err(Error::InvalidInput(format!(
                            "page tree node {object_id:?} has /Parent {parent:?}, expected {expected:?}"
                        )));
                    }
                }
                None if parent.is_ok() => {
                    return Err(Error::InvalidInput(
                        "root /Pages node must not have a /Parent".to_string(),
                    ));
                }
                None => {}
            }
            let kids_object = dictionary
                .get(b"Kids")
                .map_err(|_| Error::InvalidInput("/Pages node has no /Kids array".to_string()))?;
            let kids = resolve_array_object(document, kids_object)
                .map_err(|_| Error::InvalidInput("/Pages node has no /Kids array".to_string()))?;
            let mut count = 0usize;
            for child in kids {
                count = count
                    .checked_add(validate_page_tree_node(
                        document,
                        child,
                        depth + 1,
                        stack,
                        object_id,
                        seen_nodes,
                        seen_pages,
                    )?)
                    .ok_or_else(|| Error::InvalidInput("page count overflow".to_string()))?;
            }
            let declared = dictionary
                .get(b"Count")
                .ok()
                .and_then(|value| resolve_integer_object(document, value))
                .ok_or_else(|| {
                    Error::InvalidInput("/Pages node has no integer /Count".to_string())
                })?;
            if declared < 0 || declared as usize != count {
                return Err(Error::InvalidInput(format!(
                    "page tree /Count is {declared}, but it contains {count} pages"
                )));
            }
            Ok(count)
        }
        _ => Err(Error::InvalidInput(format!(
            "unexpected page tree /Type {:?}",
            String::from_utf8_lossy(type_name)
        ))),
    };
    if pushed {
        stack.pop();
    }
    result
}

fn resolve_integer_object(document: &Document, object: &Object) -> Option<i64> {
    let mut current = object;
    let mut seen = std::collections::HashSet::new();
    loop {
        match current {
            Object::Integer(value) => return Some(*value),
            Object::Reference(id) => {
                if seen.len() >= MAX_REFERENCE_CHAIN || !seen.insert(*id) {
                    return None;
                }
                current = document.objects.get(id)?;
            }
            _ => return None,
        }
    }
}

fn validate_direct_references(object: &Object, document: &Document) -> Result<(), Error> {
    validate_direct_references_inner(object, document, 0)
}

fn validate_direct_references_inner(
    object: &Object,
    document: &Document,
    depth: usize,
) -> Result<(), Error> {
    if depth > MAX_DIRECT_OBJECT_DEPTH {
        return Err(Error::InvalidInput(
            "PDF direct object nesting exceeds the supported depth".to_string(),
        ));
    }
    match object {
        Object::Reference(id) => {
            if !document.objects.contains_key(id) {
                return Err(Error::InvalidInput(format!(
                    "reference to missing object {id:?}"
                )));
            }
        }
        Object::Array(array) => {
            for item in array {
                validate_direct_references_inner(item, document, depth + 1)?;
            }
        }
        Object::Dictionary(dictionary) => {
            for (_, value) in dictionary.iter() {
                validate_direct_references_inner(value, document, depth + 1)?;
            }
        }
        Object::Stream(stream) => {
            for (_, value) in stream.dict.iter() {
                validate_direct_references_inner(value, document, depth + 1)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn resolve_dictionary_object<'a>(
    document: &'a Document,
    object: &'a Object,
) -> Result<&'a Dictionary, ()> {
    let mut current = object;
    let mut seen = Vec::new();
    loop {
        match current {
            Object::Dictionary(dictionary) => return Ok(dictionary),
            Object::Reference(id) => {
                if seen.len() >= MAX_REFERENCE_CHAIN || seen.contains(id) {
                    return Err(());
                }
                seen.push(*id);
                current = document.objects.get(id).ok_or(())?;
            }
            _ => return Err(()),
        }
    }
}

fn resolve_array_object<'a>(
    document: &'a Document,
    object: &'a Object,
) -> Result<&'a Vec<Object>, ()> {
    let mut current = object;
    let mut seen = Vec::new();
    loop {
        match current {
            Object::Array(array) => return Ok(array),
            Object::Reference(id) => {
                if seen.len() >= MAX_REFERENCE_CHAIN || seen.contains(id) {
                    return Err(());
                }
                seen.push(*id);
                current = document.objects.get(id).ok_or(())?;
            }
            _ => return Err(()),
        }
    }
}

/// Validate the byte-level structure that `lopdf` does not expose after
/// parsing.  The parser intentionally repairs or normalizes some malformed
/// inputs (for example, it replaces a stream's dictionary length while reading
/// the payload), so this check works against the original serialized bytes.
/// It is used both to decide whether returning the original is safe and to
/// verify that the rewritten bytes are independently well-formed.
fn serialized_structure_is_sound(bytes: &[u8], document: &Document) -> bool {
    let eof_trimmed = bytes.trim_ascii_end();
    if !bytes.starts_with(b"%PDF-") || !eof_trimmed.ends_with(b"%%EOF") {
        return false;
    }
    let Some(startxref) = parse_last_startxref(bytes) else {
        return false;
    };
    if startxref != document.xref_start || startxref >= bytes.len() {
        return false;
    }
    // A complete rewrite intentionally removes these entries. If they occur
    // in the final cross-reference section, conservatively retain the new
    // self-contained rewrite when it grows instead of returning an incremental
    // chain we have not audited.
    let Some(xref_section) = bytes.get(startxref..) else {
        return false;
    };
    if contains_pdf_name(xref_section, b"Prev") || contains_pdf_name(xref_section, b"XRefStm") {
        return false;
    }

    match document.reference_table.cross_reference_type {
        XrefType::CrossReferenceTable => {
            if !xref_section.starts_with(b"xref") {
                return false;
            }
        }
        XrefType::CrossReferenceStream => {
            if !pdf_version_at_least(&document.version, 1, 5)
                || !xref_stream_header_is_sound(bytes, startxref, document)
            {
                return false;
            }
        }
    }

    for (id, object) in &document.objects {
        let Some(entry) = document.reference_table.entries.get(&id.0) else {
            return false;
        };
        let XrefEntry::Normal { offset, generation } = entry else {
            // Objects loaded from an object stream have no standalone byte
            // offset.  Stream objects cannot legally be stored in an object
            // stream, so any stream reaching this arm is not confidently sound.
            if matches!(object, Object::Stream(_)) {
                return false;
            }
            continue;
        };
        let Ok(offset) = usize::try_from(*offset) else {
            return false;
        };
        if *generation != id.1 || !object_header_at(bytes, offset, id.0, *generation) {
            return false;
        }
        if let Object::Stream(stream) = object {
            if !raw_stream_is_sound(bytes, offset, document, Some(stream)) {
                return false;
            }
        }
    }
    true
}

fn parse_last_startxref(bytes: &[u8]) -> Option<usize> {
    let marker = b"startxref";
    let marker_pos = bytes
        .windows(marker.len())
        .rposition(|window| window == marker)?;
    let mut cursor = marker_pos.saturating_add(marker.len());
    while bytes.get(cursor).is_some_and(u8::is_ascii_whitespace) {
        cursor += 1;
    }
    let start = cursor;
    while bytes.get(cursor).is_some_and(u8::is_ascii_digit) {
        cursor += 1;
    }
    if start == cursor {
        return None;
    }
    std::str::from_utf8(bytes.get(start..cursor)?)
        .ok()?
        .parse::<usize>()
        .ok()
}

fn contains_pdf_name(bytes: &[u8], name: &[u8]) -> bool {
    let mut token = Vec::with_capacity(name.len() + 1);
    token.push(b'/');
    token.extend_from_slice(name);
    bytes
        .windows(token.len())
        .enumerate()
        .any(|(index, window)| {
            window == token
                && bytes
                    .get(index.saturating_add(token.len()))
                    .is_none_or(|byte| is_pdf_name_boundary(*byte))
        })
}

fn is_pdf_name_boundary(byte: u8) -> bool {
    byte.is_ascii_whitespace() || b"()<>[]{}/%".contains(&byte)
}

fn pdf_version_at_least(version: &str, major: u32, minor: u32) -> bool {
    let mut parts = version.split('.');
    let Some(found_major) = parts.next().and_then(|part| part.parse::<u32>().ok()) else {
        return false;
    };
    let found_minor = parts
        .next()
        .and_then(|part| {
            part.chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
                .parse::<u32>()
                .ok()
        })
        .unwrap_or(0);
    found_major > major || (found_major == major && found_minor >= minor)
}

fn object_header_at(bytes: &[u8], offset: usize, id: u32, generation: u16) -> bool {
    let expected = format!("{id} {generation} obj");
    bytes
        .get(offset..)
        .is_some_and(|remaining| remaining.starts_with(expected.as_bytes()))
}

fn xref_stream_header_is_sound(bytes: &[u8], offset: usize, document: &Document) -> bool {
    raw_stream_is_sound(bytes, offset, document, None)
}

fn skip_pdf_space(bytes: &[u8], cursor: &mut usize) {
    loop {
        while bytes
            .get(*cursor)
            .is_some_and(|b| b.is_ascii_whitespace() || *b == 0)
        {
            *cursor += 1;
        }
        if bytes.get(*cursor) != Some(&b'%') {
            break;
        }
        while bytes
            .get(*cursor)
            .is_some_and(|b| !matches!(b, b'\r' | b'\n'))
        {
            *cursor += 1;
        }
    }
}

fn raw_slice(bytes: &[u8], start: usize, length: usize) -> Option<&[u8]> {
    bytes.get(start..start.checked_add(length)?)
}

fn raw_integer(bytes: &[u8], cursor: &mut usize) -> Option<i64> {
    skip_pdf_space(bytes, cursor);
    let start = *cursor;
    if bytes.get(*cursor).is_some_and(|b| matches!(b, b'+' | b'-')) {
        *cursor += 1;
    }
    let digits = *cursor;
    while bytes.get(*cursor).is_some_and(u8::is_ascii_digit) {
        *cursor += 1;
    }
    if digits == *cursor {
        return None;
    }
    std::str::from_utf8(bytes.get(start..*cursor)?)
        .ok()?
        .parse()
        .ok()
}

/// Locate the outer stream dictionary without mistaking strings, comments,
/// nested dictionaries, or array values for a stream keyword or /Length key.
fn raw_stream_header(bytes: &[u8], offset: usize, doc: &Document) -> Option<(usize, usize)> {
    let mut cursor = offset;
    raw_integer(bytes, &mut cursor)?;
    raw_integer(bytes, &mut cursor)?;
    skip_pdf_space(bytes, &mut cursor);
    if raw_slice(bytes, cursor, 3)? != b"obj" {
        return None;
    }
    cursor += 3;
    skip_pdf_space(bytes, &mut cursor);
    if raw_slice(bytes, cursor, 2)? != b"<<" {
        return None;
    }
    cursor += 2;
    let mut depth = 1_usize;
    let mut arrays = 0_usize;
    let mut length = None;
    while cursor < bytes.len() && depth > 0 {
        skip_pdf_space(bytes, &mut cursor);
        match *bytes.get(cursor)? {
            b'(' => {
                cursor += 1;
                let mut strings = 1_usize;
                while strings > 0 {
                    match *bytes.get(cursor)? {
                        b'\\' => {
                            cursor = cursor.checked_add(2)?;
                            continue;
                        }
                        b'(' => strings += 1,
                        b')' => strings = strings.saturating_sub(1),
                        _ => {}
                    }
                    cursor += 1;
                }
            }
            b'<' if bytes.get(cursor + 1) == Some(&b'<') => {
                depth += 1;
                cursor += 2;
            }
            b'<' => {
                cursor += 1;
                while *bytes.get(cursor)? != b'>' {
                    cursor += 1;
                }
                cursor += 1;
            }
            b'>' if bytes.get(cursor + 1) == Some(&b'>') => {
                depth = depth.checked_sub(1)?;
                cursor += 2;
            }
            b'[' => {
                arrays += 1;
                cursor += 1;
            }
            b']' => {
                arrays = arrays.checked_sub(1)?;
                cursor += 1;
            }
            b'/' => {
                cursor += 1;
                let start = cursor;
                while bytes
                    .get(cursor)
                    .is_some_and(|b| !is_pdf_name_boundary(*b) && *b != 0)
                {
                    cursor += 1;
                }
                if depth == 1 && arrays == 0 && bytes.get(start..cursor) == Some(b"Length") {
                    let mut value_end = cursor;
                    if let Some(first) = raw_integer(bytes, &mut value_end) {
                        let mut after = value_end;
                        let resolved = if let Some(generation) = raw_integer(bytes, &mut after) {
                            skip_pdf_space(bytes, &mut after);
                            if bytes.get(after) != Some(&b'R') {
                                return None;
                            }
                            let id = (u32::try_from(first).ok()?, u16::try_from(generation).ok()?);
                            resolve_integer_object(doc, &Object::Reference(id))?
                        } else {
                            first
                        };
                        if length.replace(usize::try_from(resolved).ok()?).is_some() {
                            return None;
                        }
                    }
                }
            }
            _ => cursor += 1,
        }
    }
    if arrays != 0 {
        return None;
    }
    skip_pdf_space(bytes, &mut cursor);
    if raw_slice(bytes, cursor, 6)? != b"stream" {
        return None;
    }
    cursor += 6;
    // Spaces before the required line ending are tolerated by PDF readers.
    while bytes.get(cursor).is_some_and(|b| matches!(b, b' ' | b'\t')) {
        cursor += 1;
    }
    if raw_slice(bytes, cursor, 2) == Some(b"\r\n") {
        cursor += 2;
    } else if bytes
        .get(cursor)
        .is_some_and(|b| matches!(b, b'\r' | b'\n'))
    {
        cursor += 1;
    } else {
        return None;
    }
    Some((cursor, length?))
}

fn raw_stream_is_sound(
    bytes: &[u8],
    object_offset: usize,
    document: &Document,
    parsed_stream: Option<&Stream>,
) -> bool {
    let Some((payload_start, length)) = raw_stream_header(bytes, object_offset, document) else {
        return false;
    };
    let Some(payload_end) = payload_start.checked_add(length) else {
        return false;
    };
    let Some(payload) = bytes.get(payload_start..payload_end) else {
        return false;
    };
    if parsed_stream.is_some_and(|stream| payload != stream.content) {
        return false;
    }
    let mut cursor = payload_end;
    if raw_slice(bytes, cursor, 2) == Some(b"\r\n") {
        cursor += 2;
    } else if bytes
        .get(cursor)
        .is_some_and(|b| matches!(b, b'\r' | b'\n'))
    {
        cursor += 1;
    }
    if raw_slice(bytes, cursor, 9) != Some(b"endstream") {
        return false;
    }
    cursor += 9;
    skip_pdf_space(bytes, &mut cursor);
    raw_slice(bytes, cursor, 6) == Some(b"endobj")
}

fn recompress_streams(
    document: &mut Document,
    options: &Options,
    report: &mut Report,
) -> Result<(), Error> {
    let ids: Vec<ObjectId> = document.objects.keys().copied().collect();
    for id in ids {
        let Some(Object::Stream(stream)) = document.objects.get_mut(&id) else {
            continue;
        };
        if !stream.allows_compression || is_generated_stream(stream) {
            continue;
        }

        // Decode parameters can carry predictors, colour/sample transforms, or
        // filter-specific values.  Re-encoding without reproducing those exact
        // transforms would change bytes after decoding, so those streams stay
        // byte-for-byte intact.
        if stream.dict.get(b"DecodeParms").is_ok() {
            continue;
        }

        let filter = match stream.dict.get(b"Filter") {
            Ok(Object::Name(name)) => Some(name.as_slice()),
            Ok(Object::Array(filters)) if filters.len() == 1 => {
                filters.first().and_then(|filter| filter.as_name().ok())
            }
            Ok(_) => None,
            Err(_) => None,
        };

        match filter {
            None => {
                if stream.dict.get(b"Filter").is_ok() {
                    // An unknown or malformed filter is never removed.
                    continue;
                }
                if stream.content.len() > options.max_decoded_stream_bytes {
                    report.warnings.push(format!(
                        "stream object {id:?} exceeds decoded stream limit; preserved"
                    ));
                    continue;
                }
                let encoded = encode_flate(&stream.content)?;
                if encoded.len() < stream.content.len() {
                    stream.dict.set("Filter", "FlateDecode");
                    stream.set_content(encoded);
                    report.streams_recompressed += 1;
                }
            }
            Some(b"FlateDecode" | b"Fl") => {
                let decoded =
                    match decode_flate_bounded(&stream.content, options.max_decoded_stream_bytes) {
                        Ok(decoded) => decoded,
                        Err(Error::LimitExceeded(_)) => {
                            report.warnings.push(format!(
                                "stream object {id:?} exceeds decoded stream limit; preserved"
                            ));
                            continue;
                        }
                        Err(error) => {
                            report.warnings.push(format!(
                                "stream object {id:?} could not be decoded ({error}); preserved"
                            ));
                            continue;
                        }
                    };
                let encoded = encode_flate(&decoded)?;
                if encoded.len() < stream.content.len() {
                    stream.set_content(encoded);
                    report.streams_recompressed += 1;
                }
            }
            Some(_) => {
                // Preserve every filter we do not implement.  In particular,
                // ASCII85/LZW/crypt chains are left entirely untouched.
            }
        }
    }
    Ok(())
}

fn encode_flate(content: &[u8]) -> Result<Vec<u8>, Error> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
    encoder.write_all(content)?;
    Ok(encoder.finish()?)
}

fn decode_flate_bounded(content: &[u8], limit: usize) -> Result<Vec<u8>, Error> {
    let mut decoder = ZlibDecoder::new(content);
    let mut decoded = Vec::with_capacity(content.len().saturating_mul(2).min(limit));
    let mut buffer = [0_u8; 32 * 1024];
    loop {
        let read = decoder
            .read(&mut buffer)
            .map_err(|error| Error::Pdf(format!("could not decode Flate stream: {error}")))?;
        if read == 0 {
            break;
        }
        if read > limit.saturating_sub(decoded.len()) {
            return Err(Error::LimitExceeded(format!(
                "decoded stream exceeds {} bytes",
                limit
            )));
        }
        decoded.extend_from_slice(buffer.get(..read).unwrap_or_default());
    }
    Ok(decoded)
}

fn is_generated_stream(stream: &Stream) -> bool {
    stream
        .dict
        .get(b"Type")
        .and_then(Object::as_name)
        .map(|name| name == b"XRef" || name == b"ObjStm")
        .unwrap_or(false)
}

/// Nesting beyond this is never deduplicated.
const MAX_DEDUP_DEPTH: usize = 64;

/// Return true when an indirect object is a pure value with no nested indirect
/// references.  Sharing such values cannot change the object graph or identity
/// visible to a PDF consumer.  Dictionaries carrying identity-sensitive keys
/// are excluded even when they happen to contain no references.
fn is_safe_dedup_object(object: &Object) -> bool {
    is_safe_dedup_object_inner(object, 0)
}

fn is_safe_dedup_object_inner(object: &Object, depth: usize) -> bool {
    if depth > MAX_DEDUP_DEPTH {
        return false;
    }
    let nested = |value: &Object| is_safe_dedup_object_inner(value, depth + 1);
    match object {
        Object::Null
        | Object::Boolean(_)
        | Object::Integer(_)
        | Object::Real(_)
        | Object::Name(_) => true,
        Object::String(_, _) => true,
        Object::Array(array) => array.iter().all(nested),
        Object::Dictionary(dictionary) => {
            if dictionary.iter().any(|(key, _)| {
                matches!(
                    key.as_slice(),
                    b"Type"
                        | b"Subtype"
                        | b"Parent"
                        | b"P"
                        | b"T"
                        | b"TU"
                        | b"TM"
                        | b"NM"
                        | b"A"
                        | b"Dest"
                        | b"V"
                        | b"Kids"
                        | b"Fields"
                        | b"Annots"
                        | b"StructParent"
                        | b"StructParents"
                )
            }) {
                return false;
            }
            dictionary.iter().all(|(_, value)| nested(value))
        }
        Object::Stream(stream) => {
            if stream.dict.iter().any(|(key, _)| {
                matches!(
                    key.as_slice(),
                    b"Type"
                        | b"Subtype"
                        | b"Parent"
                        | b"P"
                        | b"T"
                        | b"NM"
                        | b"StructParent"
                        | b"StructParents"
                        | b"Mask"
                        | b"SMask"
                )
            }) {
                return false;
            }
            stream.dict.iter().all(|(_, value)| nested(value))
        }
        Object::Reference(_) => false,
    }
}

/// Merge identical value objects.  Candidates are bucketed by a structural
/// hash and confirmed with full equality, so the pass is linear in the number
/// of objects; the hasher is randomly keyed so input cannot force collisions.
fn deduplicate_objects(document: &mut Document) -> usize {
    const MAX_CANONICAL_BYTES: usize = 1024 * 1024;
    const MAX_BUCKET_LEN: usize = 8;
    let hasher = RandomState::new();
    let mut buckets: HashMap<u64, Vec<ObjectId>> = HashMap::new();
    let mut replacements: BTreeMap<ObjectId, ObjectId> = BTreeMap::new();

    for (id, object) in &document.objects {
        if id.1 != 0
            || !is_safe_dedup_object(object)
            || !dedup_object_size(object, MAX_CANONICAL_BYTES)
        {
            continue;
        }
        let bucket = buckets
            .entry(structural_hash(object, &hasher, 0))
            .or_default();
        let canonical = bucket
            .iter()
            .find(|candidate| document.objects.get(candidate) == Some(object));
        if let Some(canonical) = canonical {
            replacements.insert(*id, *canonical);
        } else if bucket.len() < MAX_BUCKET_LEN {
            bucket.push(*id);
        }
    }

    if replacements.is_empty() {
        return 0;
    }

    for (_, value) in document.trailer.iter_mut() {
        replace_references(value, &replacements);
    }
    for object in document.objects.values_mut() {
        replace_references(object, &replacements);
    }
    for id in replacements.keys() {
        document.objects.remove(id);
    }
    replacements.len()
}

/// Hash of the value of an object, independent of dictionary key order (which
/// `Dictionary` equality also ignores).  Only called on objects that passed
/// [`is_safe_dedup_object`], so recursion is bounded; the cut-off is defensive.
fn structural_hash(object: &Object, build: &RandomState, depth: usize) -> u64 {
    let mut state = build.build_hasher();
    if depth > MAX_DEDUP_DEPTH {
        return state.finish();
    }
    let dictionary_hash = |dictionary: &Dictionary| {
        let mut combined = 0u64;
        for (key, value) in dictionary.iter() {
            let mut entry = build.build_hasher();
            key.hash(&mut entry);
            structural_hash(value, build, depth + 1).hash(&mut entry);
            combined = combined.wrapping_add(entry.finish());
        }
        combined
    };
    match object {
        Object::Null => 0u8.hash(&mut state),
        Object::Boolean(value) => (1u8, value).hash(&mut state),
        Object::Integer(value) => (2u8, value).hash(&mut state),
        Object::Real(value) => (3u8, value.to_bits()).hash(&mut state),
        Object::Name(name) => (4u8, name).hash(&mut state),
        Object::String(bytes, format) => (
            5u8,
            bytes,
            matches!(format, lopdf::StringFormat::Hexadecimal),
        )
            .hash(&mut state),
        Object::Array(array) => {
            6u8.hash(&mut state);
            array.len().hash(&mut state);
            for value in array {
                structural_hash(value, build, depth + 1).hash(&mut state);
            }
        }
        Object::Dictionary(dictionary) => {
            (7u8, dictionary.len(), dictionary_hash(dictionary)).hash(&mut state);
        }
        Object::Stream(stream) => {
            (
                8u8,
                stream.dict.len(),
                dictionary_hash(&stream.dict),
                &stream.content,
            )
                .hash(&mut state);
        }
        Object::Reference(id) => (9u8, id).hash(&mut state),
    }
    state.finish()
}

fn dedup_object_size(object: &Object, limit: usize) -> bool {
    fn visit(object: &Object, remaining: &mut usize, depth: usize) -> bool {
        if depth > MAX_DEDUP_DEPTH {
            return false;
        }
        match object {
            Object::String(bytes, _) | Object::Name(bytes) => {
                if bytes.len() > *remaining {
                    return false;
                }
                *remaining = remaining.saturating_sub(bytes.len());
                true
            }
            Object::Stream(stream) => {
                if stream.content.len() > *remaining {
                    return false;
                }
                *remaining = remaining.saturating_sub(stream.content.len());
                stream
                    .dict
                    .iter()
                    .all(|(_, value)| visit(value, remaining, depth + 1))
            }
            Object::Array(array) => array.iter().all(|value| visit(value, remaining, depth + 1)),
            Object::Dictionary(dictionary) => dictionary
                .iter()
                .all(|(_, value)| visit(value, remaining, depth + 1)),
            _ => true,
        }
    }

    let mut remaining = limit;
    visit(object, &mut remaining, 0)
}

/// Iterative, so direct-object nesting cannot exhaust the stack.
fn replace_references(root: &mut Object, replacements: &BTreeMap<ObjectId, ObjectId>) {
    let mut stack = vec![root];
    while let Some(object) = stack.pop() {
        match object {
            Object::Reference(id) => {
                if let Some(replacement) = replacements.get(id) {
                    *id = *replacement;
                }
            }
            Object::Array(array) => stack.extend(array.iter_mut()),
            Object::Dictionary(dictionary) => {
                stack.extend(dictionary.iter_mut().map(|(_, value)| value));
            }
            Object::Stream(stream) => {
                stack.extend(stream.dict.iter_mut().map(|(_, value)| value));
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::{Dictionary, Object, Stream};

    fn minimal_document(content: Vec<u8>) -> (Document, ObjectId, ObjectId) {
        let mut document = Document::with_version("1.4");
        let pages_id = document.new_object_id();
        let page_id = document.new_object_id();
        let content_id =
            document.add_object(Object::Stream(Stream::new(Dictionary::new(), content)));
        let font_id = document.new_object_id();
        document.objects.insert(
            font_id,
            Object::Dictionary(Dictionary::from_iter([
                (b"Type".to_vec(), Object::Name(b"Font".to_vec())),
                (b"Subtype".to_vec(), Object::Name(b"Type1".to_vec())),
                (b"BaseFont".to_vec(), Object::Name(b"Helvetica".to_vec())),
            ])),
        );
        document.objects.insert(
            pages_id,
            Object::Dictionary(Dictionary::from_iter([
                (b"Type".to_vec(), Object::Name(b"Pages".to_vec())),
                (
                    b"Kids".to_vec(),
                    Object::Array(vec![Object::Reference(page_id)]),
                ),
                (b"Count".to_vec(), Object::Integer(1)),
            ])),
        );
        document.objects.insert(
            page_id,
            Object::Dictionary(Dictionary::from_iter([
                (b"Type".to_vec(), Object::Name(b"Page".to_vec())),
                (b"Parent".to_vec(), Object::Reference(pages_id)),
                (
                    b"MediaBox".to_vec(),
                    Object::Array(vec![0.into(), 0.into(), 612.into(), 792.into()]),
                ),
                (b"Contents".to_vec(), Object::Reference(content_id)),
                (
                    b"Resources".to_vec(),
                    Object::Dictionary(Dictionary::from_iter([(
                        b"Font".to_vec(),
                        Object::Dictionary(Dictionary::from_iter([(
                            b"F1".to_vec(),
                            Object::Reference(font_id),
                        )])),
                    )])),
                ),
            ])),
        );
        let catalog_id = document.new_object_id();
        document.objects.insert(
            catalog_id,
            Object::Dictionary(Dictionary::from_iter([
                (b"Type".to_vec(), Object::Name(b"Catalog".to_vec())),
                (b"Pages".to_vec(), Object::Reference(pages_id)),
            ])),
        );
        document.trailer.set("Root", Object::Reference(catalog_id));
        (document, pages_id, page_id)
    }

    fn save(document: &mut Document) -> Vec<u8> {
        let mut bytes = Vec::new();
        document.save_to(&mut bytes).unwrap();
        bytes
    }

    fn minimal_pdf(content: Vec<u8>) -> Vec<u8> {
        save(&mut minimal_document(content).0)
    }

    fn keep_larger() -> Options {
        Options {
            keep_if_larger: true,
            ..Options::default()
        }
    }

    fn nested_arrays(depth: usize, innermost: Object) -> Object {
        let mut object = innermost;
        for _ in 0..depth {
            object = Object::Array(vec![object]);
        }
        object
    }

    #[test]
    fn defaults_are_lossless_and_bounded() {
        let options = Options::default();
        assert_eq!(options.preset, Preset::Lossless);
        assert!(!options.allow_lossy);
        assert_eq!(options.max_input_bytes, 256 * 1024 * 1024);
        assert_eq!(options.max_decoded_stream_bytes, 128 * 1024 * 1024);
    }

    #[test]
    fn plain_stream_is_recompressed_without_changing_pages() {
        let bytes =
            minimal_pdf(b"BT /F1 12 Tf 10 10 Td (hello hello hello hello hello) Tj ET".to_vec());
        let result = compress(
            &bytes,
            &Options {
                keep_if_larger: true,
                ..Options::default()
            },
        )
        .unwrap();
        let output = Document::load_mem(&result.bytes).unwrap();
        assert_eq!(output.get_pages().len(), 1);
        assert!(result.report.streams_recompressed >= 1);
    }

    #[test]
    fn larger_serialization_returns_original_by_default() {
        let bytes = minimal_pdf(b"BT ET".to_vec());
        let result = compress(&bytes, &Options::default()).unwrap();
        if result.report.used_original {
            assert_eq!(result.bytes, bytes);
            assert_eq!(result.report.output_bytes, bytes.len());
        }
    }

    #[test]
    fn invalid_limits_are_rejected() {
        let bytes = minimal_pdf(b"BT ET".to_vec());
        let options = Options {
            max_decoded_stream_bytes: 0,
            ..Options::default()
        };
        assert!(matches!(
            compress(&bytes, &options),
            Err(Error::InvalidOptions(_))
        ));
    }

    #[test]
    fn raw_byte_range_requires_an_integer_array_entry() {
        for signed in [
            b"<< /ByteRange [0 100 200 100] >>".as_slice(),
            b"<</ByteRange[0 100 200 100]>>",
            b"<< /ByteRange [ 0 10 20 10 ] >>",
            b"<< /ByteRange\r\n% comment\n[\n0 10 20 10] >>",
        ] {
            assert!(contains_raw_byte_range_entry(signed), "{signed:?}");
        }
        for unsigned in [
            b"(This PDF discusses /ByteRange signatures) Tj".as_slice(),
            b"<< /ByteRangeX [0 100 200 100] >>",
            b"<< /ByteRange /Other >>",
            b"<< /ByteRange [/A] >>",
            b"/ByteRange",
        ] {
            assert!(!contains_raw_byte_range_entry(unsigned), "{unsigned:?}");
        }
    }

    #[test]
    fn unused_dangling_reference_is_nulled_with_a_warning() {
        let (mut document, _, page_id) = minimal_document(b"BT ET".to_vec());
        document
            .get_object_mut(page_id)
            .and_then(Object::as_dict_mut)
            .unwrap()
            .set("Thumb", Object::Reference((999, 0)));
        let bytes = save(&mut document);

        let result = compress(&bytes, &keep_larger()).unwrap();
        assert!(result
            .report
            .warnings
            .iter()
            .any(|warning| warning.contains("1 reference(s) to missing objects")));
        assert_eq!(result.report.compatibility_repairs, 0);
        assert!(!result.bytes.windows(7).any(|window| window == b"999 0 R"));
        assert_eq!(
            Document::load_mem(&result.bytes).unwrap().get_pages().len(),
            1
        );
    }

    #[test]
    fn dangling_references_in_trailer_and_nested_values_are_nulled() {
        let (mut document, _, page_id) = minimal_document(b"BT ET".to_vec());
        document
            .get_object_mut(page_id)
            .and_then(Object::as_dict_mut)
            .unwrap()
            .set(
                "Extra",
                Object::Array(vec![Object::Dictionary(Dictionary::from_iter([(
                    b"A".to_vec(),
                    Object::Reference((998, 0)),
                )]))]),
            );
        document.trailer.set("Info", Object::Reference((997, 0)));
        let root = document.trailer.get(b"Root").unwrap().clone();

        assert_eq!(nullify_dangling_references(&mut document), 2);
        assert_eq!(document.trailer.get(b"Info").unwrap(), &Object::Null);
        assert_eq!(document.trailer.get(b"Root").unwrap(), &root);
        validate_document_structure(&document).unwrap();
    }

    #[test]
    fn missing_root_target_is_an_error() {
        let (mut document, _, _) = minimal_document(b"BT ET".to_vec());
        document.trailer.set("Root", Object::Reference((999, 0)));
        let bytes = save(&mut document);
        assert!(compress(&bytes, &keep_larger()).is_err());

        let (mut document, _, _) = minimal_document(b"BT ET".to_vec());
        document.trailer.set("Root", Object::Reference((999, 0)));
        assert_eq!(nullify_dangling_references(&mut document), 0);
        assert!(validate_document_structure(&document).is_err());
    }

    #[test]
    fn dangling_page_tree_kid_is_an_error() {
        let (mut document, pages_id, _) = minimal_document(b"BT ET".to_vec());
        document
            .get_object_mut(pages_id)
            .and_then(Object::as_dict_mut)
            .unwrap()
            .set(
                "Kids",
                Object::Array(vec![Object::Reference((2, 0)), Object::Reference((999, 0))]),
            );
        let bytes = save(&mut document);
        assert!(matches!(
            compress(&bytes, &keep_larger()),
            Err(Error::InvalidInput(_))
        ));
    }

    #[test]
    fn deeply_nested_input_is_rejected_before_parsing() {
        for (open, close) in [(b"[".as_slice(), b"]".as_slice()), (b"<</A ", b">>")] {
            let mut bytes = b"%PDF-1.4\n1 0 obj\n".to_vec();
            for _ in 0..5000 {
                bytes.extend_from_slice(open);
            }
            for _ in 0..5000 {
                bytes.extend_from_slice(close);
            }
            bytes.extend_from_slice(b"\nendobj\ntrailer\n<< /Root 1 0 R >>\n%%EOF\n");
            assert!(matches!(
                compress(&bytes, &Options::default()),
                Err(Error::LimitExceeded(_))
            ));
        }
    }

    #[test]
    fn preflight_ignores_nesting_inside_strings_and_stream_payloads() {
        let mut bytes = b"1 0 obj\n<< /Length 5000 /S (".to_vec();
        bytes.extend(std::iter::repeat_n(b'[', 5000));
        bytes.extend_from_slice(b") >>\nstream\n");
        bytes.extend(std::iter::repeat_n(b'[', 5000));
        bytes.extend_from_slice(b"\nendstream\nendobj\n<4142[[[>");
        preflight_raw_structure(&bytes, &Options::default(), false).unwrap();
    }

    #[test]
    fn preflight_rejects_encryption_and_unbounded_xref_widths() {
        assert!(matches!(
            preflight_raw_structure(
                b"trailer\n<< /Root 1 0 R /Encrypt 5 0 R >>\n",
                &Options::default(),
                false
            ),
            Err(Error::Unsupported(_))
        ));
        for widths in ["[0 0 0]", "[1 99999999999 1]", "[-1 2 1]"] {
            let bytes = format!(
                "4 0 obj\n<< /Type /XRef /W {widths} /Size 4 >>\nstream\nxx\nendstream\nendobj\n"
            );
            assert!(matches!(
                preflight_raw_structure(bytes.as_bytes(), &Options::default(), false),
                Err(Error::InvalidInput(_))
            ));
        }
        preflight_raw_structure(
            b"4 0 obj\n<< /Type /XRef /W [1 2 1] /Size 4 >>\nstream\nxx\nendstream\nendobj\n",
            &Options::default(),
            false,
        )
        .unwrap();
    }

    #[test]
    fn preflight_rejects_object_streams_it_cannot_bound() {
        for filter in ["/LZWDecode", "[/ASCII85Decode /FlateDecode]"] {
            let bytes = format!(
                "4 0 obj\n<< /Type /ObjStm /N 1 /First 4 /Filter {filter} >>\nstream\nxxxx\nendstream\nendobj\n"
            );
            assert!(matches!(
                preflight_raw_structure(bytes.as_bytes(), &Options::default(), false),
                Err(Error::Unsupported(_))
            ));
        }
    }

    #[test]
    fn deeply_nested_direct_objects_do_not_overflow_the_stack() {
        std::thread::Builder::new()
            .stack_size(64 * 1024 * 1024)
            .spawn(|| {
                let (mut document, _, _) = minimal_document(b"BT ET".to_vec());
                let deep = nested_arrays(5000, Object::Reference((999, 0)));
                assert!(validate_direct_references(&deep, &document).is_err());
                assert!(object_contains_signature(&deep));
                assert!(!is_safe_dedup_object(&deep));
                assert!(!dedup_object_size(&deep, usize::MAX));
                let id = document.add_object(deep);
                document.trailer.set("Deep", Object::Reference(id));

                assert_eq!(nullify_dangling_references(&mut document), 1);
                assert!(validate_document_structure(&document).is_err());
                assert_eq!(prune_unreachable_objects(&mut document), 0);
                assert_eq!(deduplicate_objects(&mut document), 0);
                let mut replaced = nested_arrays(5000, Object::Reference(id));
                replace_references(&mut replaced, &BTreeMap::from([(id, (1, 0))]));
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn identical_value_objects_are_deduplicated() {
        let (mut document, _, page_id) = minimal_document(b"BT ET".to_vec());
        let value = || Object::Array(vec![1.into(), 2.into(), Object::Name(b"X".to_vec())]);
        let first = document.add_object(value());
        let second = document.add_object(value());
        let other = document.add_object(Object::Array(vec![3.into()]));
        document
            .get_object_mut(page_id)
            .and_then(Object::as_dict_mut)
            .unwrap()
            .set(
                "Extra",
                Object::Array(vec![
                    Object::Reference(first),
                    Object::Reference(second),
                    Object::Reference(other),
                ]),
            );

        assert_eq!(deduplicate_objects(&mut document), 1);
        assert!(document.objects.contains_key(&first));
        assert!(!document.objects.contains_key(&second));
        let extra = document
            .get_object(page_id)
            .and_then(Object::as_dict)
            .unwrap()
            .get(b"Extra")
            .and_then(Object::as_array)
            .unwrap();
        assert_eq!(extra[0], extra[1]);
        assert_ne!(extra[0], extra[2]);
    }

    #[test]
    fn dedup_handles_many_objects() {
        let (mut document, _, page_id) = minimal_document(b"BT ET".to_vec());
        let mut refs = Vec::new();
        for round in 0..2 {
            for value in 0..20_000_i64 {
                let id = document.add_object(Object::Array(vec![value.into(), round.into()]));
                if round == 0 {
                    refs.push(Object::Reference(id));
                }
            }
        }
        for value in 0..20_000_i64 {
            let id = document.add_object(Object::Array(vec![value.into(), 0.into()]));
            refs.push(Object::Reference(id));
        }
        document
            .get_object_mut(page_id)
            .and_then(Object::as_dict_mut)
            .unwrap()
            .set("Extra", Object::Array(refs));
        assert_eq!(deduplicate_objects(&mut document), 20_000);
    }
}
