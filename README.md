# Shared local AASB corpus

Extract local AASB PDFs into page records and search them through a Rust CLI and SQLite FTS5 index. Results retain the source PDF hash, physical PDF page number, original URL and catalogue version metadata. The corpus is a finding aid; selecting an applicable reporting framework or version remains a separate review.

The installed pdf-inspector exposes its extraction and offline OCR through Python, so `tools/extract.py` is the native extraction bridge. Indexing, search, page lookup, quotation checks and verification are Rust. No command uploads documents, calls a model provider or downloads content.

## Current local snapshot

Code: `C:\Users\-\Documents\Code\aasb-corpus`.

Data: `C:\Users\-\Documents\AASB\corpus\2026-10-15`.

The snapshot preserves `source/`, its download manifest and `catalogue.json`. Extraction produces `extracted/<PDF SHA-256>/pages.jsonl` and `document.md`, plus an extraction manifest. Exact duplicate source files share one extraction; all original filename and URL aliases remain in the index. All source pages, including copyright notices and compilation information, remain in the extracted documents.

The index uses whole PDF pages. Citations use physical PDF page numbers, which can differ from printed page labels. Paragraph lookup identifies line anchors in one exact captured PDF and retains source spans and review flags. Amendments are not compiled into another version.

## Run

From this code directory, build the Windows executable:

```powershell
rtk cargo build --locked --release
```

Search locally:

```powershell
& .\target\release\aasb-corpus.exe search 'C:\Users\-\Documents\AASB\corpus\2026-10-15' '"lease liability"'
& .\target\release\aasb-corpus.exe stats 'C:\Users\-\Documents\AASB\corpus\2026-10-15'
& .\target\release\aasb-corpus.exe verify 'C:\Users\-\Documents\AASB\corpus\2026-10-15'
```

`verify` also prints `index_sha256`, the digest of `index.sqlite3` to retain with any citation.

`search` accepts SQLite FTS5 query syntax and returns 20 ranked results by default. `--limit` accepts 1 to 100 and `--offset` starts at zero. Results include `has_more` and `next_offset`; continue with that offset and the same query, filters and unchanged snapshot. Ordering is by search rank, document hash and physical page.

### Choose a standard and PDF version

```powershell
$snapshot = 'C:\Users\-\Documents\AASB\corpus\2026-10-15'
& .\target\release\aasb-corpus.exe catalogue $snapshot --standard 'AASB 16'
& .\target\release\aasb-corpus.exe search $snapshot '"lease liability"' --standard 'AASB 16' --limit 5 --offset 0
```

`catalogue` lists document hashes, aliases, version labels and raw period metadata. It supports the same filters and pagination, ordered by document hash. `--standard` matches a complete principal-standard identifier, ignoring case and whitespace: `AASB 1` cannot match `AASB 16`. Related amendment documents remain separate; list them in the unfiltered catalogue or select their exact hash.

`--version` is the full `document_sha256` from the catalogue, which identifies exact PDF bytes even when version labels repeat. Choose the compilation using its metadata and the reporting framework and period. Then set `$version` to that hash:

```powershell
& .\target\release\aasb-corpus.exe search $snapshot '"lease liability"' --standard 'AASB 16' --version $version --limit 5
```

Filters apply before ranking and pagination. Combining a standard with a different standard's version returns no matches; an unknown version is an error. No command determines accounting applicability from catalogue dates.

### Look up a paragraph

```powershell
& .\target\release\aasb-corpus.exe paragraph $snapshot $version '26'
```

Supply an exact PDF hash and a label such as `35`, `B19`, `Aus25.1` or `BC1`. Request the parent paragraph for lettered subparagraphs such as `35(a)`; its conditions remain in the returned text.

The result retains document metadata and candidate fragments with physical PDF pages, one-based inclusive line spans, page-text hashes, native/OCR provenance and review flags. Each fragment is an unchanged slice of the indexed page. Continuations can span multiple pages; recognised page footers are excluded. Recognised `Corresponding AASB Standard` footnotes are returned separately from the paragraph text.

- `found` means one candidate has native text and an ending confirmed by an adjacent paragraph label.
- `ambiguous` preserves repeated occurrences for review, including possible examples or amendments.
- `needs_review` identifies uncertain endings, headings that interrupt unanchored content, page gaps, tables or OCR/review flags.
- `not_found` means no supported extracted line anchor was found. It does not establish that the PDF lacks the paragraph.

Only treat `found` as a located source span; the accounting conclusion remains separate. Lookup excludes recognised contents pages, bounds output to 20 candidates and withholds candidate text above 64 KiB while preserving page references. Unsupported layouts require reading the cited PDF pages. No general PDF-layout accuracy is claimed.

`page SNAPSHOT SHA256 PAGE` returns a full extracted page. `verify-quote SNAPSHOT SHA256 PAGE QUOTE` checks a case-sensitive quotation under the tax corpus's Unicode compatibility rules. A match establishes textual fidelity under those rules, not accounting applicability.

Other repositories can invoke this executable or read the SQLite database directly. Search connections are read-only. No consumer repository changes are needed to use the shared snapshot.

## Rebuild a new snapshot

Copy the authorised PDFs, download manifest and catalogue into a new snapshot with the same layout. Extraction refuses an existing output directory:

```powershell
& 'C:\Tools\pdf-inspector\.venv\Scripts\python.exe' -X utf8 tools/extract.py 'C:\path\to\new-snapshot'
& .\target\release\aasb-corpus.exe build 'C:\path\to\new-snapshot'
```

On the Linux sandbox, use `agent-run pdf-inspector-python tools/extract.py SNAPSHOT`. The extractor verifies source hashes, checks every page index, uses offline OCR only where pdf-inspector requests it and retains review flags. It stages extraction before renaming the completed folder. Indexing checks input hashes and complete document/page coverage, builds and checks FTS5 in one transaction, then promotes a new index. Failed stages are retained for inspection and never replace a completed snapshot.

## Checks

```powershell
rtk cargo fmt --check
rtk cargo test --locked
rtk cargo clippy --locked --all-targets -- -D warnings
& 'C:\Tools\pdf-inspector\.venv\Scripts\python.exe' -X utf8 -m unittest discover -s tools -p 'test_*.py'
```

The optional live-index check traverses filtered results and checks actual paragraph fragments against indexed page slices. On Linux, set `AASB_TEST_SNAPSHOT` to the local snapshot and run:

```sh
AASB_TEST_SNAPSHOT=/path/to/snapshot rtk cargo test --locked --test local_snapshot -- --ignored --nocapture
```

Ordinary tests use fabricated text. The live check stays local and prints counts and statuses only.

## Reuse and licence

Quotation normalisation and regression cases adapt `fadden/as_at.py`; page completeness adapts the invariant in `fadden/extract.py`; staging follows the tax corpus's complete-output-before-promotion pattern. Upstream is au-tax-legislation-corpus at `84db0481a924f07d3f697f54a269917475613184`. Its tax date model, Register EPUB parser, attribution assumptions and publication filters are not used for AASB documents.

Code is Apache-2.0; retain LICENSE and NOTICE. Source documents keep their own copyright notices and rights. This local build does not publish the corpus.
