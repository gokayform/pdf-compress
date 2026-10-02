#!/usr/bin/env python3
"""Verify the six JBIG2 repairs against independent PDFium-decoded samples.

The normal corpus run retains every original-render disagreement. This extra
proof creates a reference PDF by replacing only the original image encoding
with independently decoded one-bit samples, then checks actual compressor
outputs against that reference in three independent page renderers.
"""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import sys

from pypdf import PdfReader, PdfWriter
from pypdf.generic import DecodedStreamObject
from verify_corpus import render_pdf, compare_rendered_pages, structural_checks, tool_provenance
from verify_quartz import compare as compare_quartz

CASES = {"025", "029", "115", "116", "117", "120"}


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def image_object(reader):
    if len(reader.pages) != 1:
        raise ValueError("focused fixture must have one page")
    resources = reader.pages[0]["/Resources"]["/XObject"]
    images = [(key, value.get_object()) for key, value in resources.items()
              if value.get_object().get("/Subtype") == "/Image"]
    if len(images) != 1:
        raise ValueError("focused fixture must have exactly one image")
    return resources, images[0][0], images[0][1]


def attributes(image):
    return {str(key): str(value) for key, value in image.items()
            if str(key) not in {"/Filter", "/DecodeParms", "/Length", "/DL"}}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--results", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--pdfjs-module", type=Path, required=True)
    parser.add_argument("--wasm-dir", type=Path, required=True)
    parser.add_argument("--quartz", type=Path, default=Path("target/quartz/render_quartz"))
    args = parser.parse_args()
    args.output_dir.mkdir(parents=True, exist_ok=False)
    helper = Path(__file__).with_name("decode_pdfium_reference.mjs")
    shutil.copy2(helper, args.output_dir / helper.name)
    shutil.copy2(__file__, args.output_dir / Path(__file__).name)
    tools = {key: shutil.which(key) for key in ("qpdf", "gs", "pdftoppm", "mutool", "pdftotext", "pdfinfo")}
    provenance = tool_provenance(tools, 10)
    tools["_versions"] = provenance["versions"]
    rows = [json.loads(line) for line in args.results.read_text().splitlines()]
    selected = [row for row in rows if Path(row["input"]["manifest_path"]).name[:3] in CASES]
    if len(selected) != 6:
        raise ValueError("all six original fixtures are required")
    outcomes = []
    for row in selected:
        source = Path(row["input"]["path"])
        output = Path(row["output"]["path"])
        folder = args.output_dir / source.stem
        folder.mkdir()
        samples_path = folder / "pdfium.samples"
        metadata_path = folder / "pdfium.json"
        command = ["node", str(helper), str(args.pdfjs_module), str(args.wasm_dir),
                   str(source), str(samples_path), str(metadata_path)]
        decoded = subprocess.run(command, capture_output=True, text=True, timeout=60)
        (folder / "decoder.log").write_text(decoded.stdout + decoded.stderr)
        decoded.check_returncode()
        metadata = json.loads(metadata_path.read_text())
        samples = samples_path.read_bytes()
        source_reader, output_reader = PdfReader(source), PdfReader(output)
        _, _, source_image = image_object(source_reader)
        _, _, output_image = image_object(output_reader)
        if source_image["/Filter"] != "/JBIG2Decode" or output_image["/Filter"] != "/FlateDecode":
            raise ValueError("expected JBIG2 to Flate conversion")
        if output_image.get_data() != samples or attributes(source_image) != attributes(output_image):
            raise ValueError(f"sample bytes or image semantics changed: {source}")
        writer = PdfWriter(clone_from=source_reader)
        resources, key, image = image_object(writer)
        reference_image = DecodedStreamObject()
        reference_image.update({key: value for key, value in image.items()
                                if key not in {"/Filter", "/DecodeParms", "/Length", "/DL"}})
        reference_image.set_data(samples)
        resources[key] = writer._add_object(reference_image)
        reference = folder / "independent-reference.pdf"
        writer.write(reference)
        result = {"file": row["input"]["manifest_path"], "input_sha256": digest(source),
                  "output_sha256": digest(output), "reference_sha256": digest(reference),
                  "samples_equal": True, "image_attributes_equal": True, "decoder": metadata,
                  "reference_checks": structural_checks(reference, tools, 30),
                  "output_checks": structural_checks(output, tools, 30), "renderers": {}}
        for engine in ("poppler", "mupdf"):
            original = render_pdf(reference, engine, "reference", tools, folder, 30, 150, 50, 1)
            rewritten = render_pdf(output, engine, "output", tools, folder, 30, 150, 50, 1)
            result["renderers"][engine] = compare_rendered_pages(
                engine, original, rewritten, folder, "lossless")
            result["renderers"][engine]["reference_run"] = original.get("step")
            result["renderers"][engine]["output_run"] = rewritten.get("step")
        for label, path in (("reference", reference), ("output", output)):
            quartz = subprocess.run([str(args.quartz.resolve()), str(path.resolve()),
                                     str((folder / f"quartz-{label}").resolve()), "150"],
                                    capture_output=True, text=True, timeout=30)
            (folder / f"quartz-{label}.log").write_text(quartz.stdout + quartz.stderr)
            quartz.check_returncode()
        reference_pages = sorted((folder / "quartz-reference").glob("page-*.png"))
        output_pages = sorted((folder / "quartz-output").glob("page-*.png"))
        quartz_pages = [compare_quartz(a, b) for a, b in zip(reference_pages, output_pages)]
        result["renderers"]["quartz"] = {
            "passed": len(reference_pages) == len(output_pages) == 1
                      and all(page.get("exact") for page in quartz_pages), "pages": quartz_pages}
        result["passed"] = (result["reference_checks"]["valid"] and result["output_checks"]["valid"]
                            and all(check["passed"] for check in result["renderers"].values()))
        outcomes.append(result)
        (folder / "result.json").write_text(json.dumps(result, indent=2) + "\n")
        print(source.name, result["passed"], flush=True)
    proof = {"passed": all(row["passed"] for row in outcomes), "cases": outcomes,
             "provenance": provenance, "corpus_results_sha256": digest(args.results),
             "verifier_sha256": digest(__file__), "quartz_sha256": digest(args.quartz),
             "oracle_policy": "Independent PDFium packed samples; exact reference/output pixels in Poppler, MuPDF, Quartz. Original-render failures retained separately."}
    (args.output_dir / "proof.json").write_text(json.dumps(proof, indent=2) + "\n")
    return 0 if proof["passed"] else 1


if __name__ == "__main__":
    sys.exit(main())
