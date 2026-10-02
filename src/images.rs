//! Conservative image optimisation for PDF XObjects.
//!
//! Images are deliberately handled separately from the generic stream pass.  A
//! PDF image stream is not, in general, an image file: Flate streams contain
//! samples described by the image dictionary, and a colour profile, Decode
//! array, mask, or predictor can change the meaning of every byte.  The code in
//! this module only rewrites the small, well-defined subset for which it can
//! establish the pixels and the required placement resolution.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};

use flate2::read::ZlibDecoder;
use image::codecs::jpeg::JpegEncoder;
use image::imageops::{self, FilterType};
use image::ExtendedColorType;
use lopdf::content::Content;
use lopdf::{Dictionary, Document, Object, ObjectId, Stream};

use crate::{Error, Options, Report};

const EPSILON: f64 = 1.0e-12;

/// Optimise eligible image XObjects in `document`.
///
/// Lossless operation may replace simple raw samples with a PNG-predictor
/// Flate stream, but the decoded samples remain byte-for-byte identical.
/// Explicit lossy presets can convert unmasked DeviceRGB/DeviceGray 8-bit
/// images to JPEG after resolving every page/form placement that uses the
/// object.
pub fn optimize_images(
    document: &mut Document,
    options: &Options,
    report: &mut Report,
) -> Result<(), Error> {
    let mut placements = HashMap::<ObjectId, ImagePlacement>::new();
    let mut warnings = Vec::new();
    let mut optimized_ids = HashSet::new();
    let decode_limit = options.max_decoded_stream_bytes;

    if decode_limit == 0 {
        warnings.push("image optimisation skipped: max_decoded_stream_bytes is zero".to_string());
    } else {
        // Predictor 15 is a byte-for-byte lossless representation of ordinary
        // 8-bit RGB/gray samples. It does not need placement analysis, so it
        // is safe and useful even for the default lossless preset.
        let image_ids: Vec<ObjectId> = document
            .objects
            .iter()
            .filter_map(|(id, object)| {
                let stream = object.as_stream().ok()?;
                (stream.dict.get(b"Subtype").and_then(Object::as_name).ok() == Some(b"Image"))
                    .then_some(*id)
            })
            .collect();
        for image_id in image_ids {
            match optimize_lossless_image(document, image_id, decode_limit) {
                Ok(true) => {
                    optimized_ids.insert(image_id);
                }
                Ok(false) => {}
                Err(reason) => warnings.push(format!(
                    "image object {} {} was preserved: {}",
                    image_id.0, image_id.1, reason
                )),
            }
        }

        if !options.allow_lossy || options.target_dpi == Some(0) {
            report.images_optimized = report.images_optimized.saturating_add(optimized_ids.len());
            report.warnings.extend(warnings);
            return Ok(());
        }

        let mut placement_analysis_ok = true;
        let mut auxiliary = AuxiliaryUses::default();
        let pages = document.get_pages();
        for (_, page_id) in pages {
            let (resources, resources_ok) = page_resources(document, page_id);
            if !resources_ok {
                placement_analysis_ok = false;
                warnings.push(format!(
                    "image placements on page object {} {} could not resolve page resources; lossy image transforms were preserved",
                    page_id.0, page_id.1
                ));
            }
            if !guard_auxiliary_image_uses(
                document,
                page_id,
                &resources,
                &mut placements,
                &mut auxiliary,
                decode_limit,
            ) {
                placement_analysis_ok = false;
                warnings.push(format!(
                    "non-content image uses on page object {} {} could not be analysed; lossy image transforms were preserved",
                    page_id.0, page_id.1
                ));
            }

            let user_unit = match page_user_unit(document, page_id) {
                Ok(value) => value,
                Err(reason) => {
                    placement_analysis_ok = false;
                    warnings.push(format!(
                        "image placements on page object {} {} have an unusable /UserUnit ({reason}); lossy image transforms were preserved",
                        page_id.0, page_id.1
                    ));
                    1.0
                }
            };

            let mut page_content = Vec::new();
            let mut page_content_ok = true;
            for stream_id in document.get_page_contents(page_id) {
                let stream = match document.get_object(stream_id).and_then(Object::as_stream) {
                    Ok(stream) => stream,
                    Err(_) => {
                        page_content_ok = false;
                        break;
                    }
                };
                match decode_stream_bounded(document, stream, decode_limit) {
                    Ok(decoded) => {
                        if page_content
                            .len()
                            .saturating_add(decoded.len())
                            .saturating_add(1)
                            > decode_limit
                        {
                            page_content_ok = false;
                            break;
                        }
                        page_content.extend_from_slice(&decoded);
                        page_content.push(b'\n');
                    }
                    Err(_) => {
                        page_content_ok = false;
                        break;
                    }
                }
            }
            if !page_content_ok {
                if !document.get_page_contents(page_id).is_empty() {
                    placement_analysis_ok = false;
                    warnings.push(format!(
                        "image placements on page object {} {} could not be decoded; images on that page were preserved",
                        page_id.0, page_id.1
                    ));
                }
                continue;
            }
            if page_content.is_empty() {
                continue;
            }

            let mut active_forms = HashSet::new();
            let content_ok = walk_content(
                document,
                &page_content,
                &resources,
                Matrix::scale(user_unit),
                &mut placements,
                &mut auxiliary,
                &mut active_forms,
                &mut warnings,
                decode_limit,
            );
            placement_analysis_ok &= content_ok;
        }

        if !placement_analysis_ok {
            warnings.push(
                "lossy image optimisation skipped because at least one image placement could not be analysed safely"
                    .to_string(),
            );
            placements.clear();
        }
    }

    let target_dpi = options.target_dpi;
    let mut ids: Vec<ObjectId> = placements.keys().copied().collect();
    ids.sort_unstable();
    let mut warned_ids = HashSet::new();
    for image_id in ids {
        let placement = placements.get(&image_id).copied().unwrap_or_default();
        if placement.ambiguous || placement.count == 0 {
            push_image_warning(
                &mut warnings,
                &mut warned_ids,
                image_id,
                "the effective placement is ambiguous",
            );
            continue;
        }

        let prepared = match prepare_image(
            document,
            image_id,
            placement,
            target_dpi,
            options,
            decode_limit,
        ) {
            Ok(Some(prepared)) => prepared,
            Ok(None) => continue,
            Err(reason) => {
                push_image_warning(&mut warnings, &mut warned_ids, image_id, &reason);
                continue;
            }
        };

        let should_replace = match document.get_object(image_id).and_then(Object::as_stream) {
            Ok(stream) => prepared.encoded.len() < lossless_baseline_len(document, stream),
            Err(_) => false,
        };
        if !should_replace {
            push_image_warning(
                &mut warnings,
                &mut warned_ids,
                image_id,
                "JPEG output was not smaller than the existing lossless/JPEG representation",
            );
            continue;
        }

        let stream = match document
            .get_object_mut(image_id)
            .and_then(Object::as_stream_mut)
        {
            Ok(stream) => stream,
            Err(_) => {
                push_image_warning(
                    &mut warnings,
                    &mut warned_ids,
                    image_id,
                    "image object disappeared before it could be updated",
                );
                continue;
            }
        };
        stream.dict.set("Width", prepared.width as i64);
        stream.dict.set("Height", prepared.height as i64);
        stream
            .dict
            .set("ColorSpace", Object::Name(prepared.color_space.to_vec()));
        stream.dict.set("BitsPerComponent", 8_i64);
        stream
            .dict
            .set("Filter", Object::Name(b"DCTDecode".to_vec()));
        stream.dict.remove(b"DecodeParms");
        stream.set_content(prepared.encoded);
        optimized_ids.insert(image_id);
    }

    report.images_optimized = report.images_optimized.saturating_add(optimized_ids.len());
    report.warnings.extend(warnings);
    Ok(())
}

#[derive(Clone, Copy, Debug, Default)]
struct ImagePlacement {
    /// Maximum displayed width and height, in PDF points, across all uses.
    max_width_points: f64,
    max_height_points: f64,
    count: usize,
    ambiguous: bool,
}

impl ImagePlacement {
    fn record(&mut self, matrix: Matrix) {
        let width = matrix.0[0].hypot(matrix.0[1]);
        let height = matrix.0[2].hypot(matrix.0[3]);
        if !width.is_finite() || !height.is_finite() || width <= EPSILON || height <= EPSILON {
            self.ambiguous = true;
            return;
        }
        self.max_width_points = self.max_width_points.max(width);
        self.max_height_points = self.max_height_points.max(height);
        self.count = self.count.saturating_add(1);
    }
}

#[derive(Clone, Copy, Debug)]
struct Matrix([f64; 6]);

impl Matrix {
    const IDENTITY: Self = Self([1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);

    fn scale(value: f64) -> Self {
        Self([value, 0.0, 0.0, value, 0.0, 0.0])
    }

    /// Concatenate the current matrix with a PDF `cm` matrix.  Translation is
    /// retained even though image resolution only uses the linear part.
    fn concat(self, rhs: Self) -> Self {
        Self([
            self.0[0] * rhs.0[0] + self.0[2] * rhs.0[1],
            self.0[1] * rhs.0[0] + self.0[3] * rhs.0[1],
            self.0[0] * rhs.0[2] + self.0[2] * rhs.0[3],
            self.0[1] * rhs.0[2] + self.0[3] * rhs.0[3],
            self.0[0] * rhs.0[4] + self.0[2] * rhs.0[5] + self.0[4],
            self.0[1] * rhs.0[4] + self.0[3] * rhs.0[5] + self.0[5],
        ])
    }

    fn from_operands(operands: &[Object]) -> Option<Self> {
        if operands.len() != 6 {
            return None;
        }
        let mut values = [0.0; 6];
        for (slot, operand) in values.iter_mut().zip(operands) {
            *slot = number(operand)?;
        }
        if values.iter().all(|value| value.is_finite()) {
            Some(Self(values))
        } else {
            None
        }
    }
}

fn number(object: &Object) -> Option<f64> {
    match object {
        Object::Integer(value) => Some(*value as f64),
        Object::Real(value) => Some(*value),
        _ => None,
    }
}

fn push_image_warning(
    warnings: &mut Vec<String>,
    warned: &mut HashSet<ObjectId>,
    id: ObjectId,
    reason: &str,
) {
    if warned.insert(id) {
        warnings.push(format!(
            "image object {} {} was preserved: {}",
            id.0, id.1, reason
        ));
    }
}

fn page_resources(document: &Document, page_id: ObjectId) -> (Vec<&Dictionary>, bool) {
    let mut resources = Vec::new();
    let mut current = Some(page_id);
    let mut seen = HashSet::new();
    let mut ok = true;
    while let Some(id) = current {
        if !seen.insert(id) {
            ok = false;
            break;
        }
        let page = match document.get_dictionary(id) {
            Ok(page) => page,
            Err(_) => {
                ok = false;
                break;
            }
        };
        if let Ok(value) = page.get(b"Resources") {
            if let Some(dictionary) = resolved_dictionary(document, value) {
                resources.push(dictionary);
            } else {
                ok = false;
            }
        }
        current = page
            .get(b"Parent")
            .ok()
            .and_then(|value| value.as_reference().ok());
    }
    (resources, ok)
}

fn page_user_unit(document: &Document, page_id: ObjectId) -> Result<f64, String> {
    let mut current = Some(page_id);
    let mut seen = HashSet::new();
    while let Some(id) = current {
        if !seen.insert(id) {
            return Err("page tree cycle".to_string());
        }
        let page = document
            .get_dictionary(id)
            .map_err(|_| "page dictionary could not be resolved".to_string())?;
        if let Ok(value) = page.get(b"UserUnit") {
            let value = document
                .dereference(value)
                .map_err(|_| "UserUnit reference could not be resolved".to_string())?
                .1;
            let value = number(value).ok_or_else(|| "UserUnit is not numeric".to_string())?;
            if !value.is_finite() || value <= 0.0 {
                return Err("UserUnit is not positive and finite".to_string());
            }
            return Ok(value);
        }
        current = page
            .get(b"Parent")
            .ok()
            .and_then(|value| value.as_reference().ok());
    }
    Ok(1.0)
}

fn form_resources<'a>(
    document: &'a Document,
    form: &'a Dictionary,
    inherited: &[&'a Dictionary],
) -> (Vec<&'a Dictionary>, bool) {
    let mut resources = Vec::new();
    let mut ok = true;
    if let Ok(value) = form.get(b"Resources") {
        if let Some(dictionary) = resolved_dictionary(document, value) {
            resources.push(dictionary);
        } else {
            ok = false;
        }
    }
    resources.extend_from_slice(inherited);
    (resources, ok)
}

fn resolved_dictionary<'a>(document: &'a Document, value: &'a Object) -> Option<&'a Dictionary> {
    document.dereference(value).ok()?.1.as_dict().ok()
}

fn lookup_xobject<'a>(
    document: &'a Document,
    resources: &[&'a Dictionary],
    name: &[u8],
) -> Option<(Option<ObjectId>, &'a Object)> {
    for resource in resources {
        let xobjects = match resource
            .get(b"XObject")
            .ok()
            .and_then(|value| resolved_dictionary(document, value))
        {
            Some(xobjects) => xobjects,
            None => continue,
        };
        let value = match xobjects.get(name) {
            Ok(value) => value,
            Err(_) => continue,
        };
        if let Ok(resolved) = document.dereference(value) {
            return Some(resolved);
        }
    }
    None
}

const MAX_AUXILIARY_DEPTH: usize = 64;

/// Image uses reachable from resource consumers that do not appear as a page
/// `Do` operation: Type3 glyph programs, tiling patterns, ExtGState soft-mask
/// groups and annotation appearances.  Each can draw an image at a placement
/// which cannot be inferred from the page content alone, so their image ids are
/// marked ambiguous and a shared image is retained at its original resolution.
///
/// Only paths that can draw are followed and only genuine content streams are
/// decoded; font programs, CMaps, ICC profiles and function streams are never
/// touched.  One instance spans a whole `optimize_images` call and each
/// `(object, inherited resources)` pair is parsed at most once.  A repeated
/// visit reports success because the first visit already reported its outcome,
/// and any failure makes the whole call's placement analysis fail.
#[derive(Default)]
struct AuxiliaryUses {
    visited: HashSet<(ObjectId, Vec<usize>)>,
    image_ids: HashSet<ObjectId>,
}

impl AuxiliaryUses {
    fn apply(&mut self, placements: &mut HashMap<ObjectId, ImagePlacement>) {
        for id in self.image_ids.drain() {
            placements.entry(id).or_default().ambiguous = true;
        }
    }

    /// Returns true the first time `id` is seen with this resource chain.
    /// Objects without an identity cannot be memoised.
    fn first_visit(&mut self, id: Option<ObjectId>, chain: &[&Dictionary]) -> bool {
        match id {
            Some(id) => {
                let key = chain
                    .iter()
                    .map(|dictionary| std::ptr::from_ref::<Dictionary>(dictionary) as usize)
                    .collect();
                self.visited.insert((id, key))
            }
            None => true,
        }
    }

    /// Scan the auxiliary categories of the resource dictionaries in `own`.
    /// `chain` is `own` plus the inherited dictionaries, used to resolve `Do`
    /// names in the content streams found on the way.
    fn scan_resources<'a>(
        &mut self,
        document: &'a Document,
        own: &[&'a Dictionary],
        chain: &[&'a Dictionary],
        decode_limit: usize,
        depth: usize,
    ) -> bool {
        for resource in own {
            if let Some(patterns) = resource_category(document, resource, b"Pattern") {
                for (_, value) in patterns.iter() {
                    if !self.scan_pattern(document, value, chain, decode_limit, depth) {
                        return false;
                    }
                }
            }
            if let Some(states) = resource_category(document, resource, b"ExtGState") {
                for (_, value) in states.iter() {
                    if !self.scan_ext_gstate(document, value, chain, decode_limit, depth) {
                        return false;
                    }
                }
            }
            if let Some(fonts) = resource_category(document, resource, b"Font") {
                for (_, value) in fonts.iter() {
                    if !self.scan_font(document, value, chain, decode_limit, depth) {
                        return false;
                    }
                }
            }
        }
        true
    }

    /// Tiling patterns are content streams; shading patterns are dictionaries
    /// that cannot draw an image.
    fn scan_pattern<'a>(
        &mut self,
        document: &'a Document,
        value: &'a Object,
        chain: &[&'a Dictionary],
        decode_limit: usize,
        depth: usize,
    ) -> bool {
        let Ok((id, object)) = document.dereference(value) else {
            return false;
        };
        match object {
            Object::Stream(stream)
                if stream
                    .dict
                    .get(b"PatternType")
                    .ok()
                    .and_then(|value| value.as_i64().ok())
                    != Some(2) =>
            {
                self.scan_form(document, id, stream, chain, decode_limit, depth + 1)
            }
            _ => true,
        }
    }

    /// Only a soft mask's transparency group can draw; every other ExtGState
    /// entry is a number, name or function.
    fn scan_ext_gstate<'a>(
        &mut self,
        document: &'a Document,
        value: &'a Object,
        chain: &[&'a Dictionary],
        decode_limit: usize,
        depth: usize,
    ) -> bool {
        let Ok((_, state)) = document.dereference(value) else {
            return false;
        };
        let Ok(state) = state.as_dict() else {
            return true;
        };
        let Ok(mask) = state.get(b"SMask") else {
            return true;
        };
        let Ok((_, mask)) = document.dereference(mask) else {
            return false;
        };
        let Ok(mask) = mask.as_dict() else {
            return true;
        };
        let Ok(group) = mask.get(b"G") else {
            return true;
        };
        let Ok((id, group)) = document.dereference(group) else {
            return false;
        };
        match group {
            Object::Stream(stream) => {
                self.scan_form(document, id, stream, chain, decode_limit, depth + 1)
            }
            _ => true,
        }
    }

    /// Only Type3 fonts carry content streams; the glyph programs of all other
    /// fonts are font files, not PDF content.
    fn scan_font<'a>(
        &mut self,
        document: &'a Document,
        value: &'a Object,
        chain: &[&'a Dictionary],
        decode_limit: usize,
        depth: usize,
    ) -> bool {
        let Ok((id, font)) = document.dereference(value) else {
            return false;
        };
        let Ok(font) = font.as_dict() else {
            return true;
        };
        if font
            .get(b"Subtype")
            .ok()
            .and_then(|value| value.as_name().ok())
            != Some(b"Type3")
        {
            return true;
        }
        if depth > MAX_AUXILIARY_DEPTH {
            return false;
        }
        if !self.first_visit(id, chain) {
            return true;
        }
        let (resources, resources_ok) = form_resources(document, font, chain);
        if !resources_ok {
            return false;
        }
        let own_len = resources.len().saturating_sub(chain.len());
        let own = resources.get(..own_len).unwrap_or_default();
        if !self.scan_resources(document, own, &resources, decode_limit, depth + 1) {
            return false;
        }
        let Ok(procs) = font.get(b"CharProcs") else {
            return true;
        };
        let Some(procs) = resolved_dictionary(document, procs) else {
            return false;
        };
        for (_, value) in procs.iter() {
            let Ok((_, object)) = document.dereference(value) else {
                return false;
            };
            if let Object::Stream(stream) = object {
                if !self.scan_content(document, stream, &resources, decode_limit, depth + 1) {
                    return false;
                }
            }
        }
        true
    }

    /// A stream drawn like a form XObject: a tiling pattern, soft-mask group,
    /// appearance stream or an actual form.  `inherited` is the fallback used
    /// when the stream lacks its own /Resources.
    fn scan_form<'a>(
        &mut self,
        document: &'a Document,
        id: Option<ObjectId>,
        stream: &'a Stream,
        inherited: &[&'a Dictionary],
        decode_limit: usize,
        depth: usize,
    ) -> bool {
        if stream
            .dict
            .get(b"Subtype")
            .ok()
            .and_then(|value| value.as_name().ok())
            == Some(b"Image")
        {
            return match id {
                Some(id) => {
                    self.image_ids.insert(id);
                    true
                }
                None => false,
            };
        }
        if depth > MAX_AUXILIARY_DEPTH {
            return false;
        }
        if !self.first_visit(id, inherited) {
            return true;
        }
        let (resources, resources_ok) = form_resources(document, &stream.dict, inherited);
        if !resources_ok {
            return false;
        }
        let own_len = resources.len().saturating_sub(inherited.len());
        let own = resources.get(..own_len).unwrap_or_default();
        if !self.scan_resources(document, own, &resources, decode_limit, depth + 1) {
            return false;
        }
        self.scan_content(document, stream, &resources, decode_limit, depth + 1)
    }

    /// Decode and parse `stream` as content, marking every image it draws.
    fn scan_content<'a>(
        &mut self,
        document: &'a Document,
        stream: &Stream,
        resources: &[&'a Dictionary],
        decode_limit: usize,
        depth: usize,
    ) -> bool {
        if depth > MAX_AUXILIARY_DEPTH {
            return false;
        }
        let Ok(decoded) = decode_stream_bounded(document, stream, decode_limit) else {
            return false;
        };
        let Ok(content) = Content::decode(&decoded) else {
            return false;
        };
        for operation in &content.operations {
            if operation.operator != "Do" {
                continue;
            }
            let Some(name) = operation
                .operands
                .first()
                .and_then(|value| value.as_name().ok())
            else {
                return false;
            };
            let Some((id, object)) = lookup_xobject(document, resources, name) else {
                return false;
            };
            let Object::Stream(xobject) = object else {
                return false;
            };
            let subtype = xobject
                .dict
                .get(b"Subtype")
                .ok()
                .and_then(|value| value.as_name().ok());
            if (subtype == Some(b"Image") || subtype == Some(b"Form"))
                && !self.scan_form(document, id, xobject, resources, decode_limit, depth + 1)
            {
                return false;
            }
        }
        true
    }

    /// Annotation appearance streams hang off the page rather than its
    /// resource dictionary, and each uses only its own /Resources.
    fn scan_annotations(
        &mut self,
        document: &Document,
        value: &Object,
        decode_limit: usize,
    ) -> bool {
        let Ok((_, Object::Array(annotations))) = document.dereference(value) else {
            return false;
        };
        for annotation in annotations {
            let Ok((_, annotation)) = document.dereference(annotation) else {
                return false;
            };
            let Ok(annotation) = annotation.as_dict() else {
                return false;
            };
            let Ok(appearance) = annotation.get(b"AP") else {
                continue;
            };
            let Some(appearance) = resolved_dictionary(document, appearance) else {
                return false;
            };
            for key in [b"N".as_slice(), b"R", b"D"] {
                if let Ok(entry) = appearance.get(key) {
                    if !self.scan_appearance(document, entry, decode_limit) {
                        return false;
                    }
                }
            }
        }
        true
    }

    /// An appearance entry is a stream or a dictionary of per-state streams.
    fn scan_appearance(
        &mut self,
        document: &Document,
        value: &Object,
        decode_limit: usize,
    ) -> bool {
        let Ok((id, entry)) = document.dereference(value) else {
            return false;
        };
        match entry {
            Object::Stream(stream) => self.scan_form(document, id, stream, &[], decode_limit, 0),
            Object::Dictionary(states) => {
                for (_, state) in states.iter() {
                    let Ok((id, state)) = document.dereference(state) else {
                        return false;
                    };
                    if let Object::Stream(stream) = state {
                        if !self.scan_form(document, id, stream, &[], decode_limit, 0) {
                            return false;
                        }
                    }
                }
                true
            }
            _ => true,
        }
    }
}

fn resource_category<'a>(
    document: &'a Document,
    resource: &'a Dictionary,
    key: &[u8],
) -> Option<&'a Dictionary> {
    resource
        .get(key)
        .ok()
        .and_then(|value| resolved_dictionary(document, value))
}

fn guard_auxiliary_image_uses<'a>(
    document: &'a Document,
    page_id: ObjectId,
    resources: &[&'a Dictionary],
    placements: &mut HashMap<ObjectId, ImagePlacement>,
    scan: &mut AuxiliaryUses,
    decode_limit: usize,
) -> bool {
    let mut ok = scan.scan_resources(document, resources, resources, decode_limit, 0);
    if ok {
        if let Some(annots) = document
            .get_dictionary(page_id)
            .ok()
            .and_then(|page| page.get(b"Annots").ok())
        {
            ok = scan.scan_annotations(document, annots, decode_limit);
        }
    }
    scan.apply(placements);
    ok
}

#[allow(clippy::too_many_arguments)]
fn walk_content<'a>(
    document: &'a Document,
    content: &[u8],
    resources: &[&'a Dictionary],
    initial_matrix: Matrix,
    placements: &mut HashMap<ObjectId, ImagePlacement>,
    auxiliary: &mut AuxiliaryUses,
    active_forms: &mut HashSet<ObjectId>,
    warnings: &mut Vec<String>,
    decode_limit: usize,
) -> bool {
    let parsed = match Content::decode(content) {
        Ok(content) => content,
        Err(_) => {
            warnings.push("an image placement content stream could not be parsed; affected images were preserved".to_string());
            return false;
        }
    };
    let mut matrix = initial_matrix;
    let mut stack = Vec::new();
    let mut ok = true;
    for operation in parsed.operations {
        match operation.operator.as_str() {
            "q" => stack.push(matrix),
            "Q" => {
                matrix = match stack.pop() {
                    Some(saved) => saved,
                    None => {
                        warnings.push("unbalanced PDF graphics state around an image; affected images were preserved".to_string());
                        return false;
                    }
                };
            }
            "cm" => {
                if let Some(concat) = Matrix::from_operands(&operation.operands) {
                    matrix = matrix.concat(concat);
                } else {
                    warnings.push("an invalid PDF transformation was encountered near an image; affected images were preserved".to_string());
                    return false;
                }
            }
            "Do" => {
                let name = match operation
                    .operands
                    .first()
                    .and_then(|object| object.as_name().ok())
                {
                    Some(name) => name,
                    None => {
                        ok = false;
                        continue;
                    }
                };
                let (id, object) = match lookup_xobject(document, resources, name) {
                    Some(value) => value,
                    None => {
                        ok = false;
                        continue;
                    }
                };
                let id = match id {
                    Some(id) => id,
                    None => {
                        warnings.push(format!(
                            "direct XObject /{} has no object identity; its image was preserved",
                            String::from_utf8_lossy(name)
                        ));
                        ok = false;
                        continue;
                    }
                };
                ok &= walk_xobject(
                    document,
                    id,
                    object,
                    resources,
                    matrix,
                    placements,
                    auxiliary,
                    active_forms,
                    warnings,
                    decode_limit,
                );
            }
            _ => {}
        }
    }
    if !stack.is_empty() {
        warnings.push("an image content stream left the graphics state unbalanced; affected images were preserved".to_string());
        ok = false;
    }
    ok
}

#[allow(clippy::too_many_arguments)]
fn walk_xobject<'a>(
    document: &'a Document,
    id: ObjectId,
    object: &'a Object,
    inherited_resources: &[&'a Dictionary],
    matrix: Matrix,
    placements: &mut HashMap<ObjectId, ImagePlacement>,
    auxiliary: &mut AuxiliaryUses,
    active_forms: &mut HashSet<ObjectId>,
    warnings: &mut Vec<String>,
    decode_limit: usize,
) -> bool {
    let stream = match object.as_stream() {
        Ok(stream) => stream,
        Err(_) => return false,
    };
    let subtype = match stream
        .dict
        .get(b"Subtype")
        .ok()
        .and_then(|value| value.as_name().ok())
    {
        Some(subtype) => subtype,
        None => return false,
    };
    if subtype == b"Image" {
        placements.entry(id).or_default().record(matrix);
        return true;
    }
    if subtype != b"Form" {
        return true;
    }
    if !active_forms.insert(id) {
        warnings.push(format!(
            "form XObject {} {} contains a reference cycle; images in that cycle were preserved",
            id.0, id.1
        ));
        return false;
    }

    let form_matrix = match stream.dict.get(b"Matrix") {
        Ok(Object::Array(values)) => match Matrix::from_operands(values) {
            Some(matrix) => matrix,
            None => {
                warnings.push(format!(
                    "form XObject {} {} has an invalid Matrix; images in it were preserved",
                    id.0, id.1
                ));
                active_forms.remove(&id);
                return false;
            }
        },
        Ok(_) => {
            warnings.push(format!(
                "form XObject {} {} has an invalid Matrix; images in it were preserved",
                id.0, id.1
            ));
            active_forms.remove(&id);
            return false;
        }
        Err(_) => Matrix::IDENTITY,
    };
    let (resources, resources_ok) = form_resources(document, &stream.dict, inherited_resources);
    if !resources_ok {
        warnings.push(format!(
            "form XObject {} {} has unusable resources; images in it were preserved",
            id.0, id.1
        ));
        active_forms.remove(&id);
        return false;
    }
    let own_len = resources.len().saturating_sub(inherited_resources.len());
    let own = resources.get(..own_len).unwrap_or_default();
    let auxiliary_ok = auxiliary.scan_resources(document, own, &resources, decode_limit, 0);
    auxiliary.apply(placements);
    if !auxiliary_ok {
        warnings.push(format!(
            "form XObject {} {} has unanalyzable auxiliary resources; images in it were preserved",
            id.0, id.1
        ));
        active_forms.remove(&id);
        return false;
    }
    let ok = match decode_stream_bounded(document, stream, decode_limit) {
        Ok(decoded) => walk_content(
            document,
            &decoded,
            &resources,
            matrix.concat(form_matrix),
            placements,
            auxiliary,
            active_forms,
            warnings,
            decode_limit,
        ),
        Err(_) => {
            warnings.push(format!(
                "form XObject {} {} could not be decoded; images in it were preserved",
                id.0, id.1
            ));
            false
        }
    };
    active_forms.remove(&id);
    ok
}

#[derive(Debug)]
enum DecodeFailure {
    Limit,
    Unsupported(&'static str),
    Invalid(String),
}

impl DecodeFailure {
    fn message(&self) -> String {
        match self {
            Self::Limit => "decoded image data exceeded max_decoded_stream_bytes".to_string(),
            Self::Unsupported(filter) => {
                format!("unsupported image filter or predictor ({filter})")
            }
            Self::Invalid(reason) => format!("invalid image data ({reason})"),
        }
    }
}

fn filter_names(
    document: &Document,
    dictionary: &Dictionary,
) -> Result<Vec<Vec<u8>>, DecodeFailure> {
    let value = match dictionary.get(b"Filter") {
        Ok(value) => value,
        Err(_) => return Ok(Vec::new()),
    };
    let (_, resolved) = document
        .dereference(value)
        .map_err(|_| DecodeFailure::Invalid("Filter reference".to_string()))?;
    match resolved {
        Object::Name(name) => Ok(vec![name.clone()]),
        Object::Array(values) => values
            .iter()
            .map(|value| {
                let (_, resolved) = document
                    .dereference(value)
                    .map_err(|_| DecodeFailure::Invalid("Filter reference".to_string()))?;
                resolved
                    .as_name()
                    .map(|name| name.to_vec())
                    .map_err(|_| DecodeFailure::Invalid("Filter is not a name".to_string()))
            })
            .collect(),
        _ => Err(DecodeFailure::Invalid(
            "Filter is not a name or array".to_string(),
        )),
    }
}

fn decode_params<'a>(
    document: &'a Document,
    dictionary: &'a Dictionary,
    index: usize,
) -> Result<Option<&'a Dictionary>, DecodeFailure> {
    let value = match dictionary.get(b"DecodeParms") {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    let (_, value) = document
        .dereference(value)
        .map_err(|_| DecodeFailure::Invalid("DecodeParms reference".to_string()))?;
    let value = match value {
        Object::Array(values) => values.get(index).ok_or_else(|| {
            DecodeFailure::Invalid("DecodeParms has fewer entries than Filter".to_string())
        })?,
        value => value,
    };
    let (_, value) = document
        .dereference(value)
        .map_err(|_| DecodeFailure::Invalid("DecodeParms entry reference".to_string()))?;
    if value.is_null() {
        return Ok(None);
    }
    value
        .as_dict()
        .map(Some)
        .map_err(|_| DecodeFailure::Invalid("DecodeParms entry is not a dictionary".to_string()))
}

/// `DCTDecode` is rejected here; only the lossy image path decodes JPEG, via
/// [`decode_image_samples`].
fn decode_stream_bounded(
    document: &Document,
    stream: &Stream,
    limit: usize,
) -> Result<Vec<u8>, DecodeFailure> {
    let filters = filter_names(document, &stream.dict)?;
    apply_filters(document, stream, &filters, limit)
}

/// Apply `filters` (a prefix of the stream's filter list, so indices match
/// its DecodeParms) to the stream content.
fn apply_filters(
    document: &Document,
    stream: &Stream,
    filters: &[Vec<u8>],
    limit: usize,
) -> Result<Vec<u8>, DecodeFailure> {
    let mut data = stream.content.clone();
    if filters.is_empty() {
        if data.len() > limit {
            return Err(DecodeFailure::Limit);
        }
        return Ok(data);
    }

    for (index, filter) in filters.iter().enumerate() {
        match filter.as_slice() {
            b"DCTDecode" | b"DCT" => return Err(DecodeFailure::Unsupported("DCTDecode")),
            b"ASCII85Decode" | b"A85" => {
                data = decode_ascii85(&data, limit)?;
            }
            b"FlateDecode" | b"Fl" => {
                data = decode_flate(&data, decode_params(document, &stream.dict, index)?, limit)?;
            }
            _ => return Err(DecodeFailure::Unsupported("filter")),
        }
        if data.len() > limit {
            return Err(DecodeFailure::Limit);
        }
    }
    Ok(data)
}

fn decode_flate(
    input: &[u8],
    params: Option<&Dictionary>,
    limit: usize,
) -> Result<Vec<u8>, DecodeFailure> {
    let decoder = ZlibDecoder::new(input);
    let mut output = Vec::new();
    let read_limit = limit.saturating_add(1);
    decoder
        .take(read_limit as u64)
        .read_to_end(&mut output)
        .map_err(|error| DecodeFailure::Invalid(error.to_string()))?;
    if output.len() > limit {
        return Err(DecodeFailure::Limit);
    }
    apply_predictor(output, params, limit)
}

fn integer_param(dictionary: &Dictionary, key: &[u8], default: i64) -> Result<i64, DecodeFailure> {
    match dictionary.get(key) {
        Ok(Object::Integer(value)) => Ok(*value),
        Ok(Object::Real(value)) => Ok(*value as i64),
        Ok(_) => Err(DecodeFailure::Invalid("DecodeParms value".to_string())),
        Err(_) => Ok(default),
    }
}

fn apply_predictor(
    mut data: Vec<u8>,
    params: Option<&Dictionary>,
    limit: usize,
) -> Result<Vec<u8>, DecodeFailure> {
    let Some(params) = params else {
        return Ok(data);
    };
    let predictor = integer_param(params, b"Predictor", 1)?;
    if predictor == 1 {
        return Ok(data);
    }
    let columns = integer_param(params, b"Columns", 1)?;
    let colors = integer_param(params, b"Colors", 1)?;
    let bits = integer_param(params, b"BitsPerComponent", 8)?;
    if columns <= 0 || colors <= 0 || bits <= 0 {
        return Err(DecodeFailure::Invalid(
            "invalid predictor dimensions".to_string(),
        ));
    }
    let columns = usize::try_from(columns)
        .map_err(|_| DecodeFailure::Invalid("predictor columns".to_string()))?;
    let colors = usize::try_from(colors)
        .map_err(|_| DecodeFailure::Invalid("predictor colors".to_string()))?;
    let bits =
        usize::try_from(bits).map_err(|_| DecodeFailure::Invalid("predictor bits".to_string()))?;
    if bits != 8 {
        return Err(DecodeFailure::Unsupported("non-8-bit predictor"));
    }
    let row_bytes = columns.checked_mul(colors).ok_or(DecodeFailure::Limit)?;
    match predictor {
        2 => {
            if !data.len().is_multiple_of(row_bytes) {
                return Err(DecodeFailure::Invalid(
                    "TIFF predictor row length".to_string(),
                ));
            }
            for row in data.chunks_exact_mut(row_bytes) {
                for index in colors..row_bytes {
                    let left = row.get(index - colors).copied().unwrap_or(0);
                    if let Some(sample) = row.get_mut(index) {
                        *sample = sample.wrapping_add(left);
                    }
                }
            }
        }
        10..=15 => {
            let encoded_row = row_bytes.checked_add(1).ok_or(DecodeFailure::Limit)?;
            if !data.len().is_multiple_of(encoded_row) {
                return Err(DecodeFailure::Invalid(
                    "PNG predictor row length".to_string(),
                ));
            }
            let rows = data.len() / encoded_row;
            let output_len = rows.checked_mul(row_bytes).ok_or(DecodeFailure::Limit)?;
            if output_len > limit {
                return Err(DecodeFailure::Limit);
            }
            let mut decoded = vec![0u8; output_len];
            let mut previous = vec![0u8; row_bytes];
            for (encoded, output) in data
                .chunks_exact(encoded_row)
                .zip(decoded.chunks_exact_mut(row_bytes))
            {
                let (filter, source) = encoded
                    .split_first()
                    .ok_or_else(|| DecodeFailure::Invalid("PNG predictor row".to_string()))?;
                let filter = PngFilter::from_byte(*filter)
                    .ok_or_else(|| DecodeFailure::Invalid("PNG predictor filter".to_string()))?;
                for (index, byte) in source.iter().enumerate() {
                    let left = index
                        .checked_sub(colors)
                        .and_then(|left| output.get(left))
                        .copied()
                        .unwrap_or(0);
                    let up = previous.get(index).copied().unwrap_or(0);
                    let upper_left = index
                        .checked_sub(colors)
                        .and_then(|left| previous.get(left))
                        .copied()
                        .unwrap_or(0);
                    if let Some(sample) = output.get_mut(index) {
                        *sample = byte.wrapping_add(filter.predict(left, up, upper_left));
                    }
                }
                previous.clear();
                previous.extend_from_slice(output);
            }
            data = decoded;
        }
        _ => return Err(DecodeFailure::Unsupported("predictor")),
    }
    Ok(data)
}

#[derive(Clone, Copy)]
enum PngFilter {
    None,
    Sub,
    Up,
    Average,
    Paeth,
}

impl PngFilter {
    const ALL: [Self; 5] = [Self::None, Self::Sub, Self::Up, Self::Average, Self::Paeth];

    fn from_byte(byte: u8) -> Option<Self> {
        Self::ALL.get(usize::from(byte)).copied()
    }

    fn byte(self) -> u8 {
        match self {
            Self::None => 0,
            Self::Sub => 1,
            Self::Up => 2,
            Self::Average => 3,
            Self::Paeth => 4,
        }
    }

    fn predict(self, left: u8, up: u8, upper_left: u8) -> u8 {
        match self {
            Self::None => 0,
            Self::Sub => left,
            Self::Up => up,
            Self::Average => ((u16::from(left) + u16::from(up)) / 2) as u8,
            Self::Paeth => paeth(left, up, upper_left),
        }
    }
}

fn paeth(left: u8, up: u8, upper_left: u8) -> u8 {
    let p = left as i32 + up as i32 - upper_left as i32;
    let pa = (p - left as i32).unsigned_abs();
    let pb = (p - up as i32).unsigned_abs();
    let pc = (p - upper_left as i32).unsigned_abs();
    if pa <= pb && pa <= pc {
        left
    } else if pb <= pc {
        up
    } else {
        upper_left
    }
}

fn decode_ascii85(input: &[u8], limit: usize) -> Result<Vec<u8>, DecodeFailure> {
    let mut output = Vec::new();
    let mut group = [0u8; 5];
    let mut count = 0usize;
    let mut bytes = input.iter().copied().peekable();
    while let Some(byte) = bytes.next() {
        if byte.is_ascii_whitespace() {
            continue;
        }
        if byte == b'~' {
            if bytes.peek() == Some(&b'>') {
                break;
            }
            return Err(DecodeFailure::Invalid("ASCII85 terminator".to_string()));
        }
        if byte == b'z' {
            if count != 0 {
                return Err(DecodeFailure::Invalid("ASCII85 z in group".to_string()));
            }
            if output.len().saturating_add(4) > limit {
                return Err(DecodeFailure::Limit);
            }
            output.extend_from_slice(&[0, 0, 0, 0]);
            continue;
        }
        if !(b'!'..=b'u').contains(&byte) {
            return Err(DecodeFailure::Invalid("ASCII85 character".to_string()));
        }
        let slot = group
            .get_mut(count)
            .ok_or_else(|| DecodeFailure::Invalid("ASCII85 group".to_string()))?;
        *slot = byte - b'!';
        count += 1;
        if count == 5 {
            let value = ascii85_group(group)?;
            if output.len().saturating_add(4) > limit {
                return Err(DecodeFailure::Limit);
            }
            output.extend_from_slice(&value.to_be_bytes());
            count = 0;
        }
    }
    if count == 1 {
        return Err(DecodeFailure::Invalid("ASCII85 partial group".to_string()));
    }
    if count > 1 {
        for slot in group.iter_mut().skip(count) {
            *slot = 84;
        }
        let value = ascii85_group(group)?;
        let bytes = value.to_be_bytes();
        let take = count - 1;
        if output.len().saturating_add(take) > limit {
            return Err(DecodeFailure::Limit);
        }
        let partial = bytes
            .get(..take)
            .ok_or_else(|| DecodeFailure::Invalid("ASCII85 group".to_string()))?;
        output.extend_from_slice(partial);
    }
    Ok(output)
}

fn ascii85_group(group: [u8; 5]) -> Result<u32, DecodeFailure> {
    let value = u64::from(group[0]) * 85_u64.pow(4)
        + u64::from(group[1]) * 85_u64.pow(3)
        + u64::from(group[2]) * 85_u64.pow(2)
        + u64::from(group[3]) * 85
        + u64::from(group[4]);
    u32::try_from(value).map_err(|_| DecodeFailure::Invalid("ASCII85 group overflow".to_string()))
}

#[derive(Debug)]
struct PreparedImage {
    encoded: Vec<u8>,
    width: u32,
    height: u32,
    color_space: &'static [u8],
}

const IMAGE_DICTIONARY_OVERHEAD: usize = 96;
const MAX_JPEG_DIMENSION: u32 = 65_535;

/// Re-encode simple 8-bit samples with PNG predictor rows and Flate.  The
/// predictor is part of the PDF image representation, so decoding it restores
/// exactly the original samples.  We only replace the stream when the complete
/// representation is smaller than the best simple lossless representation.
fn optimize_lossless_image(
    document: &mut Document,
    id: ObjectId,
    decode_limit: usize,
) -> Result<bool, String> {
    let stream = document
        .get_object(id)
        .and_then(Object::as_stream)
        .map_err(|_| "object is not an image stream".to_string())?;
    let dictionary = &stream.dict;
    if dictionary
        .get(b"Subtype")
        .ok()
        .and_then(|value| value.as_name().ok())
        != Some(b"Image")
    {
        return Ok(false);
    }
    if dictionary
        .get(b"ImageMask")
        .ok()
        .and_then(|value| value.as_bool().ok())
        .unwrap_or(false)
        || dictionary.has(b"DecodeParms")
    {
        return Ok(false);
    }
    let filters = filter_names(document, dictionary).map_err(|error| error.message())?;
    if !filters.is_empty()
        && filters != vec![b"FlateDecode".to_vec()]
        && filters != vec![b"Fl".to_vec()]
    {
        return Ok(false);
    }
    let width = image_dimension(dictionary, b"Width")?;
    let height = image_dimension(dictionary, b"Height")?;
    let color_space = simple_color_space(document, dictionary)?;
    let channels = if color_space == b"DeviceGray" { 1 } else { 3 };
    if dictionary
        .get(b"BitsPerComponent")
        .and_then(Object::as_i64)
        .ok()
        != Some(8)
    {
        return Ok(false);
    }
    let expected = usize::try_from(width)
        .ok()
        .and_then(|width| {
            usize::try_from(height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .and_then(|pixels| pixels.checked_mul(channels))
        .ok_or_else(|| "image sample size overflow".to_string())?;
    if expected > decode_limit {
        return Err("decoded image samples exceed max_decoded_stream_bytes".to_string());
    }
    let decoded =
        decode_stream_bounded(document, stream, decode_limit).map_err(|error| error.message())?;
    if decoded.len() != expected {
        return Ok(false);
    }
    let encoded = encode_predictor(&decoded, width, height, channels, decode_limit)?;
    let baseline = lossless_baseline_len(document, stream);
    if encoded.len().saturating_add(IMAGE_DICTIONARY_OVERHEAD) >= baseline {
        return Ok(false);
    }

    let stream = document
        .get_object_mut(id)
        .and_then(Object::as_stream_mut)
        .map_err(|_| "image object disappeared before lossless optimisation".to_string())?;
    let mut decode_parms = Dictionary::new();
    decode_parms.set("Predictor", 15_i64);
    decode_parms.set("Colors", channels as i64);
    decode_parms.set("Columns", width as i64);
    decode_parms.set("BitsPerComponent", 8_i64);
    stream
        .dict
        .set("Filter", Object::Name(b"FlateDecode".to_vec()));
    stream
        .dict
        .set("DecodeParms", Object::Dictionary(decode_parms));
    stream.set_content(encoded);
    Ok(true)
}

fn lossless_baseline_len(document: &Document, stream: &Stream) -> usize {
    let filters = filter_names(document, &stream.dict).unwrap_or_default();
    if filters.is_empty() {
        let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
        if encoder.write_all(&stream.content).is_ok() {
            if let Ok(encoded) = encoder.finish() {
                return stream.content.len().min(encoded.len());
            }
        }
    }
    stream.content.len()
}

fn encode_predictor(
    samples: &[u8],
    width: u32,
    height: u32,
    channels: usize,
    limit: usize,
) -> Result<Vec<u8>, String> {
    let width = usize::try_from(width).map_err(|_| "predictor width overflow".to_string())?;
    let height = usize::try_from(height).map_err(|_| "predictor height overflow".to_string())?;
    let row_bytes = width
        .checked_mul(channels)
        .ok_or_else(|| "predictor row size overflow".to_string())?;
    let encoded_len = height
        .checked_mul(row_bytes.saturating_add(1))
        .ok_or_else(|| "predictor output size overflow".to_string())?;
    if encoded_len > limit.saturating_add(height) {
        return Err("predictor output exceeds max_decoded_stream_bytes".to_string());
    }
    let mut predicted = Vec::with_capacity(encoded_len);
    let zeros = vec![0u8; row_bytes];
    let mut previous: &[u8] = &zeros;
    for row_index in 0..height {
        let start = row_index
            .checked_mul(row_bytes)
            .ok_or_else(|| "predictor row offset overflow".to_string())?;
        let end = start
            .checked_add(row_bytes)
            .ok_or_else(|| "predictor row offset overflow".to_string())?;
        let row = samples
            .get(start..end)
            .ok_or_else(|| "predictor sample length mismatch".to_string())?;
        let filter = choose_predictor(row, previous, channels);
        predicted.push(filter.byte());
        predicted.extend(png_residuals(filter, row, previous, channels));
        previous = row;
    }
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
    encoder
        .write_all(&predicted)
        .map_err(|error| format!("lossless image encoding failed: {error}"))?;
    encoder
        .finish()
        .map_err(|error| format!("lossless image encoding failed: {error}"))
}

fn png_residuals<'a>(
    filter: PngFilter,
    row: &'a [u8],
    previous: &'a [u8],
    channels: usize,
) -> impl Iterator<Item = u8> + 'a {
    row.iter().enumerate().map(move |(index, byte)| {
        let left = index
            .checked_sub(channels)
            .and_then(|left| row.get(left))
            .copied()
            .unwrap_or(0);
        let up = previous.get(index).copied().unwrap_or(0);
        let upper_left = index
            .checked_sub(channels)
            .and_then(|left| previous.get(left))
            .copied()
            .unwrap_or(0);
        byte.wrapping_sub(filter.predict(left, up, upper_left))
    })
}

fn choose_predictor(row: &[u8], previous: &[u8], channels: usize) -> PngFilter {
    let mut best_filter = PngFilter::None;
    let mut best_score = u64::MAX;
    for filter in PngFilter::ALL {
        let score = png_residuals(filter, row, previous, channels).fold(0u64, |score, residual| {
            score.saturating_add(u64::from(i16::from(residual as i8).unsigned_abs()))
        });
        if score < best_score {
            best_score = score;
            best_filter = filter;
        }
    }
    best_filter
}

/// Decode the samples of an image the lossy pass may re-encode.  A final
/// `DCTDecode` filter is decoded with the crate's own baseline JPEG decoder
/// after any preceding filters.
fn decode_image_samples(
    document: &Document,
    stream: &Stream,
    width: u32,
    height: u32,
    channels: usize,
    limit: usize,
) -> Result<Vec<u8>, String> {
    let filters = filter_names(document, &stream.dict).map_err(|error| error.message())?;
    let Some((_, preceding)) = filters
        .split_last()
        .filter(|(last, _)| matches!(last.as_slice(), b"DCTDecode" | b"DCT"))
    else {
        return decode_stream_bounded(document, stream, limit).map_err(|error| error.message());
    };
    let color_transform = match decode_params(document, &stream.dict, preceding.len())
        .map_err(|error| error.message())?
    {
        Some(params) => dct_color_transform(params)?,
        None => None,
    };
    let data =
        apply_filters(document, stream, preceding, limit).map_err(|error| error.message())?;
    crate::jpeg::decode(&data, width, height, channels, limit, color_transform)
        .map_err(|error| error.message())
}

/// `/ColorTransform` is the only DCTDecode parameter that is understood.
fn dct_color_transform(params: &Dictionary) -> Result<Option<bool>, String> {
    let mut transform = None;
    for (key, value) in params.iter() {
        match (key.as_slice(), value) {
            (b"ColorTransform", Object::Integer(0)) => transform = Some(false),
            (b"ColorTransform", Object::Integer(1)) => transform = Some(true),
            _ => return Err("unsupported DCTDecode parameters".to_string()),
        }
    }
    Ok(transform)
}

fn prepare_image(
    document: &Document,
    id: ObjectId,
    placement: ImagePlacement,
    target_dpi: Option<u32>,
    options: &Options,
    decode_limit: usize,
) -> Result<Option<PreparedImage>, String> {
    let stream = document
        .get_object(id)
        .and_then(Object::as_stream)
        .map_err(|_| "object is not an image stream".to_string())?;
    let dictionary = &stream.dict;
    let subtype = dictionary
        .get(b"Subtype")
        .ok()
        .and_then(|value| value.as_name().ok())
        .ok_or_else(|| "image stream has no Subtype /Image".to_string())?;
    if subtype != b"Image" {
        return Ok(None);
    }
    if dictionary
        .get(b"ImageMask")
        .ok()
        .and_then(|value| value.as_bool().ok())
        .unwrap_or(false)
    {
        return Err("ImageMask objects are unsafe to convert to JPEG".to_string());
    }
    if dictionary.has(b"Mask")
        || dictionary.has(b"SMask")
        || dictionary.has(b"SMaskInData")
        || dictionary.has(b"Decode")
    {
        return Err("mask or Decode entries make JPEG conversion unsafe".to_string());
    }

    let width = image_dimension(dictionary, b"Width")?;
    let height = image_dimension(dictionary, b"Height")?;
    let color_space = simple_color_space(document, dictionary)?;
    let channels = if color_space == b"DeviceGray" {
        1usize
    } else {
        3usize
    };
    let bits = dictionary
        .get(b"BitsPerComponent")
        .and_then(Object::as_i64)
        .map_err(|_| "BitsPerComponent is not an integer".to_string())?;
    if bits != 8 {
        return Err("only 8-bit DeviceRGB/DeviceGray images are eligible".to_string());
    }

    let pixels = usize::try_from(width)
        .ok()
        .and_then(|width| {
            usize::try_from(height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .ok_or_else(|| "image dimensions overflow".to_string())?;
    let expected = pixels
        .checked_mul(channels)
        .ok_or_else(|| "image sample size overflow".to_string())?;
    if expected > decode_limit {
        return Err("decoded image samples exceed max_decoded_stream_bytes".to_string());
    }

    let pixels = decode_image_samples(document, stream, width, height, channels, decode_limit)?;
    if pixels.len() != expected {
        return Err(format!(
            "decoded sample length {} does not match {}x{} image",
            pixels.len(),
            width,
            height
        ));
    }

    let (target_width, target_height) = target_dimensions(width, height, placement, target_dpi)?;
    if target_width == 0 || target_height == 0 {
        return Err("target pixel dimensions are empty".to_string());
    }
    let (width, height, pixels) = if target_width < width || target_height < height {
        let resized = if channels == 1 {
            let image = image::GrayImage::from_raw(width, height, pixels)
                .ok_or_else(|| "decoded grayscale samples could not form an image".to_string())?;
            imageops::resize(&image, target_width, target_height, FilterType::Triangle).into_raw()
        } else {
            let image = image::RgbImage::from_raw(width, height, pixels)
                .ok_or_else(|| "decoded RGB samples could not form an image".to_string())?;
            imageops::resize(&image, target_width, target_height, FilterType::Triangle).into_raw()
        };
        (target_width, target_height, resized)
    } else {
        (width, height, pixels)
    };

    let expected_samples = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(channels as u64));
    if width == 0
        || height == 0
        || width > MAX_JPEG_DIMENSION
        || height > MAX_JPEG_DIMENSION
        || expected_samples != Some(pixels.len() as u64)
    {
        return Err("image dimensions are not encodable as JPEG".to_string());
    }

    let mut encoded = Vec::new();
    let mut encoder =
        JpegEncoder::new_with_quality(&mut encoded, options.jpeg_quality.clamp(1, 100));
    let color_type = if channels == 1 {
        ExtendedColorType::L8
    } else {
        ExtendedColorType::Rgb8
    };
    encoder
        .encode(&pixels, width, height, color_type)
        .map_err(|error| format!("JPEG encoding failed: {error}"))?;
    if encoded.is_empty() {
        return Err("JPEG encoder returned an empty stream".to_string());
    }
    Ok(Some(PreparedImage {
        encoded,
        width,
        height,
        color_space: if channels == 1 {
            b"DeviceGray"
        } else {
            b"DeviceRGB"
        },
    }))
}

fn image_dimension(dictionary: &Dictionary, key: &[u8]) -> Result<u32, String> {
    let value = dictionary
        .get(key)
        .and_then(Object::as_i64)
        .map_err(|_| format!("{} is not an integer", String::from_utf8_lossy(key)))?;
    if value <= 0 {
        return Err(format!("{} is not positive", String::from_utf8_lossy(key)));
    }
    u32::try_from(value).map_err(|_| format!("{} is too large", String::from_utf8_lossy(key)))
}

fn simple_color_space(
    document: &Document,
    dictionary: &Dictionary,
) -> Result<&'static [u8], String> {
    let value = dictionary
        .get(b"ColorSpace")
        .map_err(|_| "missing ColorSpace".to_string())?;
    let (_, value) = document
        .dereference(value)
        .map_err(|_| "ColorSpace reference could not be resolved".to_string())?;
    match value {
        Object::Name(name) if name == b"DeviceRGB" => Ok(b"DeviceRGB"),
        Object::Name(name) if name == b"DeviceGray" => Ok(b"DeviceGray"),
        _ => Err("only direct DeviceRGB and DeviceGray colour spaces are eligible".to_string()),
    }
}

fn target_dimensions(
    width: u32,
    height: u32,
    placement: ImagePlacement,
    target_dpi: Option<u32>,
) -> Result<(u32, u32), String> {
    let Some(target_dpi) = target_dpi else {
        return Ok((width, height));
    };
    if target_dpi == 0
        || !placement.max_width_points.is_finite()
        || !placement.max_height_points.is_finite()
    {
        return Err("target DPI or placement dimensions are invalid".to_string());
    }
    let scale = f64::from(target_dpi) / 72.0;
    let requested_width = (placement.max_width_points * scale).ceil();
    let requested_height = (placement.max_height_points * scale).ceil();
    if !requested_width.is_finite() || !requested_height.is_finite() {
        return Err("target pixel dimensions overflow".to_string());
    }
    let requested_width = requested_width.max(1.0).min(f64::from(width)) as u32;
    let requested_height = requested_height.max(1.0).min(f64::from(height)) as u32;
    Ok((requested_width, requested_height))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn predictor_rows_round_trip_exact_samples() {
        let samples = vec![
            0, 20, 40, 60, 80, 100, // row 1
            120, 140, 160, 180, 200, 220, // row 2
        ];
        let encoded = encode_predictor(&samples, 2, 2, 3, 4096).unwrap();
        let mut decoder = ZlibDecoder::new(encoded.as_slice());
        let mut predicted = Vec::new();
        decoder.read_to_end(&mut predicted).unwrap();
        let mut params = Dictionary::new();
        params.set("Predictor", 15_i64);
        params.set("Colors", 3_i64);
        params.set("Columns", 2_i64);
        params.set("BitsPerComponent", 8_i64);
        let decoded = apply_predictor(predicted, Some(&params), 4096).unwrap();
        assert_eq!(decoded, samples);
    }

    #[test]
    fn ascii85_rejects_u32_overflow() {
        assert!(decode_ascii85(b"uuuuu", 64).is_err());
    }

    use lopdf::dictionary;

    fn noisy_rgb_image(side: u32) -> Stream {
        let mut state = 0x2545_f491_u32;
        let samples = (0..side * side * 3)
            .map(|index| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                ((index / 7) as u8).wrapping_add((state >> 24) as u8 & 0x3f)
            })
            .collect();
        Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Image",
                "Width" => i64::from(side),
                "Height" => i64::from(side),
                "ColorSpace" => "DeviceRGB",
                "BitsPerComponent" => 8,
            },
            samples,
        )
    }

    fn truetype_font(document: &mut Document, program: Stream) -> ObjectId {
        let file = document.add_object(program);
        let descriptor = document.add_object(dictionary! {
            "Type" => "FontDescriptor",
            "FontName" => "Test",
            "Flags" => 32,
            "FontFile2" => file,
        });
        document.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "TrueType",
            "BaseFont" => "Test",
            "FontDescriptor" => descriptor,
        })
    }

    fn lzw_font_program(len: usize) -> Stream {
        let mut dictionary = Dictionary::new();
        dictionary.set("Filter", "LZWDecode");
        Stream::new(dictionary, vec![0x80; len])
    }

    fn type3_font(document: &mut Document, glyph: &str, resources: Object) -> ObjectId {
        let proc_id =
            document.add_object(Stream::new(Dictionary::new(), glyph.as_bytes().to_vec()));
        document.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type3",
            "FontBBox" => vec![0.into(), 0.into(), 1000.into(), 1000.into()],
            "FontMatrix" => vec![0.001.into(), 0.into(), 0.into(), 0.001.into(), 0.into(), 0.into()],
            "CharProcs" => dictionary! { "a" => proc_id },
            "Encoding" => dictionary! {
                "Type" => "Encoding",
                "Differences" => vec![97.into(), "a".into()],
            },
            "FirstChar" => 97,
            "LastChar" => 97,
            "Widths" => vec![1000.into()],
            "Resources" => resources,
        })
    }

    fn add_page(
        document: &mut Document,
        pages_id: ObjectId,
        resources: Object,
        content: &str,
    ) -> ObjectId {
        let content_id =
            document.add_object(Stream::new(Dictionary::new(), content.as_bytes().to_vec()));
        document.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), 200.into(), 200.into()],
            "Resources" => resources,
            "Contents" => content_id,
        })
    }

    fn install_pages(document: &mut Document, pages_id: ObjectId, kids: &[ObjectId]) {
        document.objects.insert(
            pages_id,
            dictionary! {
                "Type" => "Pages",
                "Kids" => kids.iter().map(|id| Object::Reference(*id)).collect::<Vec<_>>(),
                "Count" => kids.len() as i64,
            }
            .into(),
        );
    }

    fn finish(mut document: Document, pages_id: ObjectId, kids: &[ObjectId]) -> Vec<u8> {
        install_pages(&mut document, pages_id, kids);
        let catalog = document.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        document.trailer.set("Root", catalog);
        let mut bytes = Vec::new();
        document.save_to(&mut bytes).unwrap();
        bytes
    }

    fn screen_compress(bytes: &[u8]) -> (Document, Report) {
        let result = crate::compress(bytes, &Options::for_preset(crate::Preset::Screen)).unwrap();
        (Document::load_mem(&result.bytes).unwrap(), result.report)
    }

    fn image_dictionaries(document: &Document) -> Vec<&Dictionary> {
        document
            .objects
            .values()
            .filter_map(|object| object.as_stream().ok())
            .filter(|stream| {
                stream.dict.get(b"Subtype").and_then(Object::as_name).ok() == Some(b"Image")
            })
            .map(|stream| &stream.dict)
            .collect()
    }

    fn jpeg_image_document(jpeg: Vec<u8>, width: i64, height: i64) -> Vec<u8> {
        let mut document = Document::with_version("1.5");
        let pages_id = document.new_object_id();
        let image = document.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Image",
                "Width" => width,
                "Height" => height,
                "ColorSpace" => "DeviceRGB",
                "BitsPerComponent" => 8,
                "Filter" => "DCTDecode",
            },
            jpeg,
        ));
        let page = add_page(
            &mut document,
            pages_id,
            dictionary! { "XObject" => dictionary! { "Im1" => image } }.into(),
            "q 100 0 0 100 50 50 cm /Im1 Do Q",
        );
        finish(document, pages_id, &[page])
    }

    fn single_image(document: &Document) -> &Stream {
        document
            .objects
            .values()
            .filter_map(|object| object.as_stream().ok())
            .find(|stream| {
                stream.dict.get(b"Subtype").and_then(Object::as_name).ok() == Some(b"Image")
            })
            .unwrap()
    }

    #[test]
    fn existing_baseline_jpeg_is_downsampled() {
        let source = noisy_rgb_image(600);
        let mut jpeg = Vec::new();
        JpegEncoder::new_with_quality(&mut jpeg, 90)
            .encode(&source.content, 600, 600, ExtendedColorType::Rgb8)
            .unwrap();
        let bytes = jpeg_image_document(jpeg.clone(), 600, 600);

        let (output, report) = screen_compress(&bytes);
        assert!(report.images_optimized >= 1, "{:?}", report.warnings);
        let stream = single_image(&output);
        assert_eq!(
            stream.dict.get(b"Filter").and_then(Object::as_name).ok(),
            Some(b"DCTDecode".as_slice())
        );
        assert_eq!(
            stream.dict.get(b"Width").and_then(Object::as_i64).ok(),
            Some(100)
        );
        assert_eq!(
            stream.dict.get(b"Height").and_then(Object::as_i64).ok(),
            Some(100)
        );
        assert!(stream.content.len() < jpeg.len());
        assert!(crate::jpeg::decode(&stream.content, 100, 100, 3, 1 << 20, None).is_ok());
    }

    #[test]
    fn malformed_jpeg_is_preserved_byte_for_byte() {
        let jpeg = vec![0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x11, 0x08, 0xFF, 0xFF];
        let bytes = jpeg_image_document(jpeg.clone(), 600, 600);

        let (output, report) = screen_compress(&bytes);
        assert_eq!(report.images_optimized, 0);
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.contains("was preserved: invalid JPEG data")),
            "{:?}",
            report.warnings
        );
        assert_eq!(single_image(&output).content, jpeg);
    }

    #[test]
    fn progressive_jpeg_is_preserved() {
        let jpeg = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/jpeg/progressive.jpg"
        ))
        .to_vec();
        let bytes = jpeg_image_document(jpeg.clone(), 37, 29);

        let (output, report) = screen_compress(&bytes);
        assert_eq!(report.images_optimized, 0);
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.contains("unsupported JPEG (progressive JPEG)")),
            "{:?}",
            report.warnings
        );
        assert_eq!(single_image(&output).content, jpeg);
    }

    #[test]
    fn undecodable_font_program_does_not_disable_lossy_images() {
        let mut document = Document::with_version("1.5");
        let pages_id = document.new_object_id();
        let image = document.add_object(noisy_rgb_image(600));
        let font = truetype_font(&mut document, lzw_font_program(4096));
        let page = add_page(
            &mut document,
            pages_id,
            dictionary! {
                "XObject" => dictionary! { "Im1" => image },
                "Font" => dictionary! { "F1" => font },
            }
            .into(),
            "q 100 0 0 100 50 50 cm /Im1 Do Q BT /F1 12 Tf 10 10 Td (a) Tj ET",
        );
        let bytes = finish(document, pages_id, &[page]);

        let (output, report) = screen_compress(&bytes);
        assert!(report.images_optimized >= 1, "{:?}", report.warnings);
        assert!(
            !report
                .warnings
                .iter()
                .any(|w| w.contains("could not be analysed")),
            "{:?}",
            report.warnings
        );
        let images = image_dictionaries(&output);
        assert_eq!(images.len(), 1);
        assert_eq!(
            images[0].get(b"Filter").and_then(Object::as_name).ok(),
            Some(b"DCTDecode".as_slice())
        );
        assert_eq!(
            images[0].get(b"Width").and_then(Object::as_i64).ok(),
            Some(100)
        );
    }

    #[test]
    fn type3_glyph_image_is_ambiguous_without_failing_the_guard() {
        let mut document = Document::with_version("1.5");
        let pages_id = document.new_object_id();
        let glyph_image = document.add_object(noisy_rgb_image(600));
        let page_image = document.add_object(noisy_rgb_image(600));
        let font = type3_font(
            &mut document,
            "1000 0 d0 q 1000 0 0 1000 0 0 cm /Im1 Do Q",
            dictionary! { "XObject" => dictionary! { "Im1" => glyph_image } }.into(),
        );
        let page = add_page(
            &mut document,
            pages_id,
            dictionary! {
                "XObject" => dictionary! { "Im1" => glyph_image, "Im2" => page_image },
                "Font" => dictionary! { "F1" => font },
            }
            .into(),
            "q 100 0 0 100 50 50 cm /Im1 Do Q q 100 0 0 100 0 0 cm /Im2 Do Q BT /F1 12 Tf 10 10 Td (a) Tj ET",
        );
        let bytes = finish(document, pages_id, &[page]);

        let (output, report) = screen_compress(&bytes);
        assert!(
            !report
                .warnings
                .iter()
                .any(|w| w.contains("could not be analysed")
                    || w.contains("lossy image optimisation skipped")),
            "{:?}",
            report.warnings
        );
        let mut widths: Vec<(bool, i64)> = image_dictionaries(&output)
            .into_iter()
            .map(|dictionary| {
                (
                    dictionary.get(b"Filter").and_then(Object::as_name).ok()
                        == Some(b"DCTDecode".as_slice()),
                    dictionary
                        .get(b"Width")
                        .and_then(Object::as_i64)
                        .unwrap_or(0),
                )
            })
            .collect();
        widths.sort_unstable();
        assert_eq!(widths, vec![(false, 600), (true, 100)]);
    }

    #[test]
    fn type3_glyph_falls_back_to_page_resources() {
        let mut document = Document::with_version("1.5");
        let pages_id = document.new_object_id();
        let image = document.add_object(noisy_rgb_image(600));
        let font = type3_font(
            &mut document,
            "1000 0 d0 q 1000 0 0 1000 0 0 cm /Im1 Do Q",
            Dictionary::new().into(),
        );
        let page = add_page(
            &mut document,
            pages_id,
            dictionary! {
                "XObject" => dictionary! { "Im1" => image },
                "Font" => dictionary! { "F1" => font },
            }
            .into(),
            "q 100 0 0 100 50 50 cm /Im1 Do Q BT /F1 12 Tf 10 10 Td (a) Tj ET",
        );
        install_pages(&mut document, pages_id, &[page]);
        let mut placements = HashMap::new();
        let (resources, resources_ok) = page_resources(&document, page);
        assert!(resources_ok);
        let mut scan = AuxiliaryUses::default();
        assert!(guard_auxiliary_image_uses(
            &document,
            page,
            &resources,
            &mut placements,
            &mut scan,
            1 << 20,
        ));
        assert!(placements
            .get(&image)
            .is_some_and(|placement| placement.ambiguous));
    }

    #[test]
    fn shared_font_programs_are_never_decoded_and_type3_is_scanned_once() {
        let mut document = Document::with_version("1.5");
        let pages_id = document.new_object_id();
        let image = document.add_object(noisy_rgb_image(8));
        let truetype = truetype_font(&mut document, lzw_font_program(4 << 20));
        let type3 = type3_font(
            &mut document,
            "1000 0 d0 q /Im1 Do Q",
            dictionary! {
                "XObject" => dictionary! { "Im1" => image },
            }
            .into(),
        );
        let resources = document.add_object(dictionary! {
            "Font" => dictionary! { "F1" => truetype, "F2" => type3 },
        });
        let kids: Vec<ObjectId> = (0..50)
            .map(|_| {
                add_page(
                    &mut document,
                    pages_id,
                    resources.into(),
                    "BT /F1 12 Tf (a) Tj ET",
                )
            })
            .collect();
        install_pages(&mut document, pages_id, &kids);

        let mut placements = HashMap::new();
        let mut scan = AuxiliaryUses::default();
        for page in kids {
            let (chain, chain_ok) = page_resources(&document, page);
            assert!(chain_ok);
            assert!(guard_auxiliary_image_uses(
                &document,
                page,
                &chain,
                &mut placements,
                &mut scan,
                1 << 20,
            ));
        }
        assert_eq!(scan.visited.len(), 1);
        assert!(placements
            .get(&image)
            .is_some_and(|placement| placement.ambiguous));
    }
}
