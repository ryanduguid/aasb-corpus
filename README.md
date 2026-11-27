# Shared local AASB corpus

<div align="center">

[![licence: Apache-2.0](https://img.shields.io/badge/licence-Apache--2.0-66023C?labelColor=000000&style=flat&color=66023C)](LICENSE)

[Current local snapshot](#current-local-snapshot) · [Run](#run) · [Rebuild a new snapshot](#rebuild-a-new-snapshot) · [Checks](#checks) · [Reuse and licence](#reuse-and-licence)

</div>

Extract local AASB PDFs into page records and search them through a Rust CLI and SQLite FTS5 index. Results retain the source PDF hash, physical PDF page number, original URL and catalogue version metadata. The corpus is a finding aid; selecting an applicable reporting framework or version remains a separate review.

The installed pdf-inspector exposes its extraction and offline OCR through Python, so `tools/extract.py` is the native extraction bridge. `tools/second_reader.py` reads the same PDFs with pypdf, already installed in that environment, to confirm paragraph lookups without a new dependency. Indexing, search, page lookup, quotation checks and verification are Rust. No command uploads documents, calls a model provider or downloads content.

## Current local snapshot

Code: `C:\Users\-\Documents\Code\aasb-corpus`.

Data: `C:\Users\-\Documents\AASB\corpus\2026-10-15-metadata-2026-11-21-schema-4`, the 15 October 2026 sources with the 21 November catalogue correction, built at schema 4 on 24 November 2026. The earlier indexes stay in `2026-10-15` and `2026-10-15-metadata-2026-11-21` so citations keep their digests; `2026-10-15-schema-4` rebuilds the original catalogue.

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

Only treat `found` as a located source span; the accounting conclusion remains separate. Lookup excludes recognised contents and copyright notice pages, whose numbered clauses are not paragraphs, and numbered footnotes at a page foot: a bare number that follows neither of the last 2 anchored labels and is below any plain number among them, after a paragraph anchored on the same page and not after a heading, with only wrapped text, higher footnote numbers, footers and running headers after it on the page. An underlined label after it, as an amending standard inserts, means it is a paragraph. It bounds output to 20 candidates and withholds candidate text above 64 KiB whilst preserving page references; `search --paragraphs` still matches that text. Unsupported layouts require reading the cited PDF pages. No general PDF-layout accuracy is claimed.

Where extraction puts a deleted paragraph's marker and the next label on one line, as in `C1 [Deleted by the AASB] AusC1.1 …`, each label anchors at its own position. A fragment that starts or ends inside a line carries a one-based `start_column` or inclusive `end_column` in characters. A label that starts inside a line after a sentence end anchors there too when it follows the current label in sequence, the text after it starts with a capital or a deleted marker, the line is not a table row, and the label appears at no other line or sentence start in the PDF. Lines skipped as page-foot footnotes do not count as starts. A sentence end may also carry a footnote marker or closing bold before its space, as in `entity.¹** 2 This`; a label after one anchors only when nothing else in the PDF starts with it, and it does not count against a label after a plain full stop, which reproduced text can repeat after bold. A label alone in the first cell of a table row, as in `|4|Climate-related risks …|||`, anchors when it follows the current label, the next cell starts with a capital or a deleted marker, and the label starts no line, sentence or other row in the PDF; a row holding only the label and its text is not flagged as a table. Any other following label inside a line, such as one that reproduced text repeats, is not anchored; the candidate holding it carries a review reason instead.

Some PDFs set a paragraph number where extraction loses it: in a margin it drops, as in AASB S2 (September 2024) paragraphs 22 and 29, or in a column of bare labels it gives before the text, as in AASB S2 (December 2025) B20 to B31. When the snapshot has a second reading, `build` places such a label where that reading prints it before a paragraph: the label must start no line, sentence or table row on the extracted page, appear once at a line start in the reading's page, and its first 6 words after it must start a line, follow a sentence end or follow closing bold exactly once on the extracted page. The bare label lines it replaces are skipped. The candidate always needs review, with the reason that the second reader placed its label, and the second reader does not then confirm it.

`paragraph SNAPSHOT SHA256 LABEL --context N` also returns up to N (1 to 5) neighbouring paragraphs on each side of every candidate, in reading order, under the candidate's `context` (`before`, `after` and its own `ordinal`). Each side stops where the label series changes or restarts, so context does not run from the main text into an appendix or into reproduced text later in the PDF. Neighbours carry the same spans, hashes and review flags as a lookup, but the second reader does not check them: look one up to confirm it. A condition in an adjacent paragraph then arrives with the paragraph that depends on it.

`build` stores every anchored paragraph with the candidate lookup reports for it, so lookups read the table instead of re-scanning pages. `verify` re-derives every document's paragraphs from its indexed pages and fails if any stored column differs; an index built before the current anchoring fails and needs a rebuild. An index without the table (schema 1 or 2) still answers `paragraph` from its pages and refuses `--context`. On the `2026-10-15` sources the table holds 26,405 paragraphs. When it was added it held 26,051 and the index grew from 43 MB to 125 MB; 1,950 sampled labels returned identical output from the table and from the pages.

### Facts derived at build

From schema 4, `build` also records 3 facts from the snapshot's own sources, and `paragraph` returns them. Where a source does not state a fact, it stays empty; nothing is inferred.

- `cited_by`: pages of other standards' documents whose text cites this paragraph as `paragraph 26 of AASB 16`, `paragraphs 26, 27 and Aus27.1 of AASB 16` or `paragraphs B19–B21 of AASB 3`, grouped by the citing document's kind from its catalogue rows: `interpretations`, `agenda_decisions`, `amending_standards`, `standards` and `other`, with a `total` and at most 20 of each kind. A range cites only its endpoints and `35(a)` cites `35`. A citation names a standard, not a version, so it links to the label in every captured version; check the numbering in the version you cite. Versions of the same standard are left out. The `2026-10-15` snapshot holds no Interpretation or agenda decision document, so those 2 lists are empty there; a snapshot that adds them fills them on rebuild.
- `reporting_tier`: the tier a catalogue title states (`Tier 2 Entities` or a closing `Tier 2 Disclosures`, and likewise Tier 3), with that title as `basis`. Titles that state no tier, or a mixed one such as `Tier 2 and Other`, leave it null. The standards' appendices on simplified disclosures hold almost no numbered paragraphs, so the tier is recorded per document, not per paragraph.
- `validity`: the mandatory periods the catalogue's `applies_to_periods_text` states (`applies_from`, `applies_before`, `periods_ending_before`, `periods_ending_on_or_after`) and a `status` at the catalogue's `retrieved_date` (`as_at`): `not_yet_effective` when no period may yet begin, `superseded` once a period beginning that day is no longer covered, otherwise `current`. A 'but before' bounds the clause it follows, so after 'that end on or after' it sets `periods_ending_before`. An early-application sentence is not read. The status is null when a row states a date in a form not read, or when rows for one PDF give different statuses; the dates are null when those rows state different periods. `superseded_by` lists versions of the same standard whose periods begin on this version's `applies_before`. It restates catalogue text; it does not decide which version applies to a reporting period.

On the `2026-10-15` snapshot with the 21 November catalogue correction, the build found 4,285 citations of 56 standards (3,920 from standards, 361 from amending standards and 4 from other documents). Of the 190 documents, 71 were `current`, 103 `superseded`, 11 `not_yet_effective` and 5 without a status. 8 AASB 1060 documents state Tier 2 and 1 AASB 1061 document states Tier 3.

An index before schema 4 omits these fields. From schema 4, `verify` also re-derives the citations from the indexed pages and the facts from `catalogue.json`, and fails if either differs.

`page SNAPSHOT SHA256 PAGE` returns a full extracted page.

### AASB S2 disclosure checklist

`checklist SNAPSHOT SHA256` lists the disclosure requirements of one AASB S2 PDF from its stored paragraphs. A requirement is a paragraph outside the Basis for Conclusions, illustrative examples and implementation guidance (labels `BC`, `IE`, `IG`) and the deleted IFRS S2 text, which is not part of AASB S2, whose text says an entity 'shall disclose' or 'shall also disclose', ignoring case, emphasis and line breaks. Each carries an `id` (`AASB S2|<compilation>|<label>`), the compilation's version label, the paragraph label and its PDF page, never the standard's text. It needs review when its stored paragraph does or when its label is stored more than once. The rule keys a requirement to its paragraph: lettered clauses stay inside it, and a requirement worded otherwise is not listed. `unlisted_pages` names each page outside the deleted text where those words occur more often than in the listed requirements and `BC`, `IE` and `IG` paragraphs there, as when extraction loses a label or an appendix repeats one: read those pages, as their requirements are not listed. On the schema 4 snapshot with the 21 November catalogue, the September 2024 version lists 34 requirements (17 need review) with unlisted pages 10, 11, 13, 14, 33, 34 and 39, and the December 2025 compilation 31 (11) with unlisted pages 23, 24, 26, 35 and 36.

`gap-check SNAPSHOT SHA256 RESULT` checks a filled-in gap analysis, shaped as the checklist's `result_schema`: the same `document_sha256` and one result for each requirement `id`. A `satisfied` result carries the exact `quote` from the report and its `location` (page or section); a `not_satisfied` one carries the `missing` element. Any other shape, a missing or repeated `id`, or an `id` not in the checklist is an error. Its result repeats the checklist's `unlisted_pages`, whose requirements it does not check. Neither command reads or judges a report: whether a quote is in the report and meets the requirement stays with the reader.

### Check a quotation

```powershell
& .\target\release\aasb-corpus.exe verify-quote $snapshot $version 12 'quoted words'
& .\target\release\aasb-corpus.exe verify-quote $snapshot $version 'quoted words'
```

`verify-quote SNAPSHOT SHA256 [PAGE] QUOTE` grades a case-sensitive quotation of up to 16,384 characters against the indexed page text; a longer one is refused before any page is read:

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

`mcp SNAPSHOT` serves 8 read-only tools over stdio, one JSON-RPC message a line: `search_aasb_pages`, `list_aasb_documents`, `read_aasb_paragraph`, `verify_aasb_quote`, `read_aasb_page`, `get_aasb_stats`, `list_aasb_s2_requirements` (the `checklist` command) and `check_aasb_gap_analysis` (`gap-check`, which takes the gap analysis as a `result` object instead of a file path). `build` and `verify` stay command-line only. It accepts the `initialize` handshake of MCP revisions 2025-11-25, 2025-06-18 and 2024-11-05, and refuses JSON-RPC batches, so it does not offer 2025-03-26, which requires them. Each takes the matching command's arguments by name (`document_sha256` for the PDF hash, `context` for `--context`, and `stem` and `paragraphs` as booleans) and returns the JSON that command prints, so the statuses and grades above apply unchanged. The snapshot is fixed at start-up and no tool takes a path; an argument a tool does not declare is refused. A message line over 1 MiB is refused with an `invalid request` error and read no further than that; the server then serves the next line. Once serving, the server writes nothing to stderr; a start-up failure, such as an unbuilt snapshot, is reported there before it exits. It makes no network request, but the MCP host sends tool results, page text included, to its model.

Other repositories can invoke this executable or read the SQLite database directly. Search connections are read-only. No consumer repository changes are needed to use the shared snapshot.

## Rebuild a new snapshot

Copy the authorised PDFs, download manifest and catalogue into a new snapshot with the same layout. Extraction refuses an existing output directory:

```powershell
& 'C:\Tools\pdf-inspector\.venv\Scripts\python.exe' -X utf8 tools/extract.py 'C:\path\to\new-snapshot'
& 'C:\Tools\pdf-inspector\.venv\Scripts\python.exe' -X utf8 tools/second_reader.py 'C:\path\to\new-snapshot'
& .\target\release\aasb-corpus.exe build 'C:\path\to\new-snapshot'
```

On the Linux sandbox, use `agent-run pdf-inspector-python tools/extract.py SNAPSHOT`. The extractor verifies source hashes, checks every page index, uses offline OCR only where pdf-inspector requests it and retains review flags. It stages extraction before renaming the completed folder. Indexing checks input hashes and complete document/page coverage, builds and checks FTS5 in one transaction, then promotes a new index. Failed stages are retained for inspection and never replace a completed snapshot.

`tools/second_reader.py` stages its output and adds `second-reader/` to a snapshot; it refuses an existing output folder. Run it before `build`: the build places labels from it, so adding or changing it afterwards makes `verify` ask for a rebuild. A missing, unreadable or changed source file, a `page_count` in the source manifest that is not a positive integer or differs between entries with one SHA-256, or a source manifest that changes during the run, stops it with nothing promoted. Only a PDF whose verified bytes pypdf cannot read, or reads with a different page count, is listed under `failures` with its hash; its paragraphs report `found_unconfirmed`.

## Checks

```powershell
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
& 'C:\Tools\pdf-inspector\.venv\Scripts\python.exe' -X utf8 -m unittest discover -s tools -p 'test_*.py'
```

### Coverage gate

The repository holds 100% line coverage for the Rust and the Python it keeps, with no excluded line. Run both commands before a change merges; each fails if one line goes unrun. Install [cargo-llvm-cov](https://github.com/taiki-e/cargo-llvm-cov) and `coverage` once.

```powershell
cargo llvm-cov --locked --lcov --output-path lcov.info
if (Select-String -Path lcov.info -Pattern '^DA:\d+,0$' -Quiet) { throw 'Rust lines are uncovered' }
python -X utf8 -m coverage run --branch --source=tools -m unittest discover -s tools -p 'test_*.py'
python -X utf8 -m coverage report --fail-under=100 -m
```

On Linux, `! grep -E '^DA:[0-9]+,0$' lcov.info` replaces the `Select-String` line. The Python tests replace pdf-inspector with a fabricated module and need only `pypdf` besides `coverage`. `lcov.info` counts a line as unrun when no test reaches it; `cargo llvm-cov`'s own percentage also counts each closure that never runs on an otherwise covered line, so it can read a little under 100%.

The optional live-index check traverses filtered results and checks actual paragraph fragments against indexed page slices. On Linux, set `AASB_TEST_SNAPSHOT` to the local snapshot and run:

```sh
AASB_TEST_SNAPSHOT=/path/to/snapshot cargo test --locked --test local_snapshot -- --ignored --nocapture
```

Ordinary tests use fabricated text. The live check stays local and prints counts and statuses only.

The same command also runs the reference questions in `tests/fixtures/reference-questions.tsv`: for each question it looks up the expected paragraph in every version of the standard, then counts how often exact and stemmed search put that paragraph's page in the top 5 and top 20, and how often paragraph search returns its label there. An index without the stem table counts exact page search only; one without the paragraph table skips paragraph search. The counts measure this question set, not general recall, and the file holds only short queries and paragraph labels.

On 13 November 2026 the `2026-10-15` snapshot (`index_sha256` `114641a50b25524d217fcbd143b6b36bd03472f80d5b6d29f321b152965f88dc`) gave these counts for the 28 questions. The 81 paragraph lookups, one for each version of each question's standard, were 46 `found`, 6 `ambiguous`, 22 `needs_review` and 7 `not_found`. 74 of them located a page, and exact search put that page in the top 5 for 46 and in the top 20 for 56. Stemmed search was not counted: that index predates the stem table.

On 24 November 2026 the same snapshot rebuilt at schema 3 (`index_sha256` `66d6efb478a9f3ad02fce6190ec9b138998a499bcc65aa189136691095a47a40`) gave the same lookup statuses and exact page counts. Stemmed page search put the page in the top 5 for 68 and the top 20 for 72; paragraph search returned the label in the top 5 for 39 and the top 20 for 42, or 59 and 67 stemmed.

The fixture then gained 3 AASB S2 questions: its scope, its effective date and the year an entity must first report, which the Corporations Act sets (the fixture's comments cite the sections read). With the 21 November catalogue at schema 4 (`index_sha256` `b03d12cd758138f8a9a5916164fd201acb989e26045affbb9b1e4e56156cdcda`), the 31 questions' 87 lookups were 49 `found`, 8 `ambiguous`, 22 `needs_review` and 8 `not_found`. 79 located a page: exact page search put it in the top 5 for 51 and the top 20 for 61, stemmed for 73 and 77; paragraph search returned the label for 44 and 47, stemmed for 64 and 72. In both AASB S2 versions, label `3` was ambiguous because the copyright notice's numbered clauses also anchored as paragraphs. In the December 2025 compilation, `AusC1.1` was `not_found` because extraction puts it on the same line as the deleted `C1`.

Lookup then skipped notice pages and anchored labels after a deleted marker. Rebuilt on 24 November 2026 at schema 4 (`index_sha256` `582885e21fdf9794f1b329fe7c4bc8c6c4801f455b643fb3113b8b5dec817dc9` with the 21 November catalogue; `7a6f80073023f959564294ec96218ef5c3e9409d944605548d765b3611ec6024` with the original catalogue), the 87 lookups were 51 `found`, 7 `ambiguous`, 22 `needs_review` and 7 `not_found`. 80 located a page: exact page search put it in the top 5 for 52 and the top 20 for 62, stemmed for 74 and 78; paragraph search returned the label for 45 and 48, stemmed for 65 and 73. AASB S2 `3` is now `found` in the September 2024 version and `AusC1.1` in both. In the December 2025 compilation `3` stays `ambiguous`: a numbered footnote in its Basis for Conclusions also anchors as `3`.

Lookup then anchored in-sequence labels inside a line, skipped numbered footnotes at a page foot and kept the search text of candidates over 64 KiB (none in this snapshot). Rebuilt on 24 November 2026 at schema 4 (`index_sha256` `d71dd12b1af3f393d59ce16ba11b6b344dc6108009c2426065aa4c8923296f7d` with the 21 November catalogue; `97b8aafec3948dedf7e7cfffef401d2eb7087722bbe5c411017e6fee484ccfdd` with the original catalogue; each previous index stays in its snapshot as `index-582885e2.sqlite3` or `index-7a6f8007.sqlite3`), the table holds 26,216 paragraphs instead of 26,051, and 71 candidates instead of 657 carry the inside-line review reason. The 87 lookups were 53 `found`, 6 `ambiguous`, 23 `needs_review` and 5 `not_found`. 82 located a page: exact page search put it in the top 5 for 54 and the top 20 for 64, stemmed for 76 and 80; paragraph search returned the label for 47 and 50, stemmed for 67 and 75. Across every stored label, 17,972 lookups are `found` instead of 16,720; the only label found before and not after is `33` in the June and December 2022 AASB 1057 compilations, a Basis for Conclusions footnote. In the December 2025 AASB S2 compilation `C2` and `C4` are now `found`, and `3` is no longer ambiguous but needs review: extraction puts paragraph 4 in a table row, so no adjacent label confirms its ending.

Lookup then anchored labels in a table row's first cell and kept an amending standard's instruction that an underlined inserted paragraph follows. Rebuilt on 24 November 2026 at schema 4 (`index_sha256` `4b14412983a3bf526f40e1a0326b1c49c75ce1dbf97485baebb826dae3e50095` with the 21 November catalogue; `f38e245af2ddb5697b84a6c7d978f8799cadedc20db9437a5cbdb9659008f557` with the original catalogue; each previous index stays as `index-d71dd12b.sqlite3` or `index-97b8aafe.sqlite3`), the table holds 26,290 paragraphs instead of 26,216 and no anchor was removed. The 87 lookups and search counts are unchanged. Across every stored label, 18,016 lookups are `found` instead of 17,972, and every label found before is still found. In the December 2025 AASB S2 compilation `4` is now `found`, and `3` now ends at `4` but still needs review, as its own clauses are extracted as garbled table rows.

Lookup then ended a sentence at closing bold or a footnote marker, as in `entity.¹** 2 This`, and stopped page-foot footnotes counting as label starts. Rebuilt on 24 November 2026 at schema 4 (`index_sha256` `1dd3cce969731356638fad1c010ed18980ad100413d05f4dd342b26d66c75aa2` with the 21 November catalogue; `275e7a71744280dcf9be27a0e732def4f79b939f61b2f567b5abae75cb033a0b` with the original catalogue; each previous index stays as `index-4b144129.sqlite3` or `index-f38e245a.sqlite3`), the table holds 26,405 paragraphs instead of 26,290, no anchor was removed and every label found before is still found; 18,183 stored labels are `found` instead of 18,016. The 87 lookups were 59 `found`, 6 `ambiguous`, 17 `needs_review` and 5 `not_found`; 82 located a page, and page search counts are unchanged. AASB 137 paragraph `66` now ends with its own text instead of running on into `67` and `68`. Its text says 'contract', so exact search for `onerous contracts` had found it only through `67`'s words; the question now expects `67`. Paragraph search returns the label for 47 and 50, stemmed for 67 and 75; with the old label exact paragraph search gave 43 and 46.

## Reuse and licence

Quotation normalisation and regression cases adapt `fadden/as_at.py`; page completeness adapts the invariant in `fadden/extract.py`; staging follows the tax corpus's complete-output-before-promotion pattern. Upstream is [PaulGoodman](https://github.com/ryanduguid/PaulGoodman), formerly au-tax-legislation-corpus, at `84db0481a924f07d3f697f54a269917475613184`. Its tax date model, Register EPUB parser, attribution assumptions and publication filters are not used for AASB documents.

Code is Apache-2.0; retain LICENSE and NOTICE. Source documents keep their own copyright notices and rights. This local build does not publish the corpus.

---

<p align="center"><sub><i>Congratulations, you just advanced a Standards level.</i><br>Made in Newcastle, Australia by <a href="https://duguid.com.au/">Ryan Duguid</a>.</sub></p>
