import hashlib
import io
import json
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path
import runpy
import sys
import tempfile
import types
import unittest
from unittest import mock

# The real package reads PDFs; these tests replace its two calls with fabricated results,
# so a machine without it can still run them.
sys.modules.setdefault("pdf_inspector", types.ModuleType("pdf_inspector"))
import extract  # noqa: E402


class PageCoverageTests(unittest.TestCase):
    def test_every_printed_page_must_be_present_once_in_order(self):
        extract.check_pages([{"page": 1}, {"page": 2}], 2)
        for pages in [[{"page": 1}], [{"page": 1}, {"page": 1}],
                      [{"page": 2}, {"page": 1}], [{"page": 0}, {"page": 1}]]:
            with self.assertRaises(ValueError):
                extract.check_pages(pages, 2)

    def test_manifest_paths_cannot_select_another_folder(self):
        for value in ["", ".", "..", "C:foo", "../other.pdf", "a/b.pdf", "a\\b.pdf"]:
            with self.assertRaises(ValueError):
                extract.safe_file(value)
        self.assertEqual(extract.safe_file("001-source.pdf"), "001-source.pdf")


def native(*pages):
    """A fabricated `extract_pages_markdown` result: (markdown, needs_ocr) for each page."""
    return types.SimpleNamespace(pages=[
        types.SimpleNamespace(page=number, markdown=text, needs_ocr=ocr)
        for number, (text, ocr) in enumerate(pages)])


def ocr_result(*pages):
    """A fabricated `process_pdf_with_ocr` result: (1-based page, markdown) for each page."""
    return types.SimpleNamespace(pages=[
        types.SimpleNamespace(page_number=number, markdown=text) for number, text in pages])


def snapshot(folder, files):
    """Writes source files and their download manifest; `files` holds (name, bytes, pages)."""
    root = Path(folder)
    (root / "source").mkdir()
    entries = [{"filename": name, "sha256": hashlib.sha256(content).hexdigest(),
                "page_count": pages} for name, content, pages in files]
    for name, content, _ in files:
        (root / "source" / name).write_bytes(content)
    (root / "source" / "manifest.json").write_text(json.dumps({"files": entries}), encoding="utf-8")
    return root


class ExtractTests(unittest.TestCase):
    def run_extract(self, root, reading, ocr=None):
        calls = {"native": reading, "ocr": ocr}
        patches = [
            mock.patch.object(extract.pdf_inspector, "extract_pages_markdown",
                              lambda path: calls["native"](path) if callable(calls["native"])
                              else calls["native"], create=True),
            mock.patch.object(extract.pdf_inspector, "process_pdf_with_ocr",
                              lambda path, **options: calls["ocr"], create=True),
            mock.patch("importlib.metadata.version", return_value="0-test"),
        ]
        out = io.StringIO()
        for patch in patches:
            patch.start()
            self.addCleanup(patch.stop)
        with redirect_stdout(out):
            extract.extract(root)
        return [json.loads(line) for line in out.getvalue().splitlines()]

    def test_native_and_ocr_pages_are_written_with_their_review_reasons(self):
        with tempfile.TemporaryDirectory() as folder:
            same = b"Fabricated PDF bytes"
            root = snapshot(folder, [("a.pdf", same, 3), ("b.pdf", same, 3)])
            lines = self.run_extract(
                root, native(("one", False), ("", True), ("", False)),
                ocr_result((2, "two read by OCR")))
            self.assertEqual(lines[0]["ocr_pages"], 1)
            self.assertEqual(lines[-1], {"documents": 1, "pages": 3, "review_pages": 2})
            sha = hashlib.sha256(same).hexdigest()
            pages = [json.loads(line) for line in
                     (root / "extracted" / sha / "pages.jsonl").read_text(encoding="utf-8").splitlines()]
            self.assertEqual([(p["provenance"], p["needs_review"]) for p in pages],
                             [("native", False), ("ocr", True), ("native", True)])
            self.assertIn("two read by OCR", (root / "extracted" / sha / "document.md").read_text(encoding="utf-8"))
            receipt = json.loads((root / "extracted" / "manifest.json").read_text(encoding="utf-8"))
            self.assertEqual([row["reason"] for row in receipt["review_pages"]],
                             ["OCR", "empty native text"])
            self.assertEqual(receipt["failures"], [])
            self.assertFalse((root / "extracted.partial").exists())
            with self.assertRaises(ValueError):
                extract.extract(root)

    def test_a_failed_document_keeps_the_partial_extraction_and_promotes_nothing(self):
        content = b"Fabricated PDF bytes"

        def changes_the_source(path):
            Path(path).write_bytes(b"Changed while reading")
            return native(("one", False))

        cases = [
            ("a PDF that is not the manifest's", lambda root: (root / "source" / "a.pdf").write_bytes(b"x"),
             native(("one", False)), None, "Source SHA-256 differs"),
            ("a missing page", lambda root: None, native(("one", False)), None, "omitted, repeated or reordered"),
            ("OCR that skips a page", lambda root: None, native(("one", True), ("two", False)),
             ocr_result(), "OCR omitted a requested page"),
            ("a source changed during extraction", lambda root: None, changes_the_source, None,
             "Source changed during extraction"),
        ]
        for label, damage, reading, ocr, reason in cases:
            with self.subTest(label), tempfile.TemporaryDirectory() as folder:
                root = snapshot(folder, [("a.pdf", content, 2 if "page" in label else 1)])
                damage(root)
                with self.assertRaisesRegex(ValueError, r"1 documents failed; retained extracted\.partial"):
                    self.run_extract(root, reading, ocr)
                self.assertFalse((root / "extracted").exists())
                receipt = json.loads((root / "extracted.partial" / "manifest.json").read_text(encoding="utf-8"))
                self.assertIn(reason, receipt["failures"][0]["error"])

    def test_invalid_page_counts_and_manifest_mutation_are_refused(self):
        same = b"Fabricated PDF bytes"
        for bad in (0, True, "1"):
            with self.subTest(page_count=bad), tempfile.TemporaryDirectory() as folder:
                root = snapshot(folder, [("a.pdf", same, bad)])
                with self.assertRaisesRegex(ValueError, "page_count is not a positive integer"):
                    self.run_extract(root, native(("one", False)))
                self.assertFalse((root / "extracted.partial").exists())
        with tempfile.TemporaryDirectory() as folder:
            root = snapshot(folder, [("a.pdf", same, 1), ("b.pdf", same, 2)])
            with self.assertRaisesRegex(ValueError, "page_count conflicts for one SHA-256"):
                self.run_extract(root, native(("one", False)))
            self.assertFalse((root / "extracted.partial").exists())
        with tempfile.TemporaryDirectory() as folder:
            root = snapshot(folder, [("a.pdf", same, 1)])

            def changes_manifest(_path):
                (root / "source" / "manifest.json").write_text('{"files":[]}', encoding="utf-8")
                return native(("one", False))

            with self.assertRaisesRegex(ValueError, "Source manifest changed during extraction"):
                self.run_extract(root, changes_manifest)
            self.assertFalse((root / "extracted").exists())

    def test_the_command_prints_a_refusal_and_exits_non_zero(self):
        with tempfile.TemporaryDirectory() as folder:
            root = snapshot(folder, [("a.pdf", b"Fabricated PDF bytes", 1)])
            (root / "extracted").mkdir()
            script = str(Path(extract.__file__))
            err = io.StringIO()
            with mock.patch.object(sys, "argv", [script, str(root)]), redirect_stderr(err):
                with self.assertRaises(SystemExit) as exit_:
                    runpy.run_path(script, run_name="__main__")
            self.assertEqual(exit_.exception.code, 1)
            self.assertIn("destination already exists", err.getvalue())
            (root / "extracted").rmdir()
            fake = types.SimpleNamespace(
                extract_pages_markdown=lambda path: native(("one", False)),
                process_pdf_with_ocr=None)
            with mock.patch.object(sys, "argv", [script, str(root)]), \
                    mock.patch.dict(sys.modules, {"pdf_inspector": fake}), \
                    mock.patch("importlib.metadata.version", return_value="0-test"), \
                    redirect_stdout(io.StringIO()):
                runpy.run_path(script, run_name="__main__")
            self.assertTrue((root / "extracted" / "manifest.json").exists())

