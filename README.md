# Shared local AASB corpus

Extract local AASB PDFs into page records and search them through a Rust CLI and SQLite FTS5 index. Results retain the source PDF hash, physical PDF page number, original URL and catalogue version metadata. The corpus is a finding aid; selecting an applicable reporting framework or version remains a separate review.

The installed pdf-inspector exposes its extraction and offline OCR through Python, so `tools/extract.py` is the native extraction bridge. `tools/second_reader.py` reads the same PDFs with pypdf, already installed in that environment, to confirm paragraph lookups without a new dependency. Indexing, search, page lookup, quotation checks and verification are Rust. No command uploads documents, calls a model provider or downloads content.

## Current local snapshot

Code: `C:\Users\-\Documents\Code\aasb-corpus`.

Data: `C:\Users\-\Documents\AASB\corpus\2026-10-15`.

The snapshot preserves `source/`, its download manifest and `catalogue.json`. Extraction produces `extracted/<PDF SHA-256>/pages.jsonl` and `document.md`, plus an extraction manifest. Exact duplicate source files share one extraction; all original filename and URL aliases remain in the index. All source pages, including copyright notices and compilation information, remain in the extracted documents.

The index holds whole PDF pages and, from schema 3, a paragraph table built from them. Citations use physical PDF page numbers, which can differ from printed page labels. Paragraph lookup identifies line anchors in one exact captured PDF and retains source spans and review flags. Amendments are not compiled into another version.

## Run

From this code directory, build the executable:

```powershell
cargo build --locked --release
```

Search locally:

```powershell
& .\target\release\aasb-corpus.exe search 'C:\Users\-\Documents\AASB\corpus\2026-10-15' '"lease liability"'
& .\target\release\aasb-corpus.exe stats 'C:\Users\-\Documents\AASB\corpus\2026-10-15'
& .\target\release\aasb-corpus.exe verify 'C:\Users\-\Documents\AASB\corpus\2026-10-15'
```

`verify` also prints `index_sha256`, the digest of `index.sqlite3` to retain with any citation.

`search` accepts SQLite FTS5 query syntax and matches the words as written; `--stem` also matches other forms of each word, so `leases` finds `leased`. It returns 20 ranked results by default. `--limit` accepts 1 to 100 and `--offset` starts at zero. Results include `has_more` and `next_offset`; continue with that offset and the same query, filters and unchanged snapshot. Ordering is by search rank, document hash and physical page.

A stemmed search finds candidates; its results carry `stemmed` and `stem_boundary`, and any quotation still goes through `verify-quote`. `--stem` needs an index built with the stem table. An older index keeps working for exact search and stops `--stem` with an error. To add the table, move `index.sqlite3` out of the snapshot, run `build`, run `verify` and keep the new `index_sha256`: citations made against the old index keep the old digest, so keep that file too.

`--paragraphs` ranks stored paragraphs instead of pages, with or without `--stem`. Each result names its `paragraph` label, its reading-order `ordinal` in the PDF and the physical page of its anchor; look the label up with `paragraph` before relying on it. Every query word must fall within one paragraph, so a page search can find a requirement whose words span 2 paragraphs where a paragraph search does not. `--paragraphs` needs an index with the paragraph table (schema 3); rebuild an older index as above to add it.

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

Filters apply before ranking and pagination. Combining a standard with a different standard's version returns no matches; an unknown version is an error. No command determines accounting applicability from catalogue dates. From schema 4 each catalogue result also carries the `validity` and `reporting_tier` described under 'Facts derived at build'.

### Look up a paragraph

```powershell
& .\target\release\aasb-corpus.exe paragraph $snapshot $version '26'
```

Supply an exact PDF hash and a label such as `35`, `B19`, `Aus25.1` or `BC1`. Request the parent paragraph for lettered subparagraphs such as `35(a)`; its conditions remain in the returned text.

The result retains document metadata and candidate fragments with physical PDF pages, one-based inclusive line spans, page-text hashes, native/OCR provenance and review flags. Each fragment is an unchanged slice of the indexed page. Continuations can span multiple pages; recognised page footers are excluded. Recognised `Corresponding AASB Standard` footnotes are returned separately from the paragraph text.

- `found` means one candidate has native text and an ending confirmed by an adjacent paragraph label, and a second reader confirms it (below).
- `found_unconfirmed` meets the other `found` conditions, but the snapshot has no `second-reader/` folder, pypdf's failure on the selected PDF is recorded, or a source file recorded for the PDF is missing, unreadable or replaced.
- `ambiguous` preserves repeated occurrences for review, including possible examples or amendments.
- `needs_review` identifies uncertain endings, headings that interrupt unanchored content, page gaps, tables or OCR/review flags.
- `not_found` means no supported extracted line anchor was found. It does not establish that the PDF lacks the paragraph.

The second reader is pypdf, a PDF parser independent of pdf-inspector that is installed in the same environment. `tools/second_reader.py` stores its text for every page in `second-reader/`, with per-document digests. Before reporting `found`, lookup checks that each fragment's words appear contiguously in pypdf's text of the same physical page, starting and ending on pypdf word boundaries. Punctuation, markup and spacing are ignored and case is kept. pypdf may split one word into adjacent pieces, as it does at some kerning gaps, but no pypdf word may be skipped, matched in part or joined across fragment words. A mismatch, or a fragment without words, reports `needs_review` with a review reason. Lookup first rehashes every source file recorded for the PDF and confirms nothing if any is missing or changed. `second_reader` records `confirmed`, `mismatch` (with `unconfirmed_pages`), `unavailable`, `source_changed` (with one entry in `reasons` per missing, unreadable or replaced file) or `not_checked`, plus the reader and its version.

Once `second-reader/` exists, its manifest must have the supported schema and reader, the digest of the current source manifest, and exactly one document or recorded failure for each source PDF; each reading must match its digest and cover every page. Anything else, including a missing manifest, is an error rather than `found_unconfirmed`. So is a found result without fragment text or physical pages.

Only treat `found` as a located source span; the accounting conclusion remains separate. Lookup excludes recognised contents pages, bounds output to 20 candidates and withholds candidate text above 64 KiB whilst preserving page references. Unsupported layouts require reading the cited PDF pages. No general PDF-layout accuracy is claimed.

`paragraph SNAPSHOT SHA256 LABEL --context N` also returns up to N (1 to 5) neighbouring paragraphs on each side of every candidate, in reading order, under the candidate's `context` (`before`, `after` and its own `ordinal`). Neighbours carry the same spans, hashes and review flags as a lookup, but the second reader does not check them: look one up to confirm it. A condition in an adjacent paragraph then arrives with the paragraph that depends on it.

`build` stores every anchored paragraph with the candidate lookup reports for it, so lookups read the table instead of re-scanning pages. `verify` re-derives every document's paragraphs from its indexed pages and fails if the table differs. An index without the table (schema 1 or 2) still answers `paragraph` from its pages and refuses `--context`. On the `2026-10-15` snapshot the table holds 26,009 paragraphs and the index grows from 43 MB to 124 MB; 1,950 sampled labels returned identical output from the table and from the pages.

### Facts derived at build

From schema 4, `build` also records 3 facts from the snapshot's own sources, and `paragraph` returns them. Where a source does not state a fact, it stays empty; nothing is inferred.

- `cited_by`: pages of other standards' documents whose text cites this paragraph as `paragraph 26 of AASB 16`, `paragraphs 26, 27 and Aus27.1 of AASB 16` or `paragraphs B19–B21 of AASB 3`, grouped by the citing document's kind from its catalogue rows: `interpretations`, `agenda_decisions`, `amending_standards`, `standards` and `other`, with a `total` and at most 20 of each kind. A range cites only its endpoints and `35(a)` cites `35`. A citation names a standard, not a version, so it links to the label in every captured version; check the numbering in the version you cite. Versions of the same standard are left out. The `2026-10-15` snapshot holds no Interpretation or agenda decision document, so those 2 lists are empty there; a snapshot that adds them fills them on rebuild.
- `reporting_tier`: the tier a catalogue title states (`Tier 2 Entities` or a closing `Tier 2 Disclosures`, and likewise Tier 3), with that title as `basis`. Titles that state no tier, or a mixed one such as `Tier 2 and Other`, leave it null. The standards' appendices on simplified disclosures hold almost no numbered paragraphs, so the tier is recorded per document, not per paragraph.
- `validity`: the mandatory periods the catalogue's `applies_to_periods_text` states (`applies_from`, `applies_before`, `periods_ending_before`, `periods_ending_on_or_after`) and a `status` at the catalogue's `retrieved_date` (`as_at`): `not_yet_effective` when no period may yet begin, `superseded` once a period beginning that day is no longer covered, otherwise `current`. An early-application sentence is not read. The status is null when a row states a date in a form not read, or when rows for one PDF give different statuses; the dates are null when those rows state different periods. `superseded_by` lists versions of the same standard whose periods begin on this version's `applies_before`. It restates catalogue text; it does not decide which version applies to a reporting period.

On the `2026-10-15` snapshot with the 21 November catalogue correction, the build found 4,285 citations of 56 standards (3,920 from standards, 361 from amending standards and 4 from other documents). Of the 190 documents, 71 were `current`, 103 `superseded`, 11 `not_yet_effective` and 5 without a status. 8 AASB 1060 documents state Tier 2 and 1 AASB 1061 document states Tier 3.

An index before schema 4 omits these fields.

`page SNAPSHOT SHA256 PAGE` returns a full extracted page.

### Check a quotation

```powershell
& .\target\release\aasb-corpus.exe verify-quote $snapshot $version 12 'quoted words'
& .\target\release\aasb-corpus.exe verify-quote $snapshot $version 'quoted words'
```

`verify-quote SNAPSHOT SHA256 [PAGE] QUOTE` grades a case-sensitive quotation against the indexed page text:

- `exact`: the quotation's bytes appear unchanged in the page text.
- `normalised`: it matches only under the tax corpus's Unicode compatibility rules or after collapsing whitespace.
- `partial`: at least 4 consecutive quotation words match. `matched_words`, `quote_words` and the one-based `first_matched_word` describe the longest run.
- `none`: no grade above applies.

`verified` is true only for an `exact` grade on the named physical page of the selected PDF, where that page is native text without review flags and every source file recorded for the PDF still has its hash. The result binds it to `document_sha256`, `pdf_page`, `page_text_sha256` and `version_metadata`; the page text is rehashed against the index first. `unverified_reasons` lists each unmet condition, including each missing or replaced source PDF file by name. `matches` repeats `verified` for existing callers; before graded checking it also accepted normalised matches.

Without PAGE the command searches every page of the selected PDF and lists up to 20 pages with the strongest grade under `pages`, but `verified` stays false: run it again with a listed page to verify. Normalised and partial grades, OCR or review pages and searches without a page locate candidates for review. A verified match establishes textual fidelity, not accounting applicability.

### Serve an MCP client

```powershell
& .\target\release\aasb-corpus.exe mcp $snapshot
```

`mcp SNAPSHOT` serves 4 read-only tools over stdio, one JSON-RPC message a line: `search_aasb_pages`, `list_aasb_documents`, `read_aasb_paragraph` and `verify_aasb_quote`. It accepts the `initialize` handshake of MCP revisions 2025-11-25, 2025-06-18 and 2024-11-05, and refuses JSON-RPC batches, so it does not offer 2025-03-26, which requires them. Each takes the matching command's arguments by name (`document_sha256` for the PDF hash, `context` for `--context`, and `stem` and `paragraphs` as booleans) and returns the JSON that command prints, so the statuses and grades above apply unchanged. The snapshot is fixed at start-up and no tool takes a path; an argument a tool does not declare is refused. Once serving, the server writes nothing to stderr; a start-up failure, such as an unbuilt snapshot, is reported there before it exits. It makes no network request, but the MCP host sends tool results, page text included, to its model.

Other repositories can invoke this executable or read the SQLite database directly. Search connections are read-only. No consumer repository changes are needed to use the shared snapshot.

## Rebuild a new snapshot

Copy the authorised PDFs, download manifest and catalogue into a new snapshot with the same layout. Extraction refuses an existing output directory:

```powershell
& 'C:\Tools\pdf-inspector\.venv\Scripts\python.exe' -X utf8 tools/extract.py 'C:\path\to\new-snapshot'
& 'C:\Tools\pdf-inspector\.venv\Scripts\python.exe' -X utf8 tools/second_reader.py 'C:\path\to\new-snapshot'
& .\target\release\aasb-corpus.exe build 'C:\path\to\new-snapshot'
```

On the Linux sandbox, use `agent-run pdf-inspector-python tools/extract.py SNAPSHOT`. The extractor verifies source hashes, checks every page index, uses offline OCR only where pdf-inspector requests it and retains review flags. It stages extraction before renaming the completed folder. Indexing checks input hashes and complete document/page coverage, builds and checks FTS5 in one transaction, then promotes a new index. Failed stages are retained for inspection and never replace a completed snapshot.

`tools/second_reader.py` stages its output and adds `second-reader/` to an existing snapshot without changing the index; it refuses an existing output folder. A missing, unreadable or changed source file, a `page_count` in the source manifest that is not a positive integer or differs between entries with one SHA-256, or a source manifest that changes during the run, stops it with nothing promoted. Only a PDF whose verified bytes pypdf cannot read, or reads with a different page count, is listed under `failures` with its hash; its paragraphs report `found_unconfirmed`.

## Checks

```powershell
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
& 'C:\Tools\pdf-inspector\.venv\Scripts\python.exe' -X utf8 -m unittest discover -s tools -p 'test_*.py'
```

The optional live-index check traverses filtered results and checks actual paragraph fragments against indexed page slices. On Linux, set `AASB_TEST_SNAPSHOT` to the local snapshot and run:

```sh
AASB_TEST_SNAPSHOT=/path/to/snapshot cargo test --locked --test local_snapshot -- --ignored --nocapture
```

Ordinary tests use fabricated text. The live check stays local and prints counts and statuses only.

The same command also runs the reference questions in `tests/fixtures/reference-questions.tsv`: for each question it looks up the expected paragraph in every version of the standard, then counts how often exact and stemmed search put that paragraph's page in the top 5 and top 20, and how often paragraph search returns its label there. An index without the stem table counts exact page search only; one without the paragraph table skips paragraph search. The counts measure this question set, not general recall, and the file holds only short queries and paragraph labels.

On 13 November 2026 the `2026-10-15` snapshot (`index_sha256` `114641a50b25524d217fcbd143b6b36bd03472f80d5b6d29f321b152965f88dc`) gave these counts for the 28 questions. The 81 paragraph lookups, one for each version of each question's standard, were 46 `found`, 6 `ambiguous`, 22 `needs_review` and 7 `not_found`. 74 of them located a page, and exact search put that page in the top 5 for 46 and in the top 20 for 56. Stemmed search was not counted: that index predates the stem table.

On 24 November 2026 the same snapshot rebuilt at schema 3 (`index_sha256` `66d6efb478a9f3ad02fce6190ec9b138998a499bcc65aa189136691095a47a40`) gave the same lookup statuses and exact page counts. Stemmed page search put the page in the top 5 for 68 and the top 20 for 72; paragraph search returned the label in the top 5 for 39 and the top 20 for 42, or 59 and 67 stemmed.

The fixture then gained 3 AASB S2 questions: its scope, its effective date and the year an entity must first report, which the Corporations Act sets (the fixture's comments cite the sections read). With the 21 November catalogue at schema 4 (`index_sha256` `b03d12cd758138f8a9a5916164fd201acb989e26045affbb9b1e4e56156cdcda`), the 31 questions' 87 lookups were 49 `found`, 8 `ambiguous`, 22 `needs_review` and 8 `not_found`. 79 located a page: exact page search put it in the top 5 for 51 and the top 20 for 61, stemmed for 73 and 77; paragraph search returned the label for 44 and 47, stemmed for 64 and 72. In both AASB S2 versions, label `3` is ambiguous because the copyright notice's numbered clauses also anchor as paragraphs. In the December 2025 compilation, `AusC1.1` is `not_found` because extraction puts it on the same line as the deleted `C1`.

## Reuse and licence

Quotation normalisation and regression cases adapt `fadden/as_at.py`; page completeness adapts the invariant in `fadden/extract.py`; staging follows the tax corpus's complete-output-before-promotion pattern. Upstream is [PaulGoodman](https://github.com/ryanduguid/PaulGoodman), formerly au-tax-legislation-corpus, at `84db0481a924f07d3f697f54a269917475613184`. Its tax date model, Register EPUB parser, attribution assumptions and publication filters are not used for AASB documents.

Code is Apache-2.0; retain LICENSE and NOTICE. Source documents keep their own copyright notices and rights. This local build does not publish the corpus.
