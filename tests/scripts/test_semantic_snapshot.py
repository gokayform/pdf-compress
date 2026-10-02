"""Independent controls for the annotation-placeholder equivalence rule.

Requires the same pypdf dependency as the external corpus verifier.
"""
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

from pypdf import PdfWriter
from pypdf.generic import ArrayObject, DictionaryObject, NameObject, NullObject, NumberObject, TextStringObject
from verify_corpus import PYPDF_SNAPSHOT


class AnnotationSnapshotControls(unittest.TestCase):
    def snapshot(self, contents, placeholder=False, remove=False):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "annotation.pdf"
            writer = PdfWriter()
            page = writer.add_blank_page(100, 100)
            annotation = DictionaryObject({
                NameObject("/Type"): NameObject("/Annot"),
                NameObject("/Subtype"): NameObject("/Text"),
                NameObject("/Rect"): ArrayObject([NumberObject(n) for n in (10, 10, 30, 30)]),
                NameObject("/Contents"): TextStringObject(contents),
            })
            values = [] if remove else [writer._add_object(annotation)]
            if placeholder:
                values.append(NullObject())
            page[NameObject("/Annots")] = ArrayObject(values)
            writer.write(path)
            result = subprocess.run([sys.executable, "-c", PYPDF_SNAPSHOT, str(path)],
                                    capture_output=True, text=True, check=True, timeout=10)
            return json.loads(result.stdout)

    def test_empty_placeholder_is_equivalent(self):
        self.assertEqual(self.snapshot("preserved", True), self.snapshot("preserved"))

    def test_changed_or_removed_annotation_is_not_equivalent(self):
        original = self.snapshot("preserved", True)
        self.assertNotEqual(original, self.snapshot("changed"))
        self.assertNotEqual(original, self.snapshot("preserved", remove=True))


if __name__ == "__main__":
    unittest.main()
