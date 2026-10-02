# Public corpus provenance

`manifest.json` is the reproducibility record for the public PDF corpus used
by the compressor validation work. The PDF bytes are intentionally kept under
`target/pdf-compress-corpus/`, which is ignored by Git. The checked-in
manifest records each selected source URL, pinned origin revision or external
URL hash, local path, byte count, SHA-256, category, source test IDs, and
acquisition status.

Acquire or refresh the corpus with:

```sh
python3 tests/scripts/acquire_corpus.py
```

The script uses only the Python standard library. It discovers PDF.js files
from the pinned Mozilla revision recorded in the manifest, reads PDF.js's
public `test/test_manifest.json` for fixture provenance, and adds the fixed
public-document supplements listed in the manifest. Each response is bounded
to 5 MiB and the run is bounded to 150 MiB; requests have finite timeouts and
bounded retries. A run requires at least 100 non-encrypted positive PDFs.

Positive entries are compatibility evidence selected by source-name hints and
the public PDF.js test descriptions. Negative/stress entries are intentionally
kept in the manifest with `evidence_class: "negative_stress"`; malformed,
encrypted, oversized, or unavailable inputs are recorded rather than silently
replaced after a test result. The source-name classification is best effort and
does not claim statistical representativeness of PDF files in the wild.

The frozen acquisition currently contains 134 downloaded files totaling
13,322,362 bytes: 116 non-encrypted positive PDFs, two encrypted positive
candidates excluded from compatibility evidence, and 16 negative/stress
fixtures. The manifest also retains two failed attempts: the RFC 8259 URL
returned HTTP 404 during acquisition, and `22060_A1_01_Plans.pdf` exceeded the
5 MiB per-file cap. These outcomes are part of the provenance record.

Verify downloaded bytes against the manifest with:

```sh
python3 - <<'PY'
import hashlib, json
from pathlib import Path

manifest = json.loads(Path("tests/corpus/manifest.json").read_text())
root = Path("target/pdf-compress-corpus")
for entry in manifest["entries"]:
    if entry["status"] != "downloaded":
        continue
    data = (root / entry["local_path"]).read_bytes()
    assert len(data) == entry["size"]
    assert hashlib.sha256(data).hexdigest() == entry["sha256"]
print("manifest hashes verified")
PY
```

The corpus is sourced from Mozilla PDF.js, the RFC Editor, PDF Association,
W3C, and arXiv. Their files remain subject to their respective copyright and
license terms; this repository does not relicense or make a redistribution
claim for the downloaded bytes. Reacquire them from the recorded public URLs
when local copies are absent.
