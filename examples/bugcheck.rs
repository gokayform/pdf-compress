use lopdf::{dictionary, Document, Stream};
use pdf_compress::{compress, Options, Preset};

fn save(mut document: Document) -> Vec<u8> {
    let mut bytes = Vec::new();
    document.save_to(&mut bytes).unwrap();
    bytes
}

fn textured_rgb(w: u32, h: u32) -> Vec<u8> {
    let mut samples = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        for x in 0..w {
            samples.push(((x * 37 + y * 17) % 256) as u8);
            samples.push(((x * 13 + y * 29) % 256) as u8);
            samples.push(((x * 7 + y * 41) % 256) as u8);
        }
    }
    samples
}

fn main() {
    // Rotation: page CTM then non-uniform form Matrix should produce different
    // displayed width/height depending on multiply order.
    // Content: cm rotates 90°, then Do form whose Matrix scales X*2 Y*3.
    // Spec: final linear = Form × CTM = [2 0 0 3] × [0 1 -1 0] = [0 2 -3 0]
    //   displayed w=hypot(0,2)=2, h=hypot(-3,0)=3  → aspect 2:3 in user space
    // Bug (CTM×Form): [0 1 -1 0] × [2 0 0 3] = [0 3 -2 0]
    //   displayed w=3, h=2
    // Image is 300×200. Ebook target 150 DPI.
    // If order wrong, max placement dims flip and downsample target changes.
    let mut document = Document::with_version("1.7");
    let pages = document.new_object_id();
    let samples = textured_rgb(300, 200);
    let image = document.add_object(Stream::new(
        dictionary! {
            "Type" => "XObject",
            "Subtype" => "Image",
            "Width" => 300,
            "Height" => 200,
            "ColorSpace" => "DeviceRGB",
            "BitsPerComponent" => 8,
        },
        samples,
    ));
    let form = document.add_object(Stream::new(
        dictionary! {
            "Type" => "XObject",
            "Subtype" => "Form",
            "BBox" => vec![0.into(), 0.into(), 1.into(), 1.into()],
            "Matrix" => vec![2.into(), 0.into(), 0.into(), 3.into(), 0.into(), 0.into()],
            "Resources" => dictionary! { "XObject" => dictionary! { "Im1" => image } },
        },
        b"q /Im1 Do Q".to_vec(),
    ));
    // 90° rotation then paint form: [0 1 -1 0 100 100] cm /Fm1 Do
    let contents = document.add_object(Stream::new(
        dictionary! {},
        b"q 0 1 -1 0 100 100 cm /Fm1 Do Q".to_vec(),
    ));
    let page = document.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages,
        "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        "Contents" => contents,
        "Resources" => dictionary! { "XObject" => dictionary! { "Fm1" => form } },
    });
    document.objects.insert(
        pages,
        dictionary! {"Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1}.into(),
    );
    let root = document.add_object(dictionary! {"Type" => "Catalog", "Pages" => pages});
    document.trailer.set("Root", root);
    let input = save(document);
    let result = compress(&input, &Options::for_preset(Preset::Ebook)).unwrap();
    let out = Document::load_mem(&result.bytes).unwrap();
    // Find the image
    for (id, obj) in out.objects.iter() {
        if let Ok(stream) = obj.as_stream() {
            if stream
                .dict
                .get(b"Subtype")
                .ok()
                .and_then(|v| v.as_name().ok())
                == Some(b"Image")
            {
                let w = stream.dict.get(b"Width").unwrap().as_i64().unwrap();
                let h = stream.dict.get(b"Height").unwrap().as_i64().unwrap();
                println!(
                    "image {id:?} => {w}x{h}; optimized={}",
                    result.report.images_optimized
                );
                println!("warnings: {:?}", result.report.warnings);
            }
        }
    }

    // Spec-correct placement size in points: w=2, h=3 (from Form×CTM)
    // at 150 DPI: ceil(2*150/72)=5, ceil(3*150/72)=7 → 5×7 if downsampled
    // Bug order placement: w=3, h=2 → ceil(3*150/72)=7, ceil(2*150/72)=5 → 7×5
    // But also clamped to source 300×200, and JPEG only if smaller...
    // Actually with tiny placement (2x3 points), both downsample heavily.
    // Let's also print whether Filter became DCTDecode.
}
