#!/usr/bin/env python3
"""Compare every page in flattened corpus outputs using macOS Quartz (test only)."""
import argparse
from concurrent.futures import ThreadPoolExecutor
import hashlib
import json
import platform
from pathlib import Path
import shutil
import subprocess
import sys
import time

import numpy as np
from PIL import Image


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def compare(a, b):
    with Image.open(a) as source, Image.open(b) as output:
        if source.size != output.size:
            return {"pass": False, "error": "dimensions changed"}
        # Row chunks bound temporary arrays even on large pages.
        absolute_error = 0.0
        ssim_sum = 0.0
        windows = 0
        different = 0
        width, height = source.size
        for y in range(0, height, 64):
            box = (0, y, width, min(y + 64, height))
            x = np.asarray(source.crop(box).convert("RGB"), dtype=np.float64)
            z = np.asarray(output.crop(box).convert("RGB"), dtype=np.float64)
            delta = np.abs(x-z)
            different += int(np.count_nonzero(np.any(delta != 0, axis=2)))
            absolute_error += float(delta.sum())
            if np.any(delta):
                lx = x @ np.array([0.2126, 0.7152, 0.0722])
                lz = z @ np.array([0.2126, 0.7152, 0.0722])
                for yy in range(0, lx.shape[0], 8):
                    for xx in range(0, width, 8):
                        p = lx[yy:yy+8, xx:xx+8]
                        q = lz[yy:yy+8, xx:xx+8]
                        pm, qm = p.mean(), q.mean()
                        pv, qv = p.var(), q.var()
                        covariance = ((p-pm)*(q-qm)).mean()
                        ssim_sum += ((2*pm*qm+6.5025)*(2*covariance+58.5225))/((pm*pm+qm*qm+6.5025)*(pv+qv+58.5225))
                        windows += 1
            else:
                count = ((x.shape[0]+7)//8)*((width+7)//8)
                ssim_sum += count
                windows += count
        return {"dimensions": [width,height], "different_pixels": different,
                "exact": different == 0, "mae_similarity": 1-absolute_error/(width*height*3*255),
                "ssim": ssim_sum/windows}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, required=True)
    parser.add_argument("--input-dir", type=Path, required=True)
    parser.add_argument("--compressed-dir", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--renderer", type=Path, default=Path("target/quartz/render_quartz"))
    parser.add_argument("--preset", choices=["lossless", "prepress"], default="lossless")
    parser.add_argument("--dpi", type=int, default=150)
    parser.add_argument("--timeout", type=int, default=90)
    parser.add_argument("--workers", type=int, default=2)
    args = parser.parse_args()
    args.output_dir.mkdir(parents=True, exist_ok=False)
    renderer = args.output_dir.resolve()/"render_quartz"
    shutil.copy2(args.renderer, renderer)
    shutil.copy2(args.manifest, args.output_dir/"manifest.json")
    manifest = json.loads(args.manifest.read_text())
    metadata = {"system": platform.platform(), "renderer_sha256": digest(renderer),
                "manifest_sha256": digest(args.manifest), "dpi": args.dpi,
                "preset": args.preset, "verifier_sha256": digest(Path(__file__)), "started_unix": time.time()}
    shutil.copy2(Path(__file__), args.output_dir/"verify_quartz.py")
    (args.output_dir/"run.json").write_text(json.dumps(metadata, indent=2))

    def check(entry):
        source = args.input_dir/entry["local_path"]
        output = args.compressed_dir/source.name
        result = {"file": entry["local_path"], "evidence_class": entry["evidence_class"]}
        if not source.exists() or not output.exists():
            return dict(result, status="not_compared", reason="input or compressed output absent")
        result.update(input_sha256=digest(source), output_sha256=digest(output))
        if result["input_sha256"] != entry["sha256"]:
            return dict(result, status="failed", reason="input hash mismatch")
        folder = args.output_dir/source.stem
        folder.mkdir()
        try:
            for label, path in [("input", source), ("output", output)]:
                run = subprocess.run([str(renderer), str(path.resolve()), str((folder/label).resolve()), str(args.dpi)],
                                     capture_output=True, timeout=args.timeout)
                (folder/(label+".log")).write_bytes(run.stdout+run.stderr)
                if run.returncode:
                    return dict(result, status=label+"_render_failed", code=run.returncode)
            originals = sorted((folder/"input").glob("page-*.png"))
            outputs = sorted((folder/"output").glob("page-*.png"))
            if not originals or len(originals) != len(outputs):
                return dict(result, status="failed", reason="page count changed or no pages")
            pages = [dict(compare(a,b), page=i+1) for i,(a,b) in enumerate(zip(originals,outputs))]
            passed = all(p.get("exact",False) if args.preset == "lossless" else p.get("mae_similarity",0)>=.99 and p.get("ssim",0)>=.99 for p in pages)
            result.update(status="passed" if passed else "failed", pages=pages)
            if passed:
                shutil.rmtree(folder/"input")
                shutil.rmtree(folder/"output")
            return result
        except subprocess.TimeoutExpired:
            return dict(result,status="timeout")
        except Exception as error:
            return dict(result,status="failed",reason=str(error))

    with (args.output_dir/"results.jsonl").open("w") as stream:
        with ThreadPoolExecutor(max_workers=args.workers) as pool:
            rows=[]
            for result in pool.map(check,manifest["entries"]):
                stream.write(json.dumps(result)+"\n");stream.flush()
                rows.append(result)
                print(result["file"], result["status"],flush=True)
    from collections import Counter
    summary={"outcomes":dict(Counter(r["status"] for r in rows)),
             "compared_pages":sum(len(r.get("pages",[])) for r in rows),
             "elapsed_seconds":time.time()-metadata["started_unix"]}
    (args.output_dir/"summary.json").write_text(json.dumps(summary,indent=2))
    print(json.dumps(summary,indent=2))
    return 0 if all(row["status"] == "passed" for row in rows) else 1


if __name__ == "__main__":
    sys.exit(main())
