# Official source reader references

`public-references.json` pins AASB 116 compilation 6 and independent physical PDF
page/paragraph expectations. Source metadata came from the official standard
page and Federal Register; the latter supplies the pinned PDF. Paragraph labels
were also checked against its official EPUB HTML. The PDF has 23 physical pages.

Keep the PDF and extracted text outside redistributed test fixtures. Page 2
reserves IFRS Foundation copyright; the Register's general CC BY terms do not
override those specific rights. This fixture contains metadata and references
only. It makes no accounting applicability or completeness claim.

Prepare a new one-document snapshot with the listed PDF under `source/`,
`source/manifest.json` containing its filename, source URL, SHA256 and page count,
and `catalogue.json` with standard `AASB 116`, the compilation label and the exact
same URL as `download_url`. Use the installed project reader prerequisites:

```sh
python tools/extract.py /new/snapshot
python tools/second_reader.py /new/snapshot
cargo run -- build /new/snapshot
AASB_PUBLIC_SNAPSHOT=/new/snapshot cargo test --test public_references -- --ignored --nocapture
```

On Windows use the documented pdf-inspector Python environment for both reader
commands and set `$env:AASB_PUBLIC_SNAPSHOT` before Cargo. The first reader uses
pdf-inspector; the second deliberately uses the project's independent pypdf
reader for corroboration. The test reads the supplied built snapshot, verifies
its source bytes, checks all 5 paragraph/page references and requires actual
second-reader confirmation. Missing, unconfirmed or review-required results fail
qualification. No network request occurs in the test and no existing index is
rebuilt or overwritten. Review changed source bytes explicitly; do not replace
the pinned hash just to make a changed download pass.
