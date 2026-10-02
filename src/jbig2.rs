//! Bounded decoding of embedded JBIG2 image streams.
//!
//! PDF stores JBIG2 image data in the embedded organisation described by
//! ISO/IEC 14492.  The image dictionary supplies the width and height while
//! the stream contains segments (and may refer to a separate `/JBIG2Globals`
//! stream).  [`hayro_jbig2`] is a pure-Rust decoder for that format.  This
//! module keeps the decoder behind a small boundary and, before asking it to
//! allocate a page bitmap, checks the dimensions and packed output size from
//! the PDF dictionary.
//!
//! The returned samples are packed most-significant-bit first, one row after
//! another.  A set bit means that the JBIG2 page bitmap marked that pixel
//! black; unset padding bits at the end of each row are zero.  PDF callers
//! must apply the image's `/Decode` and image-mask rules when interpreting the
//! samples.  Keeping that mapping outside this decoder is important because
//! `/ImageMask` and ordinary `/DeviceGray` images use different paint rules.

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::io::{Read, Write};

use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use hayro_jbig2::{DecodeError, Decoder, Image};
use lopdf::{Dictionary, Document, Object, ObjectId};

use crate::{Options, Report};

/// The largest encoded JBIG2 payload accepted by this boundary, including an
/// optional globals stream.  The caller's decoded-output limit is enforced in
/// addition to this input limit.
pub const MAX_JBIG2_INPUT_BYTES: usize = 64 * 1024 * 1024;

/// Hard cap for the working bitmaps that the upstream decoder may retain.
///
/// `hayro-jbig2` allocates its page bitmap and any retained intermediate
/// regions internally.  The preflight below accepts only the generic and
/// generic-refinement segment families needed for compatibility repairs and
/// accounts for all of their possible bitmap allocations before decoding.
/// Keeping this cap independent from the caller's general PDF stream limit
/// prevents a large global stream limit from becoming a JBIG2 bitmap limit.
const MAX_JBIG2_WORKING_BYTES: usize = 64 * 1024 * 1024;
const MAX_JBIG2_SEGMENTS: usize = 65_536;
const MAX_JBIG2_REFERENCES_PER_SEGMENT: usize = 4_096;
const JBIG2_CONTEXT_BYTES: usize = 1 << 20;
const JBIG2_SEGMENT_METADATA_BYTES: usize = 128;

/// A decoded one-bit JBIG2 image in PDF row-major packed form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedImage {
    width: u32,
    height: u32,
    stride: usize,
    samples: Vec<u8>,
}

impl DecodedImage {
    /// Width of the decoded bitmap in pixels.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// Height of the decoded bitmap in pixels.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// Number of packed bytes occupied by each row.
    #[must_use]
    pub const fn stride(&self) -> usize {
        self.stride
    }

    /// Borrow the packed samples.  Bits are most-significant-bit first and a
    /// set bit denotes a black pixel in the JBIG2 page bitmap.
    #[must_use]
    pub fn samples(&self) -> &[u8] {
        &self.samples
    }

    /// Consume the image and return its packed samples.
    #[must_use]
    pub fn into_samples(self) -> Vec<u8> {
        self.samples
    }
}

/// Errors raised while parsing or bounded-decoding an embedded JBIG2 stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Jbig2Error {
    /// The encoded stream(s) exceed [`MAX_JBIG2_INPUT_BYTES`].
    InputTooLarge { actual: usize, max: usize },
    /// A zero PDF dimension was supplied.
    EmptyDimensions,
    /// PDF dimensions cannot be represented by the JBIG2 decoder.
    InvalidDimensions { width: u32, height: u32 },
    /// The packed output would exceed the caller's decoded-stream budget.
    OutputTooLarge { bytes: usize, max: usize },
    /// The segment families or representation are outside the bounded
    /// compatibility subset handled by this module.
    UnsupportedStructure,
    /// The segment headers or payload boundaries are malformed.
    MalformedStructure,
    /// The upstream decoder's possible bitmap working set exceeds the hard
    /// compatibility-repair limit.
    WorkingSetTooLarge { bytes: usize, max: usize },
    /// The dimensions in the JBIG2 page-information segment differ from the
    /// dimensions declared by the PDF image dictionary.
    DimensionMismatch {
        expected_width: u32,
        expected_height: u32,
        actual_width: u32,
        actual_height: u32,
    },
    /// The decoder emitted a malformed row sequence or pixel count.
    InvalidOutput,
    /// The pure-Rust decoder rejected the JBIG2 syntax or a coding feature.
    Decode(DecodeError),
}

impl fmt::Display for Jbig2Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InputTooLarge { actual, max } => {
                write!(f, "JBIG2 input is {actual} bytes, exceeding the {max}-byte limit")
            }
            Self::EmptyDimensions => write!(f, "JBIG2 image has an empty dimension"),
            Self::InvalidDimensions { width, height } => write!(
                f,
                "JBIG2 dimensions {width}x{height} are outside the supported range"
            ),
            Self::OutputTooLarge { bytes, max } => write!(
                f,
                "packed JBIG2 output is {bytes} bytes, exceeding the {max}-byte limit"
            ),
            Self::UnsupportedStructure => write!(
                f,
                "JBIG2 uses a segment family outside the bounded compatibility subset"
            ),
            Self::MalformedStructure => write!(f, "JBIG2 segment structure is malformed"),
            Self::WorkingSetTooLarge { bytes, max } => write!(
                f,
                "JBIG2 decoder working set is {bytes} bytes, exceeding the {max}-byte limit"
            ),
            Self::DimensionMismatch {
                expected_width,
                expected_height,
                actual_width,
                actual_height,
            } => write!(
                f,
                "JBIG2 dimensions are {actual_width}x{actual_height}, PDF declares {expected_width}x{expected_height}"
            ),
            Self::InvalidOutput => write!(f, "JBIG2 decoder emitted an invalid pixel sequence"),
            Self::Decode(error) => write!(f, "JBIG2 decode failed: {error}"),
        }
    }
}

impl std::error::Error for Jbig2Error {}

impl From<DecodeError> for Jbig2Error {
    fn from(error: DecodeError) -> Self {
        Self::Decode(error)
    }
}

/// Decode an embedded PDF JBIG2 image into packed one-bit samples.
///
/// `data` is the image stream after any PDF stream filters that precede
/// `JBIG2Decode` have been removed.  `globals`, when present, is the decoded
/// content of the `/JBIG2Globals` stream.  `expected_width` and
/// `expected_height` come from the image dictionary and are checked against
/// the JBIG2 page-information segment before output is allocated.
///
/// `max_decoded_bytes` limits the packed row-major output.  The decoder also
/// rejects encoded input larger than [`MAX_JBIG2_INPUT_BYTES`] so a tiny image
/// cannot cause an unbounded segment parse.  A successful result contains
/// exactly `ceil(width / 8) * height` bytes.
pub fn decode_embedded(
    data: &[u8],
    globals: Option<&[u8]>,
    expected_width: u32,
    expected_height: u32,
    max_decoded_bytes: usize,
) -> Result<DecodedImage, Jbig2Error> {
    if expected_width == 0 || expected_height == 0 {
        return Err(Jbig2Error::EmptyDimensions);
    }

    // hayro-jbig2 itself caps each dimension at u16::MAX.  Rejecting larger
    // values before parsing is both clearer to callers and avoids accepting a
    // PDF dictionary that could never match a decodable JBIG2 page.
    if expected_width > u16::MAX as u32 || expected_height > u16::MAX as u32 {
        return Err(Jbig2Error::InvalidDimensions {
            width: expected_width,
            height: expected_height,
        });
    }

    let encoded_len = data
        .len()
        .checked_add(globals.map_or(0, <[u8]>::len))
        .ok_or(Jbig2Error::InputTooLarge {
            actual: usize::MAX,
            max: MAX_JBIG2_INPUT_BYTES,
        })?;
    if encoded_len > MAX_JBIG2_INPUT_BYTES {
        return Err(Jbig2Error::InputTooLarge {
            actual: encoded_len,
            max: MAX_JBIG2_INPUT_BYTES,
        });
    }

    let stride = packed_stride(expected_width).ok_or(Jbig2Error::InvalidDimensions {
        width: expected_width,
        height: expected_height,
    })?;
    let output_len = stride
        .checked_mul(usize::try_from(expected_height).map_err(|_| {
            Jbig2Error::InvalidDimensions {
                width: expected_width,
                height: expected_height,
            }
        })?)
        .ok_or(Jbig2Error::InvalidDimensions {
            width: expected_width,
            height: expected_height,
        })?;
    if output_len > max_decoded_bytes {
        return Err(Jbig2Error::OutputTooLarge {
            bytes: output_len,
            max: max_decoded_bytes,
        });
    }

    let _preflight = preflight_embedded(
        data,
        globals,
        expected_width,
        expected_height,
        output_len,
        max_decoded_bytes,
    )?;

    let image = Image::new_embedded(data, globals)?;
    let actual_width = image.width();
    let actual_height = image.height();
    if actual_width != expected_width || actual_height != expected_height {
        return Err(Jbig2Error::DimensionMismatch {
            expected_width,
            expected_height,
            actual_width,
            actual_height,
        });
    }

    let mut decoder = PackedDecoder::new(expected_width, expected_height, stride, output_len);
    image.decode(&mut decoder)?;
    if decoder.invalid || decoder.row != expected_height || decoder.column != 0 {
        return Err(Jbig2Error::InvalidOutput);
    }

    Ok(DecodedImage {
        width: expected_width,
        height: expected_height,
        stride,
        samples: decoder.samples,
    })
}

#[derive(Default)]
struct Preflight {
    segment_count: usize,
    segment_numbers: HashSet<u32>,
    segment_types: BTreeMap<u32, u8>,
    page_count: usize,
    page_dimensions: Option<(u32, u32)>,
    bitmap_bytes: usize,
    metadata_bytes: usize,
    has_region: bool,
    has_intermediate_generic: bool,
}

fn preflight_embedded(
    data: &[u8],
    globals: Option<&[u8]>,
    expected_width: u32,
    expected_height: u32,
    output_len: usize,
    max_decoded_bytes: usize,
) -> Result<Preflight, Jbig2Error> {
    let mut state = Preflight::default();
    if let Some(globals) = globals {
        preflight_segment_stream(globals, true, &mut state, false)?;
    }
    preflight_segment_stream(data, false, &mut state, false)?;

    // hayro sorts segments by number before decoding.  The first pass above
    // records every segment ID/type, so validate references against that
    // complete map instead of assuming the serialized order is sorted.
    if let Some(globals) = globals {
        preflight_segment_stream(globals, true, &mut state, true)?;
    }
    preflight_segment_stream(data, false, &mut state, true)?;

    if state.page_count != 1 || !state.has_region {
        return Err(Jbig2Error::MalformedStructure);
    }
    let (actual_width, actual_height) = state
        .page_dimensions
        .ok_or(Jbig2Error::MalformedStructure)?;
    if actual_width != expected_width || actual_height != expected_height {
        return Err(Jbig2Error::DimensionMismatch {
            expected_width,
            expected_height,
            actual_width,
            actual_height,
        });
    }

    let page_bytes = bitmap_storage_bytes(expected_width, expected_height).ok_or(
        Jbig2Error::InvalidDimensions {
            width: expected_width,
            height: expected_height,
        },
    )?;
    let max_working = max_decoded_bytes.min(MAX_JBIG2_WORKING_BYTES);
    let working_bytes = page_bytes
        .checked_add(state.bitmap_bytes)
        .and_then(|bytes| bytes.checked_add(state.metadata_bytes))
        .and_then(|bytes| bytes.checked_add(JBIG2_CONTEXT_BYTES))
        .and_then(|bytes| bytes.checked_add(output_len))
        .ok_or(Jbig2Error::WorkingSetTooLarge {
            bytes: usize::MAX,
            max: max_working,
        })?;
    if working_bytes > max_working {
        return Err(Jbig2Error::WorkingSetTooLarge {
            bytes: working_bytes,
            max: max_working,
        });
    }

    Ok(state)
}

fn preflight_segment_stream(
    data: &[u8],
    is_globals: bool,
    state: &mut Preflight,
    validate_only: bool,
) -> Result<(), Jbig2Error> {
    let mut offset = 0_usize;
    while offset < data.len() {
        if !validate_only && state.segment_count >= MAX_JBIG2_SEGMENTS {
            return Err(Jbig2Error::WorkingSetTooLarge {
                bytes: usize::MAX,
                max: MAX_JBIG2_WORKING_BYTES,
            });
        }

        let segment_number = read_u32(data, &mut offset).ok_or(Jbig2Error::MalformedStructure)?;
        let flags = read_byte(data, &mut offset).ok_or(Jbig2Error::MalformedStructure)?;
        let segment_type = flags & 0x3F;
        if !supported_segment_type(segment_type, is_globals) {
            return Err(Jbig2Error::UnsupportedStructure);
        }

        let count_and_retention =
            read_byte(data, &mut offset).ok_or(Jbig2Error::MalformedStructure)?;
        let short_count = count_and_retention >> 5;
        if short_count == 5 || short_count == 6 {
            return Err(Jbig2Error::MalformedStructure);
        }
        let referred_count = if short_count < 7 {
            short_count as usize
        } else {
            let low = u32::from(count_and_retention & 0x1F);
            let [high, middle, last] =
                read_array::<3>(data, &mut offset).ok_or(Jbig2Error::MalformedStructure)?;
            let count =
                (low << 24) | (u32::from(high) << 16) | (u32::from(middle) << 8) | u32::from(last);
            usize::try_from(count).map_err(|_| Jbig2Error::MalformedStructure)?
        };
        if referred_count > MAX_JBIG2_REFERENCES_PER_SEGMENT {
            return Err(Jbig2Error::UnsupportedStructure);
        }
        if short_count == 7 {
            let retention_bytes = referred_count
                .checked_add(1)
                .ok_or(Jbig2Error::MalformedStructure)?
                .div_ceil(8);
            read_bytes(data, &mut offset, retention_bytes).ok_or(Jbig2Error::MalformedStructure)?;
        }

        let reference_width = if segment_number <= 256 {
            1
        } else if segment_number <= 65_536 {
            2
        } else {
            4
        };
        let mut references = Vec::with_capacity(referred_count);
        for _ in 0..referred_count {
            let referenced = match reference_width {
                1 => u32::from(read_byte(data, &mut offset).ok_or(Jbig2Error::MalformedStructure)?),
                2 => u32::from(read_u16(data, &mut offset).ok_or(Jbig2Error::MalformedStructure)?),
                _ => read_u32(data, &mut offset).ok_or(Jbig2Error::MalformedStructure)?,
            };
            if referenced >= segment_number {
                return Err(Jbig2Error::MalformedStructure);
            }
            references.push(referenced);
        }

        let page_association = if flags & 0x40 != 0 {
            read_u32(data, &mut offset).ok_or(Jbig2Error::MalformedStructure)?
        } else {
            u32::from(read_byte(data, &mut offset).ok_or(Jbig2Error::MalformedStructure)?)
        };
        if (is_globals && page_association != 0) || (!is_globals && page_association > 1) {
            return Err(Jbig2Error::UnsupportedStructure);
        }

        let data_length = read_u32(data, &mut offset).ok_or(Jbig2Error::MalformedStructure)?;
        if data_length == u32::MAX {
            // Unknown-length immediate generic regions require a marker scan
            // in the upstream parser.  Rejecting them keeps this preflight
            // linear and bounded; the caller preserves the original image.
            return Err(Jbig2Error::UnsupportedStructure);
        }
        let data_length =
            usize::try_from(data_length).map_err(|_| Jbig2Error::MalformedStructure)?;
        let payload =
            read_bytes(data, &mut offset, data_length).ok_or(Jbig2Error::MalformedStructure)?;

        if validate_only {
            validate_references(state, segment_type, segment_number, &references)?;
            continue;
        }

        state.segment_count = state
            .segment_count
            .checked_add(1)
            .ok_or(Jbig2Error::MalformedStructure)?;
        if !state.segment_numbers.insert(segment_number) {
            return Err(Jbig2Error::MalformedStructure);
        }
        state.segment_types.insert(segment_number, segment_type);
        state.metadata_bytes = state
            .metadata_bytes
            .checked_add(
                JBIG2_SEGMENT_METADATA_BYTES
                    .checked_add(
                        referred_count
                            .checked_mul(4)
                            .ok_or(Jbig2Error::MalformedStructure)?,
                    )
                    .ok_or(Jbig2Error::MalformedStructure)?,
            )
            .ok_or(Jbig2Error::MalformedStructure)?;

        match segment_type {
            48 => {
                if is_globals || payload.len() < 19 {
                    return Err(Jbig2Error::MalformedStructure);
                }
                if state.page_count != 0 {
                    return Err(Jbig2Error::MalformedStructure);
                }
                let mut cursor = 0_usize;
                let width = read_u32(payload, &mut cursor).ok_or(Jbig2Error::MalformedStructure)?;
                let height =
                    read_u32(payload, &mut cursor).ok_or(Jbig2Error::MalformedStructure)?;
                if width == 0 || height == 0 || height == u32::MAX {
                    return Err(Jbig2Error::UnsupportedStructure);
                }
                if width > u16::MAX as u32 || height > u16::MAX as u32 {
                    return Err(Jbig2Error::InvalidDimensions { width, height });
                }
                state.page_count += 1;
                state.page_dimensions = Some((width, height));
            }
            36 | 38 | 39 | 40 | 42 | 43 => {
                if payload.len() < 17 {
                    return Err(Jbig2Error::MalformedStructure);
                }
                let mut cursor = 0_usize;
                let width = read_u32(payload, &mut cursor).ok_or(Jbig2Error::MalformedStructure)?;
                let height =
                    read_u32(payload, &mut cursor).ok_or(Jbig2Error::MalformedStructure)?;
                let x = read_u32(payload, &mut cursor).ok_or(Jbig2Error::MalformedStructure)?;
                let y = read_u32(payload, &mut cursor).ok_or(Jbig2Error::MalformedStructure)?;
                // A region origin beyond the largest page can never be visible.
                // Bounding it keeps every offset sum the upstream decoder
                // forms (region origin + pixel position + adaptive-template
                // offset) far from i32 overflow.
                if width == 0 || height == 0 || x > u16::MAX as u32 || y > u16::MAX as u32 {
                    return Err(Jbig2Error::UnsupportedStructure);
                }
                if width > u16::MAX as u32 || height > u16::MAX as u32 {
                    return Err(Jbig2Error::InvalidDimensions { width, height });
                }
                let bitmap_bytes = bitmap_storage_bytes(width, height)
                    .ok_or(Jbig2Error::InvalidDimensions { width, height })?;
                state.bitmap_bytes = state.bitmap_bytes.checked_add(bitmap_bytes).ok_or(
                    Jbig2Error::WorkingSetTooLarge {
                        bytes: usize::MAX,
                        max: MAX_JBIG2_WORKING_BYTES,
                    },
                )?;
                state.has_region = true;
                if segment_type == 36 {
                    state.has_intermediate_generic = true;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn validate_references(
    state: &Preflight,
    segment_type: u8,
    _segment_number: u32,
    references: &[u32],
) -> Result<(), Jbig2Error> {
    if !matches!(segment_type, 40 | 42 | 43) {
        return Ok(());
    }

    // The generic refinement decoder consumes at most one reference.  A
    // missing reference currently falls back to the page bitmap in hayro;
    // accepting that fallback would turn malformed input into a different
    // image, so require every explicit reference to be a stored bitmap.
    if references.len() > 1 {
        return Err(Jbig2Error::UnsupportedStructure);
    }
    if let Some(reference) = references.first() {
        let target_type = state
            .segment_types
            .get(reference)
            .copied()
            .ok_or(Jbig2Error::MalformedStructure)?;
        if !matches!(target_type, 36 | 40) {
            return Err(Jbig2Error::UnsupportedStructure);
        }
    }
    Ok(())
}

fn supported_segment_type(segment_type: u8, is_globals: bool) -> bool {
    match segment_type {
        36 | 38 | 39 | 40 | 42 | 43 => true,
        48 => !is_globals,
        49..=51 => !is_globals,
        _ => false,
    }
}

fn bitmap_storage_bytes(width: u32, height: u32) -> Option<usize> {
    let words_per_row = width.div_ceil(32) as usize;
    words_per_row.checked_mul(4)?.checked_mul(height as usize)
}

fn packed_stride(width: u32) -> Option<usize> {
    usize::try_from(width.div_ceil(8)).ok()
}

fn read_byte(data: &[u8], offset: &mut usize) -> Option<u8> {
    let byte = *data.get(*offset)?;
    *offset += 1;
    Some(byte)
}

fn read_bytes<'a>(data: &'a [u8], offset: &mut usize, len: usize) -> Option<&'a [u8]> {
    let end = offset.checked_add(len)?;
    let bytes = data.get(*offset..end)?;
    *offset = end;
    Some(bytes)
}

fn read_array<const N: usize>(data: &[u8], offset: &mut usize) -> Option<[u8; N]> {
    read_bytes(data, offset, N)?.try_into().ok()
}

fn read_u16(data: &[u8], offset: &mut usize) -> Option<u16> {
    read_array(data, offset).map(u16::from_be_bytes)
}

fn read_u32(data: &[u8], offset: &mut usize) -> Option<u32> {
    read_array(data, offset).map(u32::from_be_bytes)
}

struct Jbig2Candidate {
    width: u32,
    height: u32,
    data: Vec<u8>,
    globals: Option<Vec<u8>>,
}

/// Replace bounded, decodable PDF JBIG2 image streams with Flate streams.
///
/// This is a compatibility repair rather than a size optimisation: the
/// resulting stream is deliberately retained even when its Flate payload is
/// larger than the JBIG2 payload.  A number of PDF consumers reject valid
/// JBIG2 intermediate generic-region segments, while the pure-Rust decoder
/// above handles those segments and gives us the exact one-bit page bitmap.
/// Unsupported segment families, malformed dictionaries, and resource-limit
/// failures are reported as warnings and leave the original object untouched.
pub fn normalize_images(
    document: &mut Document,
    options: &Options,
    report: &mut Report,
) -> Result<(), crate::Error> {
    let image_ids: Vec<ObjectId> = document
        .objects
        .iter()
        .filter_map(|(id, object)| {
            let stream = object.as_stream().ok()?;
            let subtype = stream.dict.get(b"Subtype").ok()?.as_name().ok()?;
            (subtype == b"Image").then_some(*id)
        })
        .collect();

    let mut warnings = Vec::new();
    for image_id in image_ids {
        let candidate =
            match inspect_candidate(document, image_id, options.max_decoded_stream_bytes) {
                Ok(Some(candidate)) => candidate,
                Ok(None) => continue,
                Err(reason) => {
                    warnings.push(format!(
                        "JBIG2 image object {} {} was preserved: {reason}",
                        image_id.0, image_id.1
                    ));
                    continue;
                }
            };

        // Keep already-compatible JBIG2 streams byte-for-byte.  The repair is
        // needed for the intermediate generic-region family (segment type
        // 36), which a number of PDF consumers reject; immediate generic and
        // refinement-only streams have established renderer semantics and are
        // left in their original representation.
        let output_len = packed_stride(candidate.width)
            .and_then(|stride| stride.checked_mul(usize::try_from(candidate.height).ok()?))
            .unwrap_or(usize::MAX);
        let needs_repair = match preflight_embedded(
            &candidate.data,
            candidate.globals.as_deref(),
            candidate.width,
            candidate.height,
            output_len,
            options.max_decoded_stream_bytes,
        ) {
            Ok(preflight) => preflight.has_intermediate_generic,
            Err(reason) => {
                warnings.push(format!(
                    "JBIG2 image object {} {} was preserved: {reason}",
                    image_id.0, image_id.1
                ));
                continue;
            }
        };
        if !needs_repair {
            continue;
        }

        let decoded = match decode_embedded(
            &candidate.data,
            candidate.globals.as_deref(),
            candidate.width,
            candidate.height,
            options.max_decoded_stream_bytes,
        ) {
            Ok(decoded) => decoded,
            Err(reason) => {
                warnings.push(format!(
                    "JBIG2 image object {} {} was preserved: {reason}",
                    image_id.0, image_id.1
                ));
                continue;
            }
        };

        // hayro's bitmap uses 1 for black.  PDF image samples use the inverse
        // convention for JBIG2Decode (the default DeviceGray decode maps 0 to
        // black), so invert only meaningful bits and clear row padding.
        let samples = pdf_samples(decoded);
        let encoded = match flate_encode(&samples, options.max_decoded_stream_bytes) {
            Ok(encoded) => encoded,
            Err(reason) => {
                warnings.push(format!(
                    "JBIG2 image object {} {} was preserved: {reason}",
                    image_id.0, image_id.1
                ));
                continue;
            }
        };

        let stream = match document
            .get_object_mut(image_id)
            .and_then(Object::as_stream_mut)
        {
            Ok(stream) => stream,
            Err(error) => {
                warnings.push(format!(
                    "JBIG2 image object {} {} was preserved: could not update stream ({error})",
                    image_id.0, image_id.1
                ));
                continue;
            }
        };
        stream
            .dict
            .set("Filter", Object::Name(b"FlateDecode".to_vec()));
        stream.dict.remove(b"DecodeParms");
        stream.set_content(encoded);
        report.compatibility_repairs = report.compatibility_repairs.saturating_add(1);
    }

    report.warnings.extend(warnings);
    Ok(())
}

fn inspect_candidate(
    document: &Document,
    image_id: ObjectId,
    limit: usize,
) -> Result<Option<Jbig2Candidate>, String> {
    let object = document
        .get_object(image_id)
        .map_err(|error| format!("could not resolve object ({error})"))?;
    let stream = object
        .as_stream()
        .map_err(|_| "object is not an image stream".to_string())?;
    let filters = filter_names(document, &stream.dict)?;
    if filters.first().map(Vec::as_slice) != Some(b"JBIG2Decode".as_slice()) {
        return Ok(None);
    }
    if filters.len() != 1 {
        return Err("JBIG2 filter chains are preserved conservatively".to_string());
    }
    if stream.content.len() > MAX_JBIG2_INPUT_BYTES {
        return Err(format!(
            "encoded stream exceeds the {}-byte JBIG2 input limit",
            MAX_JBIG2_INPUT_BYTES
        ));
    }

    let image_mask = match stream.dict.get(b"ImageMask") {
        Ok(value) => {
            let (_, value) = document
                .dereference(value)
                .map_err(|error| format!("ImageMask reference is invalid ({error})"))?;
            value
                .as_bool()
                .map_err(|_| "ImageMask is not a boolean".to_string())?
        }
        Err(_) => false,
    };
    if let Ok(value) = stream.dict.get(b"BitsPerComponent") {
        let (_, value) = document
            .dereference(value)
            .map_err(|error| format!("BitsPerComponent reference is invalid ({error})"))?;
        if value.as_i64().ok().filter(|value| *value == 1).is_none() {
            return Err("JBIG2 image has a BitsPerComponent other than 1".to_string());
        }
    } else if !image_mask {
        return Err("JBIG2 image is missing BitsPerComponent".to_string());
    }

    let width = dictionary_dimension(document, &stream.dict, b"Width")?;
    let height = dictionary_dimension(document, &stream.dict, b"Height")?;
    let globals = decode_globals(document, &stream.dict, limit)?;
    Ok(Some(Jbig2Candidate {
        width,
        height,
        data: stream.content.clone(),
        globals,
    }))
}

fn filter_names(document: &Document, dictionary: &Dictionary) -> Result<Vec<Vec<u8>>, String> {
    let value = match dictionary.get(b"Filter") {
        Ok(value) => value,
        Err(_) => return Ok(Vec::new()),
    };
    let (_, value) = document
        .dereference(value)
        .map_err(|error| format!("Filter reference is invalid ({error})"))?;
    match value {
        Object::Name(name) => Ok(vec![name.clone()]),
        Object::Array(values) => values
            .iter()
            .map(|value| {
                let (_, value) = document
                    .dereference(value)
                    .map_err(|error| format!("Filter reference is invalid ({error})"))?;
                value
                    .as_name()
                    .map(|name| name.to_vec())
                    .map_err(|_| "Filter entry is not a name".to_string())
            })
            .collect(),
        _ => Err("Filter is not a name or array".to_string()),
    }
}

fn dictionary_dimension(
    document: &Document,
    dictionary: &Dictionary,
    key: &[u8],
) -> Result<u32, String> {
    let value = dictionary
        .get(key)
        .map_err(|_| format!("missing /{}", String::from_utf8_lossy(key)))?;
    let (_, value) = document.dereference(value).map_err(|error| {
        format!(
            "/{} reference is invalid ({error})",
            String::from_utf8_lossy(key)
        )
    })?;
    let value = value
        .as_i64()
        .map_err(|_| format!("/{} is not an integer", String::from_utf8_lossy(key)))?;
    u32::try_from(value)
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| {
            format!(
                "/{} is outside the supported range",
                String::from_utf8_lossy(key)
            )
        })
}

fn decode_globals(
    document: &Document,
    dictionary: &Dictionary,
    limit: usize,
) -> Result<Option<Vec<u8>>, String> {
    let params = match dictionary.get(b"DecodeParms") {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    let (_, params) = document
        .dereference(params)
        .map_err(|error| format!("DecodeParms reference is invalid ({error})"))?;
    let params = match params {
        Object::Array(values) => values
            .first()
            .ok_or_else(|| "DecodeParms array has no entry for JBIG2Decode".to_string())?,
        params => params,
    };
    let (_, params) = document
        .dereference(params)
        .map_err(|error| format!("DecodeParms entry reference is invalid ({error})"))?;
    let params = match params {
        Object::Null => return Ok(None),
        Object::Dictionary(dictionary) => dictionary,
        _ => return Err("DecodeParms entry is not a dictionary or null".to_string()),
    };
    let globals = match params.get(b"JBIG2Globals") {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    let (_, globals) = document
        .dereference(globals)
        .map_err(|error| format!("JBIG2Globals reference is invalid ({error})"))?;
    let stream = globals
        .as_stream()
        .map_err(|_| "JBIG2Globals is not a stream".to_string())?;
    decode_external_stream(document, stream, limit).map(Some)
}

fn decode_external_stream(
    document: &Document,
    stream: &lopdf::Stream,
    limit: usize,
) -> Result<Vec<u8>, String> {
    if stream.dict.get(b"DecodeParms").is_ok() {
        return Err("JBIG2Globals DecodeParms are preserved conservatively".to_string());
    }
    let filters = filter_names(document, &stream.dict)?;
    let mut data = stream.content.clone();
    if data.len() > limit {
        return Err("JBIG2Globals exceeds max_decoded_stream_bytes".to_string());
    }
    for filter in filters {
        match filter.as_slice() {
            b"FlateDecode" | b"Fl" => {
                let mut decoder = ZlibDecoder::new(data.as_slice());
                let mut output = Vec::new();
                decoder
                    .by_ref()
                    .take(limit.saturating_add(1) as u64)
                    .read_to_end(&mut output)
                    .map_err(|error| format!("JBIG2Globals FlateDecode failed ({error})"))?;
                if output.len() > limit {
                    return Err("JBIG2Globals exceeds max_decoded_stream_bytes".to_string());
                }
                data = output;
            }
            _ => return Err("JBIG2Globals uses an unsupported stream filter".to_string()),
        }
    }
    Ok(data)
}

fn pdf_samples(decoded: DecodedImage) -> Vec<u8> {
    let DecodedImage {
        width,
        stride,
        mut samples,
        ..
    } = decoded;
    if stride == 0 {
        return samples;
    }
    let used_bits = width % 8;
    for row in samples.chunks_exact_mut(stride) {
        for byte in row.iter_mut() {
            *byte = !*byte;
        }
        if used_bits != 0 {
            let mask = 0xFF_u8 << (8 - used_bits);
            if let Some(last) = row.last_mut() {
                *last &= mask;
            }
        }
    }
    samples
}

fn flate_encode(samples: &[u8], limit: usize) -> Result<Vec<u8>, String> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
    encoder
        .write_all(samples)
        .map_err(|error| format!("FlateEncode failed ({error})"))?;
    let encoded = encoder
        .finish()
        .map_err(|error| format!("FlateEncode failed ({error})"))?;
    if encoded.len() > MAX_JBIG2_INPUT_BYTES || encoded.len() > limit.saturating_add(limit / 8) {
        return Err("Flate JBIG2 replacement exceeds the stream resource limit".to_string());
    }
    Ok(encoded)
}

struct PackedDecoder {
    width: u32,
    height: u32,
    stride: usize,
    row: u32,
    column: u32,
    samples: Vec<u8>,
    invalid: bool,
}

impl PackedDecoder {
    fn new(width: u32, height: u32, stride: usize, output_len: usize) -> Self {
        Self {
            width,
            height,
            stride,
            row: 0,
            column: 0,
            samples: vec![0; output_len],
            invalid: false,
        }
    }

    fn push_one(&mut self, black: bool) {
        if self.invalid || self.row >= self.height || self.column >= self.width {
            self.invalid = true;
            return;
        }

        if black {
            let bit = 1_u8 << (7 - (self.column % 8));
            match self.sample_mut(self.column) {
                Some(byte) => *byte |= bit,
                None => {
                    self.invalid = true;
                    return;
                }
            }
        }
        self.column += 1;
    }

    fn sample_mut(&mut self, column: u32) -> Option<&mut u8> {
        let offset = (self.row as usize)
            .checked_mul(self.stride)?
            .checked_add(column as usize / 8)?;
        self.samples.get_mut(offset)
    }
}

impl Decoder for PackedDecoder {
    fn push_pixel(&mut self, black: bool) {
        self.push_one(black);
    }

    fn push_pixel_chunk(&mut self, black: bool, chunk_count: u32) {
        if self.invalid {
            return;
        }
        if self.row >= self.height {
            self.invalid = true;
            return;
        }

        // The hayro callback contract guarantees byte alignment.  Check the
        // advertised run before looping so a corrupt count cannot turn into a
        // long allocation-free CPU loop or advance beyond the row.
        let remaining = self.width.saturating_sub(self.column);
        let Some(pixel_count) = chunk_count.checked_mul(8) else {
            self.invalid = true;
            return;
        };
        if pixel_count > remaining || !self.column.is_multiple_of(8) {
            self.invalid = true;
            return;
        }

        if black {
            for _ in 0..chunk_count {
                match self.sample_mut(self.column) {
                    Some(byte) => *byte = 0xFF,
                    None => {
                        self.invalid = true;
                        return;
                    }
                }
                self.column += 8;
            }
        } else {
            self.column += pixel_count;
        }
    }

    fn next_line(&mut self) {
        if self.invalid || self.column != self.width || self.row >= self.height {
            self.invalid = true;
            return;
        }
        self.column = 0;
        self.row += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMIT: usize = 16 * 1024 * 1024;
    const REGION_TYPES: [u8; 6] = [36, 38, 39, 40, 42, 43];

    fn segment(number: u32, segment_type: u8, payload: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&number.to_be_bytes());
        bytes.push(segment_type);
        bytes.push(0);
        bytes.push(1);
        bytes.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        bytes.extend_from_slice(payload);
        bytes
    }

    fn page_info(width: u32, height: u32) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend_from_slice(&width.to_be_bytes());
        payload.extend_from_slice(&height.to_be_bytes());
        payload.extend_from_slice(&[0; 8]);
        payload.extend_from_slice(&[0; 3]);
        payload
    }

    fn region_info(width: u32, height: u32, x: u32, y: u32) -> Vec<u8> {
        let mut payload = Vec::new();
        for value in [width, height, x, y] {
            payload.extend_from_slice(&value.to_be_bytes());
        }
        payload.push(0);
        payload
    }

    fn generic_region(width: u32, height: u32, x: u32, y: u32) -> Vec<u8> {
        let mut payload = region_info(width, height, x, y);
        payload.push(0);
        payload.extend_from_slice(&[3, 0xFF, 0xFD, 0xFF, 2, 0xFE, 0xFE, 0xFE]);
        payload.extend_from_slice(&[0x12, 0x34, 0x56, 0x78]);
        payload
    }

    fn valid_stream() -> Vec<u8> {
        let mut stream = segment(0, 48, &page_info(8, 8));
        stream.extend(segment(1, 38, &generic_region(8, 8, 0, 0)));
        stream
    }

    #[test]
    fn well_formed_stream_decodes() {
        let image =
            decode_embedded(&valid_stream(), None, 8, 8, LIMIT).expect("valid stream decodes");
        assert_eq!(image.samples().len(), 8);
    }

    #[test]
    fn truncated_streams_are_rejected() {
        let stream = valid_stream();
        for length in 0..stream.len() {
            let prefix = &stream[..length];
            assert!(
                decode_embedded(prefix, None, 8, 8, LIMIT).is_err(),
                "prefix of {length} bytes was accepted"
            );
            assert!(
                preflight_embedded(prefix, None, 8, 8, 8, LIMIT).is_err(),
                "prefix of {length} bytes passed preflight"
            );
        }
    }

    #[test]
    fn truncated_globals_are_rejected() {
        let globals = segment(0, 36, &generic_region(8, 8, 0, 0));
        let data = segment(2, 48, &page_info(8, 8));
        for length in 0..globals.len() {
            let result = decode_embedded(&data, Some(&globals[..length]), 8, 8, LIMIT);
            assert!(result.is_err(), "globals prefix of {length} bytes accepted");
        }
    }

    #[test]
    fn garbage_streams_are_rejected() {
        let mut state = 0x9E37_79B9_u32;
        let mut next = move || {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 24) as u8
        };
        for length in 0..96 {
            let garbage: Vec<u8> = (0..length).map(|_| next()).collect();
            assert!(decode_embedded(&garbage, None, 8, 8, LIMIT).is_err());
            if !garbage.is_empty() {
                assert!(decode_embedded(&valid_stream(), Some(&garbage), 8, 8, LIMIT).is_err());
            }
        }
        for fill in [0x00_u8, 0xFF] {
            let garbage = vec![fill; 64];
            assert!(decode_embedded(&garbage, None, 8, 8, LIMIT).is_err());
        }
    }

    #[test]
    fn tiny_region_payloads_are_rejected() {
        for segment_type in REGION_TYPES {
            for length in 0..17 {
                let payload = vec![0_u8; length];
                let mut stream = segment(0, 48, &page_info(8, 8));
                stream.extend(segment(1, segment_type, &payload));
                assert_eq!(
                    preflight_embedded(&stream, None, 8, 8, 8, LIMIT).err(),
                    Some(Jbig2Error::MalformedStructure),
                    "type {segment_type}, payload length {length}"
                );
                assert!(decode_embedded(&stream, None, 8, 8, LIMIT).is_err());
            }
        }
    }

    #[test]
    fn tiny_page_information_payloads_are_rejected() {
        let full = page_info(8, 8);
        for length in 0..full.len() {
            let mut stream = segment(0, 48, &full[..length]);
            stream.extend(segment(1, 38, &generic_region(8, 8, 0, 0)));
            assert_eq!(
                preflight_embedded(&stream, None, 8, 8, 8, LIMIT).err(),
                Some(Jbig2Error::MalformedStructure),
                "payload length {length}"
            );
        }
    }

    #[test]
    fn region_payloads_too_short_for_the_decoder_do_not_panic() {
        for length in 17..generic_region(8, 8, 0, 0).len() {
            let payload = &generic_region(8, 8, 0, 0)[..length];
            let mut stream = segment(0, 48, &page_info(8, 8));
            stream.extend(segment(1, 38, payload));
            let _ = decode_embedded(&stream, None, 8, 8, LIMIT);
        }
    }

    #[test]
    fn region_offsets_outside_any_page_are_rejected() {
        for (x, y) in [
            (u32::from(u16::MAX) + 1, 0),
            (0, u32::from(u16::MAX) + 1),
            (i32::MAX as u32, 0),
            (0, i32::MAX as u32),
            (u32::MAX, u32::MAX),
        ] {
            let mut stream = segment(0, 48, &page_info(8, 8));
            stream.extend(segment(1, 38, &generic_region(8, 8, x, y)));
            assert_eq!(
                preflight_embedded(&stream, None, 8, 8, 8, LIMIT).err(),
                Some(Jbig2Error::UnsupportedStructure),
                "origin ({x}, {y})"
            );
        }
    }

    #[test]
    fn oversized_dimensions_are_rejected() {
        for (width, height) in [
            (u32::MAX, 8),
            (8, u32::MAX),
            (u32::from(u16::MAX) + 1, 8),
            (8, u32::from(u16::MAX) + 1),
        ] {
            let mut stream = segment(0, 48, &page_info(width, height));
            stream.extend(segment(1, 38, &generic_region(8, 8, 0, 0)));
            assert!(preflight_embedded(&stream, None, 8, 8, 8, LIMIT).is_err());

            let mut stream = segment(0, 48, &page_info(8, 8));
            stream.extend(segment(1, 38, &generic_region(width, height, 0, 0)));
            assert!(preflight_embedded(&stream, None, 8, 8, 8, LIMIT).is_err());
        }
    }

    #[test]
    fn malformed_segment_headers_are_rejected() {
        // Declared payload longer than the remaining data.
        let mut stream = segment(0, 48, &page_info(8, 8));
        let mut oversized = segment(1, 38, &generic_region(8, 8, 0, 0));
        let length_offset = 4 + 1 + 1 + 1;
        oversized
            .get_mut(length_offset..length_offset + 4)
            .expect("length field")
            .copy_from_slice(&1_000_u32.to_be_bytes());
        stream.extend(oversized);
        assert_eq!(
            preflight_embedded(&stream, None, 8, 8, 8, LIMIT).err(),
            Some(Jbig2Error::MalformedStructure)
        );

        // Unknown-length segments need a marker scan and are not supported.
        let mut stream = segment(0, 48, &page_info(8, 8));
        let mut unknown = segment(1, 38, &generic_region(8, 8, 0, 0));
        unknown
            .get_mut(length_offset..length_offset + 4)
            .expect("length field")
            .copy_from_slice(&u32::MAX.to_be_bytes());
        stream.extend(unknown);
        assert_eq!(
            preflight_embedded(&stream, None, 8, 8, 8, LIMIT).err(),
            Some(Jbig2Error::UnsupportedStructure)
        );

        // Long-form referred-segment count that promises far more data than exists.
        let mut stream = segment(0, 48, &page_info(8, 8));
        stream.extend_from_slice(&[0, 0, 0, 1, 38, 0xE0, 0x00, 0x0F, 0xFF]);
        assert!(preflight_embedded(&stream, None, 8, 8, 8, LIMIT).is_err());
        let mut stream = segment(0, 48, &page_info(8, 8));
        stream.extend_from_slice(&[0, 0, 0, 1, 38, 0xFF, 0xFF, 0xFF, 0xFF]);
        assert!(preflight_embedded(&stream, None, 8, 8, 8, LIMIT).is_err());
        let mut stream = segment(0, 48, &page_info(8, 8));
        stream.extend_from_slice(&[0, 0, 0, 1, 38, 0xE0, 0x00]);
        assert_eq!(
            preflight_embedded(&stream, None, 8, 8, 8, LIMIT).err(),
            Some(Jbig2Error::MalformedStructure)
        );

        // Reserved referred-count encodings and forward references.
        let mut stream = segment(0, 48, &page_info(8, 8));
        stream.extend_from_slice(&[0, 0, 0, 1, 38, 0xA0, 1, 0, 0, 0, 0]);
        assert_eq!(
            preflight_embedded(&stream, None, 8, 8, 8, LIMIT).err(),
            Some(Jbig2Error::MalformedStructure)
        );
        let mut stream = segment(0, 48, &page_info(8, 8));
        stream.extend_from_slice(&[0, 0, 0, 1, 38, 0x20, 5, 1, 0, 0, 0, 0]);
        assert_eq!(
            preflight_embedded(&stream, None, 8, 8, 8, LIMIT).err(),
            Some(Jbig2Error::MalformedStructure)
        );
    }

    #[test]
    fn segment_numbers_above_the_one_byte_range_parse_references() {
        let mut stream = segment(0, 48, &page_info(8, 8));
        stream.extend(segment(1, 36, &generic_region(8, 8, 0, 0)));
        let mut refinement = vec![0, 0, 1, 0, 43, 0x20, 0, 1, 1];
        let payload = {
            let mut payload = region_info(8, 8, 0, 0);
            payload.push(0x01);
            payload.extend_from_slice(&[0x12, 0x34]);
            payload
        };
        refinement.push(0);
        refinement.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        refinement.extend_from_slice(&payload);
        stream.extend(refinement);
        let _ = decode_embedded(&stream, None, 8, 8, LIMIT);

        // Truncated two-byte reference.
        let mut stream = segment(0, 48, &page_info(8, 8));
        stream.extend_from_slice(&[0, 0, 1, 0, 43, 0x20, 0]);
        assert_eq!(
            preflight_embedded(&stream, None, 8, 8, 8, LIMIT).err(),
            Some(Jbig2Error::MalformedStructure)
        );
    }

    #[test]
    fn duplicate_and_missing_pages_are_rejected() {
        let mut stream = segment(0, 48, &page_info(8, 8));
        stream.extend(segment(1, 48, &page_info(8, 8)));
        stream.extend(segment(2, 38, &generic_region(8, 8, 0, 0)));
        assert!(preflight_embedded(&stream, None, 8, 8, 8, LIMIT).is_err());

        let stream = segment(0, 38, &generic_region(8, 8, 0, 0));
        assert!(preflight_embedded(&stream, None, 8, 8, 8, LIMIT).is_err());
    }

    fn image(width: u32, stride: usize, samples: Vec<u8>) -> DecodedImage {
        DecodedImage {
            width,
            height: 1,
            stride,
            samples,
        }
    }

    #[test]
    fn pdf_samples_handles_zero_stride() {
        assert!(pdf_samples(image(0, 0, Vec::new())).is_empty());
        assert_eq!(pdf_samples(image(8, 0, vec![1, 2, 3])), vec![1, 2, 3]);
        assert_eq!(pdf_samples(image(3, 0, vec![0xAA])), vec![0xAA]);
    }

    #[test]
    fn pdf_samples_inverts_and_clears_padding() {
        assert_eq!(pdf_samples(image(8, 1, vec![0x00, 0xFF])), vec![0xFF, 0x00]);
        assert_eq!(
            pdf_samples(image(10, 2, vec![0x00, 0x00, 0xFF, 0xFF])),
            vec![0xFF, 0xC0, 0x00, 0x00]
        );
        assert_eq!(pdf_samples(image(1, 1, vec![0x00])), vec![0x80]);
        // A trailing partial row is left untouched rather than indexed.
        assert_eq!(
            pdf_samples(image(8, 2, vec![0x00, 0x00, 0x0F])),
            vec![0xFF, 0xFF, 0x0F]
        );
        assert!(pdf_samples(image(8, 1, Vec::new())).is_empty());
    }

    #[test]
    fn packed_decoder_flags_out_of_range_output() {
        // More pixels than the row holds.
        let mut decoder = PackedDecoder::new(8, 1, 1, 1);
        for _ in 0..9 {
            decoder.push_pixel(true);
        }
        assert!(decoder.invalid);

        // More rows than the image holds.
        let mut decoder = PackedDecoder::new(8, 1, 1, 1);
        decoder.push_pixel_chunk(true, 1);
        decoder.next_line();
        decoder.push_pixel_chunk(true, 1);
        assert!(decoder.invalid);

        // Chunk run longer than the row, and one that overflows the multiplication.
        let mut decoder = PackedDecoder::new(8, 1, 1, 1);
        decoder.push_pixel_chunk(true, 2);
        assert!(decoder.invalid);
        let mut decoder = PackedDecoder::new(8, 1, 1, 1);
        decoder.push_pixel_chunk(false, u32::MAX);
        assert!(decoder.invalid);

        // Output buffer smaller than the advertised geometry.
        let mut decoder = PackedDecoder::new(16, 2, 2, 1);
        decoder.push_pixel_chunk(true, 1);
        decoder.push_pixel_chunk(true, 1);
        assert!(decoder.invalid);
        let mut decoder = PackedDecoder::new(16, 2, 2, 0);
        decoder.push_pixel(true);
        assert!(decoder.invalid);

        // A white pixel never touches the buffer, so an empty buffer is fine for it.
        let mut decoder = PackedDecoder::new(8, 1, 1, 1);
        decoder.push_pixel_chunk(false, 1);
        decoder.next_line();
        assert!(!decoder.invalid);
        assert_eq!(decoder.row, 1);
    }

    #[test]
    fn packed_decoder_rejects_unaligned_chunks_and_early_lines() {
        let mut decoder = PackedDecoder::new(16, 1, 2, 2);
        decoder.push_pixel(false);
        decoder.push_pixel_chunk(true, 1);
        assert!(decoder.invalid);

        let mut decoder = PackedDecoder::new(16, 1, 2, 2);
        decoder.push_pixel(false);
        decoder.next_line();
        assert!(decoder.invalid);
    }
}
