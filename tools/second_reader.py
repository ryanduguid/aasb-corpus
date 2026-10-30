"""Independent pypdf text layer used to confirm paragraph fragments."""
import argparse
import hashlib
import importlib.metadata
import io
import json
import os
from pathlib import Path
import sys

import pypdf

from extract import check_pages, digest, safe_file


def read(root):
    source = root / "source"
    manifest_bytes = (source / "manifest.json").read_bytes()
    manifest = json.loads(manifest_bytes)
    final = root / "second-reader"
    stage = root / "second-reader.partial"
    if final.exists() or stage.exists():
        raise ValueError("Second-reader destination already exists; remove it deliberately to rerun")
    for entry in manifest["files"]:
        # Manifest metadata errors are fatal, not per-document pypdf failures.
        count = entry.get("page_count")
        if type(count) is not int or count < 1:
            raise ValueError(f"Download manifest page_count is not a positive integer: {entry.get('filename')}")
    stage.mkdir()
    documents = {}
    failures = {}
    for entry in manifest["files"]:
        name = safe_file(entry["filename"])
        # Missing, unreadable or changed sources are fatal: nothing is promoted.
        content = (source / name).read_bytes()
        sha = hashlib.sha256(content).hexdigest()
        if sha != entry["sha256"]:
            raise ValueError(f"Source SHA-256 differs from the download manifest: {name}")
        if sha in documents or sha in failures:
            continue
        try:
            reader = pypdf.PdfReader(io.BytesIO(content))
            pages = [{"page": number + 1, "text": page.extract_text()}
                     for number, page in enumerate(reader.pages)]
            check_pages(pages, entry["page_count"])
        except Exception as error:
            # Only pypdf's reading of verified bytes may fail per document; its paragraphs
            # then report found_unconfirmed.
            failures[sha] = {"filename": name, "sha256": sha, "error": str(error)}
            continue
        folder = stage / sha
        folder.mkdir()
        page_file = folder / "pages.jsonl"
        with page_file.open("x", encoding="utf-8", newline="\n") as stream:
            for page in pages:
                stream.write(json.dumps(page, ensure_ascii=False) + "\n")
        documents[sha] = {"sha256": sha, "page_count": len(pages),
                          "pages_sha256": digest(page_file)}
    manifest_sha = hashlib.sha256(manifest_bytes).hexdigest()
    if digest(source / "manifest.json") != manifest_sha:
        raise ValueError("Source manifest changed during reading")
    receipt = {"schema_version": 1, "reader": "pypdf",
               "reader_version": importlib.metadata.version("pypdf"),
               "source_manifest_sha256": manifest_sha,
               "documents": list(documents.values()), "failures": list(failures.values())}
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
