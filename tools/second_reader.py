"""Independent pypdf text layer used to confirm paragraph fragments."""
import argparse
import importlib.metadata
import json
import os
from pathlib import Path
import sys

import pypdf

from extract import check_pages, digest, safe_file


def read(root):
    source = root / "source"
    manifest = json.loads((source / "manifest.json").read_text(encoding="utf-8"))
    final = root / "second-reader"
    stage = root / "second-reader.partial"
    if final.exists() or stage.exists():
        raise ValueError("Second-reader destination already exists; remove it deliberately to rerun")
    stage.mkdir()
    documents = {}
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
            reader = pypdf.PdfReader(path)
            pages = [{"page": number + 1, "text": page.extract_text()}
                     for number, page in enumerate(reader.pages)]
            check_pages(pages, entry["page_count"])
            if digest(path) != sha:
                raise ValueError("Source changed during reading")
            folder = stage / sha
            folder.mkdir()
            page_file = folder / "pages.jsonl"
            with page_file.open("x", encoding="utf-8", newline="\n") as stream:
                for page in pages:
                    stream.write(json.dumps(page, ensure_ascii=False) + "\n")
            documents[sha] = {"sha256": sha, "page_count": len(pages),
                              "pages_sha256": digest(page_file)}
        except Exception as error:
            # A document without a second reading leaves its paragraphs unconfirmed.
            failures.append({"filename": name, "error": str(error)})
    receipt = {"schema_version": 1, "reader": "pypdf",
               "reader_version": importlib.metadata.version("pypdf"),
               "source_manifest_sha256": digest(source / "manifest.json"),
               "documents": list(documents.values()), "failures": failures}
    with (stage / "manifest.json").open("x", encoding="utf-8") as stream:
        json.dump(receipt, stream, indent=2)
        stream.write("\n")
        stream.flush()
        os.fsync(stream.fileno())
    stage.rename(final)
    return {"documents": len(documents), "failures": len(failures)}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("snapshot", type=Path)
    arguments = parser.parse_args()
    try:
        print(json.dumps(read(arguments.snapshot.resolve())))
    except Exception as error:
        print(str(error), file=sys.stderr)
        sys.exit(1)
