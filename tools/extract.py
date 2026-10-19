"""Offline PDF extraction through the installed pdf-inspector Python API."""
import argparse
import hashlib
import importlib.metadata
import json
import os
from pathlib import Path
import sys

import pdf_inspector


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def safe_file(name):
    if (not isinstance(name, str) or not name or name in (".", "..") or ":" in name
            or Path(name).name != name or "/" in name or "\\" in name):
        raise ValueError("Manifest filename must be a single filename")
    return name


def check_pages(pages, expected):
    # Adapted completeness invariant from fadden/extract.py::check_chunks_complete.
    if [page["page"] for page in pages] != list(range(1, expected + 1)):
        raise ValueError("Extraction omitted, repeated or reordered a PDF page")


def extract(root):
    source = root / "source"
    manifest = json.loads((source / "manifest.json").read_text(encoding="utf-8"))
    final = root / "extracted"
    stage = root / "extracted.partial"
    if final.exists() or stage.exists():
        raise ValueError("Extraction destination already exists; choose a new snapshot")
    stage.mkdir()
    documents = {}
    review = []
    failures = []
    for entry in manifest["files"]:
        name = safe_file(entry["filename"])
        path = source / name
        try:
            sha = digest(path)
            if sha != entry["sha256"]:
                raise ValueError("Source SHA-256 differs from the download manifest")
            if sha in documents:
                continue
            result = pdf_inspector.extract_pages_markdown(str(path))
            pages = [{"page": page.page + 1, "text": page.markdown,
                      "provenance": "native", "needs_review": bool(page.needs_ocr)}
                     for page in result.pages]
            check_pages(pages, entry["page_count"])
            needs_ocr = [page["page"] for page in pages if page["needs_review"]]
            if needs_ocr:
                ocr = pdf_inspector.process_pdf_with_ocr(
                    str(path), page_numbers=needs_ocr, offline=True, dpi=200,
                    hosted_recommendation_confidence=0.9)
                ocr_pages = {page.page_number: page for page in ocr.pages}
                for page in pages:
                    if page["page"] not in needs_ocr:
                        continue
                    if page["page"] not in ocr_pages:
                        raise ValueError("OCR omitted a requested page")
                    recovered = ocr_pages[page["page"]]
                    page["text"] = recovered.markdown
                    page["provenance"] = "ocr"
                    # OCR needs review even when the engine reports high confidence.
                    page["needs_review"] = True
            check_pages(pages, entry["page_count"])
            for page in pages:
                if not page["text"].strip():
                    page["needs_review"] = True
            if digest(path) != sha:
                raise ValueError("Source changed during extraction")
            folder = stage / sha
            folder.mkdir()
            page_file = folder / "pages.jsonl"
            with page_file.open("x", encoding="utf-8", newline="\n") as stream:
                for page in pages:
                    stream.write(json.dumps(page, ensure_ascii=False) + "\n")
            with (folder / "document.md").open("x", encoding="utf-8", newline="\n") as stream:
                for page in pages:
                    stream.write(f"<!-- PDF page {page['page']} -->\n\n{page['text']}\n\n")
            documents[sha] = {"sha256": sha, "page_count": len(pages),
                              "pages_sha256": digest(page_file)}
            for page in pages:
                if page["needs_review"] or not page["text"].strip():
                    review.append({"sha256": sha, "page": page["page"],
                                   "reason": "OCR" if page["provenance"] == "ocr" else "empty native text"})
            print(json.dumps({"completed": len(documents), "filename": name,
                              "pages": len(pages), "ocr_pages": len(needs_ocr)}), flush=True)
        except Exception as error:
            failures.append({"filename": name, "error": str(error)})
    receipt = {"schema_version": 1, "extractor": "pdf-inspector",
               "extractor_version": importlib.metadata.version("pdf-inspector"),
               "source_manifest_sha256": digest(source / "manifest.json"),
               "documents": list(documents.values()), "review_pages": review, "failures": failures}
    with (stage / "manifest.json").open("x", encoding="utf-8") as stream:
        json.dump(receipt, stream, indent=2)
        stream.write("\n")
        stream.flush()
        os.fsync(stream.fileno())
    if failures:
        raise ValueError(f"{len(failures)} documents failed; retained extraction.partial for inspection")
    stage.rename(final)
    print(json.dumps({"documents": len(documents),
                      "pages": sum(row["page_count"] for row in documents.values()),
                      "review_pages": len(review)}))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("snapshot", type=Path)
    arguments = parser.parse_args()
    try:
        extract(arguments.snapshot.resolve())
    except Exception as error:
        print(str(error), file=sys.stderr)
        sys.exit(1)
