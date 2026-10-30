import hashlib
import io
import json
from pathlib import Path
import tempfile
import unittest

import pypdf

import second_reader


def fabricated_pdf(pages):
    writer = pypdf.PdfWriter()
    for _ in range(pages):
        writer.add_blank_page(width=72, height=72)
    stream = io.BytesIO()
    writer.write(stream)
    return stream.getvalue()


def snapshot(folder, files):
    """Writes source files and a download manifest; `files` holds (name, bytes, pages, sha)."""
    root = Path(folder)
    (root / "source").mkdir()
    entries = []
    for name, content, pages, sha in files:
        if content is not None:
            (root / "source" / name).write_bytes(content)
        entries.append({"filename": name, "sha256": sha or hashlib.sha256(content).hexdigest(),
                        "page_count": pages})
    (root / "source" / "manifest.json").write_text(json.dumps({"files": entries}), encoding="utf-8")
    return root


class SecondReaderTests(unittest.TestCase):
    def test_readings_cover_every_page_and_record_reader_failures(self):
        content = fabricated_pdf(2)
        broken = b"Fabricated bytes that are not a PDF"
        with tempfile.TemporaryDirectory() as folder:
            root = snapshot(folder, [("sample.pdf", content, 2, None),
                                     ("broken.pdf", broken, 1, None)])
            self.assertEqual(second_reader.read(root), {"documents": 1, "failures": 1})
            manifest = json.loads((root / "second-reader" / "manifest.json").read_text(encoding="utf-8"))
            self.assertEqual((manifest["schema_version"], manifest["reader"]), (1, "pypdf"))
            sha = hashlib.sha256(content).hexdigest()
            self.assertEqual([d["sha256"] for d in manifest["documents"]], [sha])
            self.assertEqual(manifest["source_manifest_sha256"],
                             hashlib.sha256((root / "source" / "manifest.json").read_bytes()).hexdigest())
            pages = (root / "second-reader" / sha / "pages.jsonl").read_bytes()
            self.assertEqual(manifest["documents"][0]["pages_sha256"], hashlib.sha256(pages).hexdigest())
            self.assertEqual([json.loads(line)["page"] for line in pages.splitlines()], [1, 2])
            self.assertEqual(manifest["failures"][0]["sha256"], hashlib.sha256(broken).hexdigest())
            with self.assertRaises(ValueError):
                second_reader.read(root)

    def test_changed_or_missing_sources_are_fatal_and_publish_nothing(self):
        content = fabricated_pdf(1)
        for files in ([("sample.pdf", content, 1, "0" * 64)],
                      [("sample.pdf", content, 1, None), ("absent.pdf", None, 1, "1" * 64)]):
            with tempfile.TemporaryDirectory() as folder:
                root = snapshot(folder, files)
                with self.assertRaises((ValueError, OSError)):
                    second_reader.read(root)
                self.assertFalse((root / "second-reader").exists())

    def test_invalid_page_counts_are_fatal_and_publish_nothing(self):
        content = fabricated_pdf(1)
        for pages in (None, 0, -1, "1", True, 1.0, "missing"):
            with self.subTest(pages=pages), tempfile.TemporaryDirectory() as folder:
                root = snapshot(folder, [("sample.pdf", content, pages, None)])
                if pages == "missing":
                    manifest = root / "source" / "manifest.json"
                    entries = json.loads(manifest.read_text(encoding="utf-8"))
                    del entries["files"][0]["page_count"]
                    manifest.write_text(json.dumps(entries), encoding="utf-8")
                with self.assertRaises(ValueError):
                    second_reader.read(root)
                self.assertFalse((root / "second-reader").exists())

    def test_conflicting_page_counts_for_one_hash_are_fatal_and_stage_nothing(self):
        content = fabricated_pdf(1)
        with tempfile.TemporaryDirectory() as folder:
            root = snapshot(folder, [("sample.pdf", content, 1, None), ("copy.pdf", content, 2, None)])
            with self.assertRaises(ValueError):
                second_reader.read(root)
            self.assertFalse((root / "second-reader").exists())
            self.assertFalse((root / "second-reader.partial").exists())


if __name__ == "__main__":
    unittest.main()
