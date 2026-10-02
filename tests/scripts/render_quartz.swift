// Quartz/CoreGraphics reference renderer for the independent PDF test suite.
//
// This file is intentionally test-only. It is compiled and invoked by the
// host test runner; the compressor itself remains pure Rust and has no native
// rendering dependency.
//
// Usage:
//   render_quartz <input.pdf> <output-directory> <dpi>
//
// Each page is written as page-0001.png plus a matching JSON sidecar. The PNG
// is opaque RGB content (stored in an RGBA bitmap with a white background),
// and the sidecar records the crop box, rotation, pixel dimensions, and
// renderer details needed to compare it with another engine's raster.

import CoreGraphics
import Darwin
import Foundation
import ImageIO

let usage = "usage: render_quartz <input.pdf> <output-directory> <dpi>"
let arguments = CommandLine.arguments

func fail(_ message: String) -> Never {
    let text = "render_quartz: \(message)\n\(usage)\n"
    FileHandle.standardError.write(Data(text.utf8))
    exit(2)
}

guard arguments.count == 4 else {
    fail("expected an input PDF, an output directory, and a positive DPI")
}

let inputURL = URL(fileURLWithPath: arguments[1], isDirectory: false)
let outputURL = URL(fileURLWithPath: arguments[2], isDirectory: true)
guard FileManager.default.fileExists(atPath: inputURL.path) else {
    fail("input does not exist: \(inputURL.path)")
}

guard let dpi = Double(arguments[3]), dpi.isFinite, dpi > 0 else {
    fail("DPI must be a finite number greater than zero")
}

do {
    try FileManager.default.createDirectory(
        at: outputURL,
        withIntermediateDirectories: true,
        attributes: nil
    )
} catch {
    fail("could not create output directory \(outputURL.path): \(error)")
}

guard let document = CGPDFDocument(inputURL as CFURL) else {
    fail("could not open PDF with Quartz: \(inputURL.path)")
}

let pageCount = document.numberOfPages
guard pageCount > 0 else {
    fail("PDF has no pages")
}

let rgbColorSpace = CGColorSpaceCreateDeviceRGB()
// Keep the test renderer bounded: 50M pixels is about 200 MiB of RGBA
// backing storage before CoreGraphics' own temporary allocations.
let maxPixels = 50_000_000

func finite(_ value: CGFloat) -> Bool {
    value.isFinite
}

func normalizedRotation(_ value: Int) -> Int {
    let remainder = value % 360
    return remainder >= 0 ? remainder : remainder + 360
}

func jsonNumber(_ value: CGFloat) -> NSNumber {
    NSNumber(value: Double(value))
}

for pageNumber in 1...pageCount {
    guard let page = document.page(at: pageNumber) else {
        fail("could not read page \(pageNumber)")
    }

    let cropBox = page.getBoxRect(.cropBox)
    let mediaBox = page.getBoxRect(.mediaBox)
    let useCropBox = cropBox.width > 0 && cropBox.height > 0
    let displayBox: CGPDFBox = useCropBox ? .cropBox : .mediaBox
    let selectedBox = useCropBox ? cropBox : mediaBox
    guard finite(selectedBox.origin.x), finite(selectedBox.origin.y),
          finite(selectedBox.width), finite(selectedBox.height),
          selectedBox.width > 0, selectedBox.height > 0 else {
        fail("page \(pageNumber) has an invalid crop/media box")
    }

    let rotation = normalizedRotation(Int(page.rotationAngle))
    let scale = dpi / 72.0
    let baseWidth = Double(abs(selectedBox.width)) * scale
    let baseHeight = Double(abs(selectedBox.height)) * scale
    let rotated = rotation == 90 || rotation == 270
    let renderedWidth = rotated ? baseHeight : baseWidth
    let renderedHeight = rotated ? baseWidth : baseHeight
    guard renderedWidth.isFinite, renderedHeight.isFinite,
          renderedWidth > 0, renderedHeight > 0,
          renderedWidth <= Double(Int.max), renderedHeight <= Double(Int.max) else {
        fail("page \(pageNumber) is too large to rasterize")
    }

    let width = Int(ceil(renderedWidth))
    let height = Int(ceil(renderedHeight))
    guard width > 0, height > 0,
          width <= maxPixels, height <= maxPixels,
          width <= maxPixels / max(1, height) else {
        fail("page \(pageNumber) exceeds the \(maxPixels)-pixel renderer limit")
    }

    let bytesPerRow = width.multipliedReportingOverflow(by: 4)
    guard !bytesPerRow.overflow else {
        fail("page \(pageNumber) has an overflowing row size")
    }
    let byteCount = bytesPerRow.partialValue.multipliedReportingOverflow(by: height)
    guard !byteCount.overflow else {
        fail("page \(pageNumber) has an overflowing bitmap size")
    }

    var pixels = [UInt8](repeating: 255, count: byteCount.partialValue)
    let bitmapInfo = CGImageAlphaInfo.premultipliedLast.rawValue
        | CGBitmapInfo.byteOrder32Big.rawValue
    guard let context = CGContext(
        data: &pixels,
        width: width,
        height: height,
        bitsPerComponent: 8,
        bytesPerRow: bytesPerRow.partialValue,
        space: rgbColorSpace,
        bitmapInfo: bitmapInfo
    ) else {
        fail("could not create RGB bitmap for page \(pageNumber)")
    }

    // Fill the page with opaque white before drawing. This gives all engines
    // the same background for transparent pages and makes RGB comparisons
    // independent of an implementation's default alpha compositing.
    context.setFillColor(CGColor(red: 1, green: 1, blue: 1, alpha: 1))
    context.fill(CGRect(x: 0, y: 0, width: width, height: height))
    context.interpolationQuality = .high
    context.setShouldAntialias(true)
    context.setAllowsAntialiasing(true)

    // CGPDFPage's drawing transform maps the selected box and /Rotate into the
    // exact pixel rectangle. CoreGraphics' CGImage export preserves the PDF
    // page's visual orientation for PNG consumers, so no extra y-flip is
    // applied here (doing so would invert the page a second time).
    let target = CGRect(x: 0, y: 0, width: width, height: height)
    let transform = page.getDrawingTransform(
        displayBox,
        rect: target,
        rotate: Int32(rotation),
        preserveAspectRatio: true
    )
    context.concatenate(transform)
    context.drawPDFPage(page)

    guard let image = context.makeImage() else {
        fail("could not create PNG image for page \(pageNumber)")
    }

    let stem = String(format: "page-%04d", pageNumber)
    let imageURL = outputURL.appendingPathComponent(stem).appendingPathExtension("png")
    guard let destination = CGImageDestinationCreateWithURL(
        imageURL as CFURL,
        "public.png" as CFString,
        1,
        nil
    ) else {
        fail("could not create PNG destination for page \(pageNumber)")
    }
    CGImageDestinationAddImage(destination, image, nil)
    guard CGImageDestinationFinalize(destination) else {
        fail("could not write PNG for page \(pageNumber)")
    }

    let metadata: [String: Any] = [
        "engine": "Quartz/CoreGraphics",
        "input": inputURL.path,
        "page": pageNumber,
        "pageCount": pageCount,
        "displayBox": useCropBox ? "cropBox" : "mediaBox",
        "cropBox": [
            "x": jsonNumber(selectedBox.origin.x),
            "y": jsonNumber(selectedBox.origin.y),
            "width": jsonNumber(selectedBox.width),
            "height": jsonNumber(selectedBox.height),
        ],
        "rotation": rotation,
        "dpi": dpi,
        "width": width,
        "height": height,
        "pixelFormat": "RGBA8 premultiplied, opaque white background",
        "colorSpace": "DeviceRGB",
    ]
    let metadataURL = outputURL.appendingPathComponent(stem).appendingPathExtension("json")
    do {
        let metadataData = try JSONSerialization.data(
            withJSONObject: metadata,
            options: [.prettyPrinted, .sortedKeys]
        )
        try metadataData.write(to: metadataURL, options: .atomic)
    } catch {
        fail("could not write metadata for page \(pageNumber): \(error)")
    }
}
