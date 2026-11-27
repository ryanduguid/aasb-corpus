//! Any bytes as one `pages.jsonl` line: parsing never panics, and a parsed page
//! serialises without error and parses back to the same fields.
#![no_main]

use aasb_corpus::Page;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(line) = std::str::from_utf8(data) else {
        return;
    };
    // As `insert_pages` in src/main.rs reads each line.
    let Ok(page) = serde_json::from_str::<Page>(line) else {
        return;
    };
    let written = serde_json::to_string(&page).expect("a parsed page serialises");
    let again: Page = serde_json::from_str(&written).expect("a serialised page parses");
    assert_eq!(again.page, page.page);
    assert_eq!(again.text, page.text);
    assert_eq!(again.provenance, page.provenance);
    assert_eq!(again.needs_review, page.needs_review);
});
