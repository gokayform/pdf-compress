//! Explicit external-oracle runner; never linked into the compressor library.
use super::{compare, corpus, read_ppm};
use pdf_compress::{compress, Options, Preset};
use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

fn invoke(command: &mut Command) -> Result<(), String> {
    let shown = format!("{command:?}");
    let output = command.output().map_err(|e| format!("{shown}: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "{shown}: {}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(())
}

fn render(tool: &OsString, input: &Path, prefix: &Path) -> Result<Vec<PathBuf>, String> {
    invoke(
        Command::new(tool)
            .arg("-r")
            .arg("96")
            .arg(input)
            .arg(prefix),
    )?;
    let parent = prefix.parent().unwrap();
    let name = format!("{}-", prefix.file_name().unwrap().to_string_lossy());
    let mut pages = fs::read_dir(parent)
        .map_err(|e| e.to_string())?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.extension().is_some_and(|x| x == "ppm")
                && p.file_name().unwrap().to_string_lossy().starts_with(&name)
        })
        .collect::<Vec<_>>();
    // Numeric ordering matters for documents longer than nine pages.
    pages.sort_by_key(|p| {
        p.file_stem()
            .unwrap()
            .to_string_lossy()
            .rsplit('-')
            .next()
            .unwrap()
            .parse::<u32>()
            .unwrap_or(0)
    });
    if pages.is_empty() {
        return Err(format!("no rendered pages for {}", input.display()));
    }
    Ok(pages)
}

pub fn run(mut args: impl Iterator<Item = OsString>) -> Result<(), String> {
    let mut output = PathBuf::from("target/pdf-compress-validation");
    let mut preset = Preset::Lossless;
    let mut preset_name = "lossless".to_string();
    let mut gs = OsString::from("gs");
    let mut poppler = OsString::from("pdftoppm");
    let mut qpdf = OsString::from("qpdf");
    let mut inputs = Vec::new();
    let mut threshold = 0.99_f64;
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--help" | "-h") => {
                println!("Usage: cargo run -p pdf-compress --example pdf-compress-verify -- [--preset lossless|screen|ebook|printer|prepress] [--output NEW_DIRECTORY] [--gs PATH] [--pdftoppm PATH] [--qpdf PATH] [--min-similarity 0.99] [PDF ...]\nNo PDFs: generate six two-page regression fixtures. Requires Ghostscript, Poppler, and qpdf. Lossless requires exact pixels; lossy requires both MAE similarity and local SSIM above the threshold. Output directory must not exist. Writes report.tsv, PDFs and all page rasters. Size ratios are reported, not gated.");
                return Ok(());
            }
            Some("--output") => {
                output = PathBuf::from(args.next().ok_or("missing output directory")?)
            }
            Some("--gs") => gs = args.next().ok_or("missing gs executable")?,
            Some("--qpdf") => qpdf = args.next().ok_or("missing qpdf executable")?,
            Some("--pdftoppm") => poppler = args.next().ok_or("missing pdftoppm executable")?,
            Some("--min-similarity") => {
                threshold = args
                    .next()
                    .ok_or("missing threshold")?
                    .to_string_lossy()
                    .parse()
                    .map_err(|_| "invalid threshold")?
            }
            Some("--preset") => {
                preset_name = args
                    .next()
                    .ok_or("missing preset")?
                    .to_string_lossy()
                    .into_owned();
                preset = match preset_name.as_str() {
                    "lossless" => Preset::Lossless,
                    "screen" => Preset::Screen,
                    "ebook" => Preset::Ebook,
                    "printer" => Preset::Printer,
                    "prepress" => Preset::Prepress,
                    _ => return Err("invalid preset".into()),
                };
            }
            Some(s) if s.starts_with('-') => return Err(format!("unknown option: {s}")),
            _ => inputs.push(PathBuf::from(arg)),
        }
    }
    if !threshold.is_finite() || !(0.0..=1.0).contains(&threshold) {
        return Err("threshold must be 0..1".into());
    }
    let version = Command::new(&gs)
        .arg("--version")
        .output()
        .map_err(|e| format!("Ghostscript is required for this explicit reference run: {e}"))?;
    if !version.status.success() {
        return Err("Ghostscript version check failed".into());
    }
    let gs_version = String::from_utf8_lossy(&version.stdout).trim().to_owned();
    let qpdf_version = Command::new(&qpdf)
        .arg("--version")
        .output()
        .map_err(|e| format!("qpdf is required for this explicit reference run: {e}"))?;
    if !qpdf_version.status.success() {
        return Err("qpdf version check failed".into());
    }
    // Create a new directory so stale rasters cannot falsely satisfy a test.
    if let Some(parent) = output.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    fs::create_dir(&output)
        .map_err(|e| format!("output directory must be new ({}): {e}", output.display()))?;
    if inputs.is_empty() {
        for (name, bytes) in corpus() {
            let path = output.join(format!("fixture-{name}.pdf"));
            fs::write(&path, bytes).map_err(|e| e.to_string())?;
            inputs.push(path);
        }
    }
    let mut report=String::from("input\tpreset\tgs_version\tpages\tinput_bytes\trust_bytes\tgs_bytes\trust_gs_ratio\tmin_mae_similarity\tmin_ssim\tmax_channel_error\tpassed\n");
    let mut failures = Vec::new();
    let reference_preset = if preset_name == "lossless" {
        "/prepress".to_string()
    } else {
        format!("/{preset_name}")
    };
    let mut provenance=format!("Ghostscript version: {gs_version}\nRaster tool: {}\nRaster options: -r 96 (RGB PPM), every page\nRust preset: {preset_name}\nLossy gate: MAE similarity AND mean 8x8 luminance SSIM >= {threshold}\nLossless gate: max channel error = 0\nGS validation arguments: -q -dSAFER -dBATCH -dNOPAUSE -dPDFSTOPONERROR -dPDFSTOPONWARNING -sDEVICE=nullpage\nGS reference arguments: -q -dSAFER -dBATCH -dNOPAUSE -dPDFSTOPONERROR -sDEVICE=pdfwrite -dAutoRotatePages=/None -dPDFSETTINGS={reference_preset}\n",poppler.to_string_lossy());
    provenance.push_str(&format!(
        "qpdf version: {}\nqpdf validation: --check on input and output; warnings are failures\n",
        String::from_utf8_lossy(&qpdf_version.stdout).trim()
    ));
    if preset_name == "lossless" {
        provenance.push_str("Lossless reference overrides: downsampling false for color/gray/mono; AutoFilterColorImages=false; AutoFilterGrayImages=false; ColorImageFilter=/FlateEncode; GrayImageFilter=/FlateEncode. GS font/document reconstruction may still differ.\n");
    }
    fs::write(output.join("reference.txt"), provenance).map_err(|e| e.to_string())?;
    for (index, input) in inputs.iter().enumerate() {
        let case = output.join(format!("case-{index:04}"));
        fs::create_dir(&case).map_err(|e| e.to_string())?;
        let result = (|| -> Result<String, String> {
            let source = fs::read(input).map_err(|e| e.to_string())?;
            let optimized =
                compress(&source, &Options::for_preset(preset)).map_err(|e| e.to_string())?;
            let rust_pdf = case.join("rust.pdf");
            let gs_pdf = case.join("ghostscript.pdf");
            fs::write(&rust_pdf, &optimized.bytes).map_err(|e| e.to_string())?;
            // Warning exit 3 is a failure: a viewer repair is not validation.
            invoke(Command::new(&qpdf).arg("--check").arg(input))?;
            invoke(Command::new(&qpdf).arg("--check").arg(&rust_pdf))?;
            // A successful repair by a tolerant viewer is not sufficient.
            if !optimized.report.used_original {
                super::structure::classic_xref(&optimized.bytes)?;
            }
            invoke(
                Command::new(&gs)
                    .args([
                        "-q",
                        "-dSAFER",
                        "-dBATCH",
                        "-dNOPAUSE",
                        "-dPDFSTOPONERROR",
                        "-dPDFSTOPONWARNING",
                        "-sDEVICE=nullpage",
                        "-f",
                    ])
                    .arg(&rust_pdf),
            )?;
            fs::write(
                case.join("compression-report.txt"),
                format!("{:?}", optimized.report),
            )
            .map_err(|e| e.to_string())?;
            let mut command = Command::new(&gs);
            command
                .args([
                    "-q",
                    "-dSAFER",
                    "-dBATCH",
                    "-dNOPAUSE",
                    "-sDEVICE=pdfwrite",
                    "-dPDFSTOPONERROR",
                    "-dAutoRotatePages=/None",
                ])
                .arg(format!("-dPDFSETTINGS={reference_preset}"));
            if preset_name == "lossless" {
                command.args([
                    "-dDownsampleColorImages=false",
                    "-dDownsampleGrayImages=false",
                    "-dDownsampleMonoImages=false",
                    "-dAutoFilterColorImages=false",
                    "-dAutoFilterGrayImages=false",
                    "-dColorImageFilter=/FlateEncode",
                    "-dGrayImageFilter=/FlateEncode",
                ]);
            }
            let mut output_arg = OsString::from("-sOutputFile=");
            output_arg.push(&gs_pdf);
            command.arg(output_arg).arg("-f").arg(input);
            invoke(&mut command)?;
            let original_pages = render(&poppler, input, &case.join("original"))?;
            let rust_pages = render(&poppler, &rust_pdf, &case.join("rust"))?;
            let gs_pages = render(&poppler, &gs_pdf, &case.join("gs"))?;
            if original_pages.len() != rust_pages.len() || original_pages.len() != gs_pages.len() {
                return Err("page count mismatch".into());
            }
            let (mut min_mae, mut min_ssim, mut max_error) = (1.0_f64, 1.0_f64, 0_u8);
            for (a, b) in original_pages.iter().zip(&rust_pages) {
                let a = read_ppm(&fs::read(a).map_err(|e| e.to_string())?)?;
                let b = read_ppm(&fs::read(b).map_err(|e| e.to_string())?)?;
                let score = compare(&a, &b)?;
                min_mae = min_mae.min(score.mae_similarity);
                min_ssim = min_ssim.min(score.ssim);
                max_error = max_error.max(score.max_channel_error);
            }
            let passed = if preset_name == "lossless" {
                max_error == 0
            } else {
                min_mae >= threshold && min_ssim >= threshold
            };
            if !passed {
                failures.push(format!("{}: visual fidelity gate failed", input.display()));
            }
            let gs_bytes = fs::metadata(gs_pdf).map_err(|e| e.to_string())?.len();
            let name = input.to_string_lossy().replace(['\t', '\n', '\r'], " ");
            Ok(format!("{name}\t{preset_name}\t{gs_version}\t{}\t{}\t{}\t{gs_bytes}\t{:.6}\t{min_mae:.9}\t{min_ssim:.9}\t{max_error}\t{passed}\n",original_pages.len(),source.len(),optimized.bytes.len(),optimized.bytes.len() as f64/gs_bytes as f64))
        })();
        match result {
            Ok(row) => {
                print!("{row}");
                report.push_str(&row);
            }
            Err(e) => failures.push(format!("{}: {e}", input.display())),
        }
    }
    fs::write(output.join("report.tsv"), report).map_err(|e| e.to_string())?;
    fs::write(output.join("failures.txt"), failures.join("\n")).map_err(|e| e.to_string())?;
    println!("Report: {}", output.join("report.tsv").display());
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("\n"))
    }
}
