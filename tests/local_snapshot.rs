use serde_json::Value;
use std::collections::BTreeSet;
use std::process::Command;

fn call(root: &str, command: &str, arguments: &[&str]) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_aasb-corpus"))
        .arg(command)
        .arg(root)
        .args(arguments)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

/// Every (document, page) a paginated AASB 15 search returns, each exactly once.
fn paginated_aasb15_pages(root: &str) -> BTreeSet<(String, u64)> {
    let mut all = BTreeSet::new();
    let mut offset = 0;
    loop {
        let found = call(
            root,
            "search",
            &[
                "\"performance obligation\"",
                "--standard",
                "AASB 15",
                "--limit",
                "7",
                "--offset",
                &offset.to_string(),
            ],
        );
        for hit in found["results"].as_array().unwrap() {
            assert!(
                hit["version_metadata"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|m| m["standard"] == "AASB 15")
            );
            assert!(all.insert((
                hit["document_sha256"].as_str().unwrap().to_owned(),
                hit["pdf_page"].as_u64().unwrap()
            )));
        }
        if let Some(next) = found["next_offset"].as_u64() {
            assert!(next > offset);
            offset = next;
        } else {
            assert_eq!(found["has_more"], false);
            break;
        }
    }
    all
}

/// A fragment is an unchanged slice of its page; a found one verifies as a quotation.
fn check_fragment(root: &str, sha: &str, fragment: &Value, found: bool) {
    let page = call(root, "page", &[sha, &fragment["pdf_page"].to_string()]);
    let text = page["text"].as_str().unwrap();
    let start = fragment["start_line"].as_u64().unwrap() as usize - 1;
    let end = fragment["end_line"].as_u64().unwrap() as usize;
    assert_eq!(
        fragment["text"],
        text.lines()
            .skip(start)
            .take(end - start)
            .collect::<Vec<_>>()
            .join("\n")
    );
    if found {
        let line = fragment["text"].as_str().unwrap().lines().next().unwrap();
        let pdf_page = fragment["pdf_page"].to_string();
        let checked = call(root, "verify-quote", &[sha, &pdf_page, line]);
        assert_eq!(checked["verified"], true);
        assert_eq!(checked["page_text_sha256"], fragment["page_text_sha256"]);
    }
}

/// Filtered search, paragraph lookup and fragment checks for one version; returns its status.
fn check_paragraph_version(
    root: &str,
    standard: &str,
    label: &str,
    document: &Value,
    spans: &mut usize,
) -> String {
    let sha = document["document_sha256"].as_str().unwrap();
    let filtered = call(
        root,
        "search",
        &[
            "the",
            "--standard",
            standard,
            "--version",
            sha,
            "--limit",
            "1",
        ],
    );
    assert_eq!(filtered["results"][0]["document_sha256"], sha);
    let found = call(root, "paragraph", &[sha, label]);
    println!(
        "{standard} {label} {}: status={} next={} reasons={}",
        &sha[..8],
        found["status"],
        found["candidates"][0]["next_paragraph"],
        found["candidates"][0]["review_reasons"]
    );
    assert!(
        found["candidate_count"].as_u64().unwrap() > 0,
        "missing {standard} {label} in {sha}"
    );
    assert_eq!(found["document_sha256"], sha);
    assert_eq!(found["sources"], document["sources"]);
    assert_eq!(found["version_metadata"], document["version_metadata"]);
    for candidate in found["candidates"].as_array().unwrap() {
        for fragment in candidate["fragments"].as_array().unwrap() {
            check_fragment(root, sha, fragment, found["status"] == "found");
            *spans += 1;
        }
    }
    found["status"].as_str().unwrap().to_owned()
}

#[test]
#[ignore = "requires AASB_TEST_SNAPSHOT pointing to a local captured index"]
fn filters_pagination_and_paragraph_spans_on_the_local_snapshot() {
    let root = std::env::var("AASB_TEST_SNAPSHOT").expect("set AASB_TEST_SNAPSHOT");
    let all = paginated_aasb15_pages(&root);
    assert!(!all.is_empty());
    let mut paragraphs = 0;
    let mut spans = 0;
    let mut statuses = std::collections::BTreeMap::<String, usize>::new();
    for (standard, label) in [("AASB 15", "B19"), ("AASB 16", "26"), ("AASB 1060", "157")] {
        let catalogue = call(
            &root,
            "catalogue",
            &["--standard", standard, "--limit", "100"],
        );
        let documents = catalogue["results"].as_array().unwrap();
        assert!(!documents.is_empty());
        for document in documents {
            let status = check_paragraph_version(&root, standard, label, document, &mut spans);
            *statuses.entry(status).or_default() += 1;
            paragraphs += 1;
        }
    }
    assert!(
        statuses.get("found").copied().unwrap_or(0) > 0,
        "no confirmed paragraphs; run tools/second_reader.py on the snapshot"
    );
    println!(
        "local snapshot: {} distinct paginated AASB 15 pages; {paragraphs} paragraph lookups; {spans} exact page spans; statuses={statuses:?}",
        all.len()
    );
}

/// Search modes as `(stemmed, paragraphs)`: exact page search always, stemmed search when the
/// index has the stem table, and both paragraph searches when it has the paragraph table.
fn search_modes(root: &str) -> Vec<(bool, bool)> {
    let index = rusqlite::Connection::open_with_flags(
        std::path::Path::new(root).join("index.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let version: i64 = index
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    let mut modes = vec![(false, false)];
    if version >= 2 {
        modes.push((true, false));
    }
    if version >= 3 {
        modes.extend([(false, true), (true, true)]);
    }
    modes
}

fn fragment_pages(found: &Value) -> BTreeSet<u64> {
    found["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|candidate| candidate["fragments"].as_array().unwrap())
        .map(|fragment| fragment["pdf_page"].as_u64().unwrap())
        .collect()
}

/// Whether a search of document `sha` has the expected paragraph in its first 5 and 20
/// results: one of its `pages` for page search, its `label` for paragraph search.
fn ranked(
    root: &str,
    query: &str,
    sha: &str,
    (pages, label): (&BTreeSet<u64>, &str),
    (stem, paragraphs): (bool, bool),
) -> [bool; 2] {
    let mut arguments = vec![query, "--version", sha, "--limit", "20"];
    if stem {
        arguments.push("--stem");
    }
    if paragraphs {
        arguments.push("--paragraphs");
    }
    let found = call(root, "search", &arguments);
    let hits: Vec<bool> = found["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|hit| {
            if paragraphs {
                hit["paragraph"] == label
            } else {
                pages.contains(&hit["pdf_page"].as_u64().unwrap())
            }
        })
        .collect();
    [5, 20].map(|limit| hits.iter().take(limit).any(|hit| *hit))
}

/// Counts only: the fixture holds short queries and labels, and nothing prints page text.
#[test]
#[ignore = "requires AASB_TEST_SNAPSHOT pointing to a local captured index"]
fn reference_questions_on_the_local_snapshot() {
    let root = std::env::var("AASB_TEST_SNAPSHOT").expect("set AASB_TEST_SNAPSHOT");
    let modes = search_modes(&root);
    let questions = include_str!("fixtures/reference-questions.tsv");
    let mut statuses = std::collections::BTreeMap::<String, usize>::new();
    let mut located = 0;
    let mut counts = vec![[0; 2]; modes.len()];
    for line in questions.lines().filter(|line| !line.starts_with('#')) {
        let [query, standard, label] = line.split('\t').collect::<Vec<_>>()[..] else {
            panic!("expected query, standard and paragraph: {line}");
        };
        let catalogue = call(
            &root,
            "catalogue",
            &["--standard", standard, "--limit", "100"],
        );
        let documents = catalogue["results"].as_array().unwrap();
        assert!(!documents.is_empty(), "no document for {standard}");
        for document in documents {
            let sha = document["document_sha256"].as_str().unwrap();
            let found = call(&root, "paragraph", &[sha, label]);
            *statuses
                .entry(found["status"].as_str().unwrap().into())
                .or_default() += 1;
            let pages = fragment_pages(&found);
            if pages.is_empty() {
                continue;
            }
            located += 1;
            for (mode, count) in modes.iter().zip(&mut counts) {
                let hits = ranked(&root, query, sha, (&pages, label), *mode);
                for (rank, hit) in hits.iter().enumerate() {
                    count[rank] += usize::from(*hit);
                }
            }
        }
    }
    assert!(located > 0);
    let total = questions.lines().filter(|l| !l.starts_with('#')).count();
    println!(
        "reference questions: {total} questions; {located} located paragraph versions; statuses={statuses:?}"
    );
    for ((stem, paragraphs), [top5, top20]) in modes.iter().zip(counts) {
        let mode = match (stem, paragraphs) {
            (false, false) => "exact",
            (true, false) => "stemmed",
            (false, true) => "paragraphs exact",
            (true, true) => "paragraphs stemmed",
        };
        println!("{mode}: top5={top5} top20={top20}");
    }
}
