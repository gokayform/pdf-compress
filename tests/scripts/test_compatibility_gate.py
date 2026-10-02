"""Negative controls for the independent compatibility proof gate."""
import copy
import unittest

from check_compatibility_repairs import audit


def passing():
    return {
        "input": {"sha256": "frozen"}, "output": {"sha256": "rewritten"},
        "output_checks": {"valid": True}, "semantic_comparison": {"passed": True},
        "render_comparisons": {engine: {"passed": True, "pages": [{"exact": True}]}
                               for engine in ("poppler", "mupdf")},
    }


class GateControls(unittest.TestCase):
    def test_valid_repair_is_distinct_from_rejection(self):
        old = passing()
        old["output_checks"]["valid"] = False
        result = audit({"a.pdf": old}, {"a.pdf": passing()}, {})
        self.assertTrue(result["passed"])
        self.assertEqual((result["repaired"], result["rejected"]), (1, 0))

    def test_pixel_and_semantic_changes_fail(self):
        for key in ("pixels", "semantics", "validator"):
            changed = passing()
            if key == "pixels":
                changed["render_comparisons"]["mupdf"]["pages"][0]["exact"] = False
            elif key == "semantics":
                changed["semantic_comparison"]["passed"] = False
            else:
                changed["output_checks"]["valid"] = False
            self.assertFalse(audit({"a": passing()}, {"a": changed}, {})["passed"])

    def test_removing_or_changing_a_source_fails(self):
        self.assertFalse(audit({"a": passing()}, {}, {})["passed"])
        changed = passing()
        changed["input"]["sha256"] = "different"
        self.assertFalse(audit({"a": passing()}, {"a": changed}, {})["passed"])

    def test_timeout_or_signal_is_not_an_explicit_rejection(self):
        old = passing()
        old["output_checks"]["valid"] = False
        new = {"input": old["input"], "compression": {
            "returncode": 1, "timed_out": False, "stderr": "invalid page geometry"}}
        result = audit({"a": old}, {"a": new}, {"a": "invalid page geometry"})
        self.assertTrue(result["passed"])
        self.assertEqual((result["repaired"], result["rejected"]), (0, 1))
        for override in ({"timed_out": True}, {"returncode": -9}, {"returncode": 0},
                         {"stderr": "unrelated error"}):
            bad = copy.deepcopy(new)
            bad["compression"].update(override)
            self.assertFalse(audit({"a": old}, {"a": bad}, {"a": "invalid page geometry"})["passed"])

    def test_previously_valid_output_cannot_be_whitelisted_for_rejection(self):
        new = {"input": {"sha256": "frozen"}, "compression": {
            "returncode": 1, "timed_out": False, "stderr": "unsupported"}}
        self.assertFalse(audit({"a": passing()}, {"a": new}, {"a": "unsupported"})["passed"])

    def test_independent_reference_requires_matching_hashes_and_exact_pages(self):
        old = passing()
        old["output_checks"]["valid"] = False
        new = passing()
        new["render_comparisons"]["mupdf"]["passed"] = False
        reference = {"passed": True, "samples_equal": True, "image_attributes_equal": True,
                     "input_sha256": "frozen", "output_sha256": "rewritten",
                     "decoder": {"source_sha256": "frozen"}, "reference_checks": {"valid": True},
                     "renderers": {name: {"passed": True, "pages": [{"exact": True}]}
                                   for name in ("poppler", "mupdf", "quartz")}}
        self.assertTrue(audit({"a": old}, {"a": new}, {}, {"a": reference})["passed"])
        for key in ("output_sha256", "input_sha256", "samples_equal"):
            bad = copy.deepcopy(reference)
            bad[key] = False
            self.assertFalse(audit({"a": old}, {"a": new}, {}, {"a": bad})["passed"])
        bad = copy.deepcopy(reference)
        bad["renderers"]["quartz"]["pages"][0]["exact"] = False
        self.assertFalse(audit({"a": old}, {"a": new}, {}, {"a": bad})["passed"])


if __name__ == "__main__":
    unittest.main()
