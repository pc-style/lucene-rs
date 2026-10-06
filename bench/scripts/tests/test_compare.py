"""Exercise the CLI status that gates run-all.sh, including optimized Python."""

from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "compare.py"
ROW = "TERM\tx\t2\teq\t0:1.000000000e+00 1:5.000000000e-01\n"


class CompareCliTests(unittest.TestCase):
    def compare(self, left=ROW, right=ROW, *, optimized=False):
        with tempfile.TemporaryDirectory() as directory:
            left_path = Path(directory) / "left.tsv"
            right_path = Path(directory) / "right.tsv"
            left_path.write_text(left, encoding="utf-8")
            right_path.write_text(right, encoding="utf-8")
            command = [sys.executable]
            if optimized:
                command.append("-O")
            command.extend([str(SCRIPT), str(left_path), str(right_path)])
            return subprocess.run(command, capture_output=True, text=True, check=False)

    def test_identical_dumps_pass(self):
        for optimized in (False, True):
            with self.subTest(optimized=optimized):
                result = self.compare(optimized=optimized)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn("queries: 1", result.stdout)
                self.assertIn("bit_identical_scores: 2", result.stdout)

    def test_every_observable_mismatch_fails(self):
        variants = {
            "kind": ROW.replace("TERM", "OR"),
            "query": ROW.replace("\tx\t", "\ty\t"),
            "row_count": ROW + ROW.replace("\tx\t", "\ty\t"),
            "hit_count": ROW.replace(" 1:5.000000000e-01", ""),
            "doc_order": ROW.replace("0:1.000000000e+00 1:5.000000000e-01", "1:1.000000000e+00 0:5.000000000e-01"),
            "doc_id": ROW.replace("0:1.000", "9:1.000"),
            "one_ulp_score": ROW.replace("1.000000000e+00", "1.000000119e+00"),
            "total": ROW.replace("\t2\teq\t", "\t3\teq\t"),
            "relation": ROW.replace("\teq\t", "\tgte\t"),
        }
        for name, row in variants.items():
            for optimized in (False, True):
                with self.subTest(name=name, optimized=optimized):
                    result = self.compare(right=row, optimized=optimized)
                    self.assertEqual(result.returncode, 1, result.stderr)
                    self.assertIn("DIFF", result.stdout)

    def test_score_only_difference_is_diagnosed(self):
        result = self.compare(right=ROW.replace("1.000000000e+00", "1.000000119e+00"))
        self.assertEqual(result.returncode, 1)
        self.assertIn("score", result.stdout.lower())

    def test_signed_zero_is_a_bit_difference(self):
        left = "TERM\tx\t1\teq\t0:0.0\n"
        right = "TERM\tx\t1\teq\t0:-0.0\n"
        result = self.compare(left, right)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("bit_identical_scores: 0", result.stdout)

    def test_decimal_spellings_that_round_to_the_same_f32_pass(self):
        result = self.compare(right=ROW.replace("1.000000000e+00", "1.00000001"))
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_empty_hit_vector_passes(self):
        row = "TERM\tabsent\t0\teq\t\n"
        result = self.compare(row, row)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_lower_bound_values_are_strict_dump_equality(self):
        left = ROW.replace("\t2\teq\t", "\t1001\tgte\t")
        right = ROW.replace("\t2\teq\t", "\t1002\tgte\t")
        result = self.compare(left, right)
        self.assertEqual(result.returncode, 1, result.stderr)

    def test_legacy_relationless_dumps_request_regeneration(self):
        row = ROW.replace("\teq\t", "\t")
        result = self.compare(row, row)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("rerun", result.stderr.lower())

    def test_malformed_dumps_fail_even_when_identical(self):
        variants = {
            "empty": "",
            "blank_row": "\n",
            "missing_column": "TERM\tx\t2\n",
            "extra_column": ROW.rstrip("\n") + "\textra\n",
            "unknown_kind": ROW.replace("TERM", "UNKNOWN"),
            "empty_query": ROW.replace("\tx\t", "\t\t"),
            "negative_total": ROW.replace("\t2\t", "\t-1\t"),
            "bad_total": ROW.replace("\t2\t", "\ttwo\t"),
            "overflow_total": ROW.replace("\t2\t", "\t18446744073709551616\t"),
            "unknown_relation": ROW.replace("\teq\t", "\tunknown\t"),
            "negative_doc": ROW.replace("0:1.000", "-1:1.000"),
            "bad_doc": ROW.replace("0:1.000", "bad:1.000"),
            "overflow_doc": ROW.replace("0:1.000", "4294967296:1.000"),
            "missing_score": ROW.replace("0:1.000000000e+00", "0"),
            "bad_score": ROW.replace("1.000000000e+00", "bad"),
            "nan": ROW.replace("1.000000000e+00", "nan"),
            "positive_inf": ROW.replace("1.000000000e+00", "inf"),
            "negative_inf": ROW.replace("1.000000000e+00", "-inf"),
            "overflow_f32": ROW.replace("1.000000000e+00", "1e100"),
        }
        for name, row in variants.items():
            with self.subTest(name=name):
                result = self.compare(row, row)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("error", result.stderr.lower())

    def test_usage_is_nonzero(self):
        result = subprocess.run([sys.executable, str(SCRIPT)], capture_output=True, text=True, check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("usage", result.stderr.lower())


if __name__ == "__main__":
    unittest.main()
