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


class SecondReaderTests(unittest.TestCase):
    def test_readings_cover_every_page_and_missing_sources_stay_unconfirmed(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            (root / "source").mkdir()
            content = fabricated_pdf(2)
            (root / "source" / "sample.pdf").write_bytes(content)
            sha = hashlib.sha256(content).hexdigest()
            files = [{"filename": "sample.pdf", "sha256": sha, "page_count": 2},
                     {"filename": "absent.pdf", "sha256": "0" * 64, "page_count": 1}]
            (root / "source" / "manifest.json").write_text(json.dumps({"files": files}), encoding="utf-8")
            self.assertEqual(second_reader.read(root), {"documents": 1, "failures": 1})
            manifest = json.loads((root / "second-reader" / "manifest.json").read_text(encoding="utf-8"))
            self.assertEqual(manifest["reader"], "pypdf")
            self.assertEqual([d["sha256"] for d in manifest["documents"]], [sha])
            pages = (root / "second-reader" / sha / "pages.jsonl").read_bytes()
            self.assertEqual(manifest["documents"][0]["pages_sha256"], hashlib.sha256(pages).hexdigest())
            self.assertEqual([json.loads(line)["page"] for line in pages.splitlines()], [1, 2])
            self.assertEqual(manifest["failures"][0]["filename"], "absent.pdf")
            with self.assertRaises(ValueError):
                second_reader.read(root)


if __name__ == "__main__":
    unittest.main()
