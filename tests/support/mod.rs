//! Independent fixtures, raster metrics, and optional external verification.
pub mod reference;
use lopdf::{dictionary, Document, Object, Stream};
pub mod structure;

pub fn save(mut document: Document) -> Vec<u8> {
    let mut bytes = Vec::new();
    document.save_to(&mut bytes).unwrap();
    bytes
}

/// Two pages, text, vectors, links, a form field, and a high-resolution RGB image.
pub fn fixture() -> Vec<u8> {
    let mut doc = Document::with_version("1.7");
    let pages = doc.new_object_id();
    let font =
        doc.add_object(dictionary! {"Type"=>"Font", "Subtype"=>"Type1", "BaseFont"=>"Helvetica"});
    let mut pixels = Vec::with_capacity(600 * 600 * 3);
    for y in 0..600 {
        for x in 0..600 {
            pixels.extend_from_slice(&[(x * 255 / 599) as u8, (y * 255 / 599) as u8, 96]);
        }
    }
    let image = doc.add_object(Stream::new(dictionary! {"Type"=>"XObject", "Subtype"=>"Image", "Width"=>600, "Height"=>600, "ColorSpace"=>"DeviceRGB", "BitsPerComponent"=>8}, pixels));
    let link = doc.add_object(dictionary! {"Type"=>"Annot", "Subtype"=>"Link", "Rect"=>vec![50.into(), 690.into(), 300.into(), 720.into()], "Border"=>vec![0.into(),0.into(),0.into()], "A"=>dictionary!{"S"=>"URI", "URI"=>Object::string_literal("https://example.com/compression")}});
    let appearance = doc.add_object(Stream::new(dictionary!{"Type"=>"XObject", "Subtype"=>"Form", "BBox"=>vec![0.into(),0.into(),180.into(),24.into()], "Resources"=>dictionary!{"Font"=>dictionary!{"F1"=>font}}}, b"q 0.95 g 0 0 180 24 re f 0 g BT /F1 12 Tf 4 6 Td (Preserve me) Tj ET Q".to_vec()));
    let field = doc.add_object(dictionary! {"Type"=>"Annot", "Subtype"=>"Widget", "FT"=>"Tx", "T"=>Object::string_literal("customer"), "V"=>Object::string_literal("Preserve me"), "Rect"=>vec![50.into(),600.into(),230.into(),624.into()], "AP"=>dictionary!{"N"=>appearance}, "F"=>4});
    let mut kids = Vec::new();
    for index in 0..2 {
        let mut content = format!("BT /F1 18 Tf 50 700 Td (Compression reference page {}) Tj ET\nq 144 0 0 144 50 400 cm /Im1 Do Q\n", index + 1).into_bytes();
        // Highly redundant vector commands exercise stream compression.
        content.extend_from_slice(
            b"q 0.2 0.4 0.8 rg 260 400 80 80 re f Q\n"
                .repeat(250)
                .as_slice(),
        );
        let stream = doc.add_object(Stream::new(dictionary! {}, content));
        let page = doc.add_object(dictionary! {"Type"=>"Page", "Parent"=>pages, "MediaBox"=>vec![0.into(),0.into(),612.into(),792.into()], "CropBox"=>vec![0.into(),0.into(),612.into(),792.into()], "Resources"=>dictionary!{"Font"=>dictionary!{"F1"=>font}, "XObject"=>dictionary!{"Im1"=>image}}, "Contents"=>stream, "Annots"=> if index == 0 {vec![link.into(),field.into()]} else {vec![link.into()]}});
        if index == 0 {
            doc.get_object_mut(field)
                .unwrap()
                .as_dict_mut()
                .unwrap()
                .set("P", page);
        }
        kids.push(page.into());
    }
    doc.objects.insert(
        pages,
        dictionary! {"Type"=>"Pages", "Kids"=>kids, "Count"=>2}.into(),
    );
    let root = doc.add_object(dictionary!{"Type"=>"Catalog", "Pages"=>pages, "AcroForm"=>dictionary!{"Fields"=>vec![field.into()], "DA"=>Object::string_literal("/F1 12 Tf 0 g"), "DR"=>dictionary!{"Font"=>dictionary!{"F1"=>font}}}});
    let info = doc.add_object(dictionary!{"Title"=>Object::string_literal("Compression regression"), "Author"=>Object::string_literal("Rust test suite")});
    doc.trailer.set("Root", root);
    doc.trailer.set("Info", info);
    // An unreachable large object verifies graph cleanup independently of rendering.
    doc.add_object(Object::string_literal("unused".repeat(1000)));
    save(doc)
}

/// Several independently encoded image and PDF structure cases for external viewers.
pub fn corpus() -> Vec<(&'static str, Vec<u8>)> {
    let base = fixture();
    let source = Document::load_mem(&base).unwrap();
    let image_id = *source
        .objects
        .iter()
        .find(|(_, o)| {
            o.as_stream().ok().is_some_and(|s| {
                s.dict.get(b"Subtype").and_then(Object::as_name).ok() == Some(b"Image")
            })
        })
        .unwrap()
        .0;
    let mut gray = source.clone();
    let stream = gray
        .get_object_mut(image_id)
        .unwrap()
        .as_stream_mut()
        .unwrap();
    let pixels = stream
        .content
        .chunks_exact(3)
        .map(|rgb| ((rgb[0] as u32 + rgb[1] as u32 + rgb[2] as u32) / 3) as u8)
        .collect();
    stream.set_content(pixels);
    stream.dict.set("ColorSpace", "DeviceGray");
    let mut jpeg = source.clone();
    let stream = jpeg
        .get_object_mut(image_id)
        .unwrap()
        .as_stream_mut()
        .unwrap();
    let mut encoded = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut encoded, 95)
        .encode(&stream.content, 600, 600, image::ExtendedColorType::Rgb8)
        .unwrap();
    stream.set_content(encoded);
    stream.dict.set("Filter", "DCTDecode");
    let mut masked = source.clone();
    let mask=masked.add_object(Stream::new(dictionary!{"Type"=>"XObject","Subtype"=>"Image","Width"=>600,"Height"=>600,"ColorSpace"=>"DeviceGray","BitsPerComponent"=>8},(0..600).flat_map(|y|std::iter::repeat_n((y*255/599) as u8,600)).collect()));
    masked
        .get_object_mut(image_id)
        .unwrap()
        .as_stream_mut()
        .unwrap()
        .dict
        .set("SMask", mask);
    let mut modern = source.clone();
    let mut modern_bytes = Vec::new();
    modern.save_modern(&mut modern_bytes).unwrap();
    vec![
        ("rgb", base),
        ("gray", save(gray)),
        ("jpeg", save(jpeg)),
        ("soft-mask", save(masked)),
        ("object-streams", modern_bytes),
        ("texture", textured_fixture()),
    ]
}

/// A deterministic textured gradient whose entropy exercises JPEG rather than
/// allowing the lossless predictor to win every image-encoding decision.
pub fn textured_fixture() -> Vec<u8> {
    let mut d = Document::load_mem(&fixture()).unwrap();
    let mut seed = 0x1842_7781_u32;
    for object in d.objects.values_mut() {
        let Ok(s) = object.as_stream_mut() else {
            continue;
        };
        if s.dict.get(b"Subtype").and_then(Object::as_name).ok() != Some(b"Image") {
            continue;
        }
        for byte in &mut s.content {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            *byte = (*byte as i16 + (seed % 25) as i16 - 12).clamp(0, 255) as u8;
        }
    }
    save(d)
}

#[derive(Debug)]
pub struct Raster {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<u8>,
}

/// Read Poppler's binary RGB PPM, including legal header comments.
pub fn read_ppm(bytes: &[u8]) -> Result<Raster, String> {
    fn token<'a>(b: &'a [u8], p: &mut usize) -> Result<&'a [u8], String> {
        loop {
            while *p < b.len() && b[*p].is_ascii_whitespace() {
                *p += 1;
            }
            if b.get(*p) == Some(&b'#') {
                while *p < b.len() && b[*p] != b'\n' {
                    *p += 1;
                }
            } else {
                break;
            }
        }
        let start = *p;
        while *p < b.len() && !b[*p].is_ascii_whitespace() {
            *p += 1;
        }
        if start == *p {
            return Err("truncated PPM header".into());
        }
        Ok(&b[start..*p])
    }
    let mut p = 0;
    if token(bytes, &mut p)? != b"P6" {
        return Err("expected RGB P6 PPM".into());
    }
    let w: usize = std::str::from_utf8(token(bytes, &mut p)?)
        .map_err(|e| e.to_string())?
        .parse()
        .map_err(|_| "invalid width")?;
    let h: usize = std::str::from_utf8(token(bytes, &mut p)?)
        .map_err(|e| e.to_string())?
        .parse()
        .map_err(|_| "invalid height")?;
    if token(bytes, &mut p)? != b"255" {
        return Err("expected 8-bit PPM".into());
    }
    if !bytes.get(p).is_some_and(u8::is_ascii_whitespace) {
        return Err("missing PPM separator".into());
    }
    if bytes[p] == b'\r' && bytes.get(p + 1) == Some(&b'\n') {
        p += 2;
    } else {
        p += 1;
    }
    let len = w
        .checked_mul(h)
        .and_then(|x| x.checked_mul(3))
        .ok_or("PPM size overflow")?;
    if w == 0 || h == 0 || bytes.len() - p != len {
        return Err("PPM pixel length mismatch".into());
    }
    Ok(Raster {
        width: w,
        height: h,
        pixels: bytes[p..].to_vec(),
    })
}

#[derive(Debug)]
pub struct Similarity {
    pub mae_similarity: f64,
    pub ssim: f64,
    pub max_channel_error: u8,
}

/// RGB MAE plus mean 8x8-window luminance SSIM (population moments).
/// This explicit metric is reproducible; it is not a claim of perceptual equivalence.
pub fn compare(a: &Raster, b: &Raster) -> Result<Similarity, String> {
    if a.width != b.width
        || a.height != b.height
        || a.pixels.len() != b.pixels.len()
        || a.pixels.is_empty()
    {
        return Err("raster dimensions differ or empty".into());
    }
    let total: u64 = a
        .pixels
        .iter()
        .zip(&b.pixels)
        .map(|(x, y)| x.abs_diff(*y) as u64)
        .sum();
    let max_channel_error = a
        .pixels
        .iter()
        .zip(&b.pixels)
        .map(|(x, y)| x.abs_diff(*y))
        .max()
        .unwrap();
    let mut sum = 0.0;
    let mut windows = 0;
    for y in (0..a.height).step_by(8) {
        for x in (0..a.width).step_by(8) {
            let (mut sa, mut sb, mut saa, mut sbb, mut sab, mut n) = (0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
            for yy in y..(y + 8).min(a.height) {
                for xx in x..(x + 8).min(a.width) {
                    let i = (yy * a.width + xx) * 3;
                    let luma = |p: &[u8]| {
                        0.2126 * p[i] as f64 + 0.7152 * p[i + 1] as f64 + 0.0722 * p[i + 2] as f64
                    };
                    let (u, v) = (luma(&a.pixels), luma(&b.pixels));
                    sa += u;
                    sb += v;
                    saa += u * u;
                    sbb += v * v;
                    sab += u * v;
                    n += 1.0;
                }
            }
            let (ma, mb) = (sa / n, sb / n);
            let (va, vb, cov) = (
                (saa / n - ma * ma).max(0.0),
                (sbb / n - mb * mb).max(0.0),
                sab / n - ma * mb,
            );
            sum += ((2.0 * ma * mb + 6.5025) * (2.0 * cov + 58.5225))
                / ((ma * ma + mb * mb + 6.5025) * (va + vb + 58.5225));
            windows += 1;
        }
    }
    Ok(Similarity {
        mae_similarity: 1.0 - total as f64 / (a.pixels.len() as f64 * 255.0),
        ssim: sum / windows as f64,
        max_channel_error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ppm_preserves_whitespace_valued_first_pixel() {
        let p = read_ppm(b"P6\n# test\n1 1\n255\n\n\r ").unwrap();
        assert_eq!(p.pixels, b"\n\r ");
    }
    #[test]
    fn ppm_rejects_truncation() {
        assert!(read_ppm(b"P6\n100 100\n255\n").is_err());
    }
    #[test]
    fn metrics_detect_changes() {
        let a = Raster {
            width: 8,
            height: 8,
            pixels: vec![0; 192],
        };
        let b = Raster {
            width: 8,
            height: 8,
            pixels: vec![255; 192],
        };
        assert_eq!(compare(&a, &a).unwrap().ssim, 1.0);
        assert_eq!(compare(&a, &b).unwrap().mae_similarity, 0.0);
        assert!(compare(&a, &b).unwrap().ssim < 0.001);
    }
}
