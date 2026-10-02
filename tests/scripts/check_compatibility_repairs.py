#!/usr/bin/env python3
"""Audit a new full-corpus run against the frozen pre-repair run.

This is a separate repair gate: the normal verifier still reports invalid source
baselines as failures. Rejections are reported separately and never counted as
pixel-preserving repairs. Both runs must contain the same files and hashes.
"""
import argparse
import json
from pathlib import Path


def records(path):
    rows = [json.loads(line) for line in path.read_text().splitlines() if line]
    if not rows:
        raise ValueError("empty proof run")
    result = {row["input"]["manifest_path"]: row for row in rows}
    if len(rows) != len(result):
        raise ValueError("duplicate corpus records")
    return result


def preservation_errors(row, independent_reference=None):
    errors = []
    if not row.get("output_checks", {}).get("valid"):
        errors.append("output did not pass qpdf and strict Ghostscript")
    if not row.get("semantic_comparison", {}).get("passed"):
        errors.append("semantic preservation failed")
    if independent_reference is not None:
        reference = independent_reference
        if not (reference.get("passed") and reference.get("samples_equal")
                and reference.get("image_attributes_equal")
                and reference.get("input_sha256") == row["input"].get("sha256")
                and reference.get("output_sha256") == row.get("output", {}).get("sha256")
                and reference.get("decoder", {}).get("source_sha256") == row["input"].get("sha256")
                and reference.get("reference_checks", {}).get("valid")):
            errors.append("independent image reference failed or hashes do not match")
        for engine in ("poppler", "mupdf", "quartz"):
            check = reference.get("renderers", {}).get(engine, {})
            if not check.get("passed") or not check.get("pages") or not all(
                    page.get("exact") for page in check["pages"]):
                errors.append(f"{engine}: independent reference exact pixels not established")
        return errors
    for engine in ("poppler", "mupdf"):
        comparison = row.get("render_comparisons", {}).get(engine, {})
        pages = comparison.get("pages", [])
        if not comparison.get("passed") or not pages or not all(p.get("exact") for p in pages):
            errors.append(f"{engine}: every-page exact pixels not established")
    return errors


def audit(before, after, expected_rejections, image_references=None):
    errors = []
    outcomes = []
    if set(before) != set(after):
        errors.append("corpus membership changed")
    original_failures = {
        key for key, row in before.items()
        if row.get("output") and not row.get("output_checks", {}).get("valid")
    }
    image_references = image_references or {}
    if not set(image_references).issubset(original_failures):
        errors.append("independent-reference exception includes a previously passing or absent file")
    if not set(expected_rejections).issubset(original_failures):
        errors.append("rejection exception includes a previously passing or absent file")
    for key in sorted(set(before) & set(after)):
        old, new = before[key], after[key]
        if old["input"].get("sha256") != new["input"].get("sha256"):
            errors.append(f"{key}: input hash changed")
        if key in expected_rejections:
            command = new.get("compression", {})
            diagnostic = command.get("stderr", "")
            partial_output = bool(new.get("case_directory")) and (
                Path(new["case_directory"]) / "output.pdf").exists()
            valid = (
                not new.get("output")
                and not partial_output
                and not command.get("timed_out", True)
                and isinstance(command.get("returncode"), int)
                and command["returncode"] > 0
                and expected_rejections[key] in diagnostic
            )
            if not valid:
                errors.append(f"{key}: explicit diagnostic rejection not established")
            outcomes.append({"file": key, "outcome": "rejected" if valid else "failed",
                             "diagnostic": diagnostic})
        elif old.get("output"):
            failures = preservation_errors(new, image_references.get(key))
            errors.extend(f"{key}: {error}" for error in failures)
            if key in original_failures:
                outcomes.append({"file": key, "outcome": "repaired" if not failures else "failed",
                                 "errors": failures, "output": new.get("output"),
                                 "pixel_reference": "independently decoded image" if key in image_references else "original PDF",
                                 "original_comparison_failures": new.get("failures", [])})
        elif new.get("output"):
            errors.extend(f"{key}: {error}" for error in preservation_errors(new))
    return {"passed": not errors, "errors": errors, "original_failure_count": len(original_failures),
            "outcomes": outcomes,
            "repaired": sum(row["outcome"] == "repaired" for row in outcomes),
            "rejected": sum(row["outcome"] == "rejected" for row in outcomes),
            "previously_valid_outputs_preserved": sum(
                bool(row.get("output_checks", {}).get("valid"))
                and key in after and not preservation_errors(after[key])
                for key, row in before.items())}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--before", type=Path, required=True)
    parser.add_argument("--after", type=Path, required=True)
    parser.add_argument("--expected-rejections", type=Path, required=True,
                        help="JSON object mapping exact corpus paths to required diagnostic substrings")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--jbig2-proof", type=Path,
                        help="Additional independently decoded sample proof; raw original-render failures remain recorded")
    args = parser.parse_args()
    references = {}
    if args.jbig2_proof:
        proof = json.loads(args.jbig2_proof.read_text())
        if not proof.get("passed"):
            raise ValueError("independent JBIG2 proof did not pass")
        references = {row["file"]: row for row in proof["cases"]}
    result = audit(records(args.before), records(args.after),
                   json.loads(args.expected_rejections.read_text()), references)
    result.update({"before": str(args.before.resolve()), "after": str(args.after.resolve()),
                   "expected_rejections": json.loads(args.expected_rejections.read_text()),
                   "independent_jbig2_proof": str(args.jbig2_proof.resolve()) if args.jbig2_proof else None})
    args.output.write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result, indent=2))
    return 0 if result["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
