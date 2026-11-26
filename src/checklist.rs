//! An AASB S2 disclosure checklist derived from the stored paragraph table, and a check that a
//! filled-in gap analysis answers every requirement in the agreed shape. Neither reads or
//! judges a report, and the checklist carries labels and pages, never the standard's text.

use crate::{Result, aliases_metadata, require_paragraph_table, valid_sha};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// The documented rule a stored paragraph must meet to become a requirement.
const RULE: &str = "A requirement is a paragraph outside the Basis for Conclusions, illustrative examples and implementation guidance (labels BC, IE, IG) and the deleted IFRS S2 text, whose text says that an entity 'shall disclose' or 'shall also disclose', ignoring case, emphasis and line breaks. One requirement per label; a label stored more than once in the PDF needs review. unlisted_pages are pages whose text says so outside every listed requirement and BC, IE or IG paragraph, as where extraction lost a label: read them, as their requirements are not listed.";

/// A stored paragraph: its label, physical page, review flag, search text and candidate.
type Row = (String, u32, bool, String, Value);

/// The compilation's version label when `metadata` describes AASB S2.
fn s2_compilation(metadata: &str) -> Option<String> {
    let entries: Vec<Value> = serde_json::from_str(metadata).ok()?;
    entries
        .iter()
        .find(|entry| entry["standard"] == "AASB S2")?["version_label"]
        .as_str()
        .map(str::to_owned)
}

/// Basis for Conclusions, illustrative examples and implementation guidance.
fn accompanying(label: &str) -> bool {
    let prefix: String = label
        .strip_prefix("Aus")
        .unwrap_or(label)
        .chars()
        .take_while(char::is_ascii_uppercase)
        .collect();
    matches!(prefix.as_str(), "BC" | "IE" | "IG")
}

/// How often `text` says 'shall disclose' or 'shall also disclose'.
fn disclosures(text: &str) -> usize {
    let words = text
        .replace(['*', '_'], "")
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    words.matches("shall disclose").count() + words.matches("shall also disclose").count()
}

/// A page of the deleted IFRS S2 text, which is not part of AASB S2: its heading or running
/// footer (`AASB S2-compiled DELETED IFRS S2 TEXT`), not a contents entry.
fn deleted_ifrs_text(page: &str) -> bool {
    page.lines().any(|line| {
        let plain = line.trim().trim_matches(['#', '*', '_', ' ']);
        plain.eq_ignore_ascii_case("Deleted IFRS S2 text")
            || (plain.starts_with("AASB S2") && plain.ends_with("DELETED IFRS S2 TEXT"))
    })
}

/// The indices of the rows that are requirements, in reading order.
fn listed(rows: &[Row], deleted: &BTreeSet<u32>) -> Vec<usize> {
    let mut seen = BTreeSet::new();
    (0..rows.len())
        .filter(|&i| {
            let (label, page, _, text, _) = &rows[i];
            !accompanying(label)
                && !deleted.contains(page)
                && disclosures(text) > 0
                && seen.insert(label)
        })
        .collect()
}

/// The requirements among `rows`, in reading order.
fn requirements(compilation: &str, rows: &[Row], listed: &[usize]) -> Vec<Value> {
    let mut stored = BTreeMap::<&str, usize>::new();
    for (label, ..) in rows {
        *stored.entry(label.as_str()).or_default() += 1;
    }
    listed
        .iter()
        .map(|&i| {
            let (label, page, review, ..) = &rows[i];
            json!({"id":format!("AASB S2|{compilation}|{label}"),"standard":"AASB S2",
                "compilation":compilation,"paragraph":label,"pdf_page":page,
                "needs_review":*review || stored[label.as_str()] > 1})
        })
        .collect()
}

/// Pages outside the deleted IFRS S2 text that say an entity shall disclose more often than
/// the listed requirements and accompanying paragraphs hold there.
fn unlisted_pages(
    pages: &[(u32, String)],
    rows: &[Row],
    listed: &[usize],
    deleted: &BTreeSet<u32>,
) -> Vec<u32> {
    let mut held = BTreeMap::<u64, usize>::new();
    for (i, (label, _, _, _, candidate)) in rows.iter().enumerate() {
        if accompanying(label) || listed.contains(&i) {
            for fragment in candidate["fragments"].as_array().into_iter().flatten() {
                if let Some(page) = fragment["pdf_page"].as_u64() {
                    *held.entry(page).or_default() +=
                        disclosures(fragment["text"].as_str().unwrap_or(""));
                }
            }
        }
    }
    pages
        .iter()
        .filter(|(page, text)| {
            !deleted.contains(page)
                && disclosures(text) > held.get(&u64::from(*page)).copied().unwrap_or(0)
        })
        .map(|(page, _)| *page)
        .collect()
}

/// Every way `result` departs from the gap-analysis schema for `checklist`.
fn problems(checklist: &Value, result: &Value) -> Vec<String> {
    let mut problems = Vec::new();
    if result["document_sha256"] != checklist["document_sha256"] {
        problems.push("document_sha256 differs from the checklist's document".to_owned());
    }
    let wanted: BTreeSet<&str> = checklist["requirements"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| r["id"].as_str())
        .collect();
    let filled = |entry: &Value, field: &str| {
        entry[field]
            .as_str()
            .is_some_and(|text| !text.trim().is_empty())
    };
    let mut answered = BTreeSet::new();
    for entry in result["results"].as_array().into_iter().flatten() {
        let id = entry["id"].as_str().unwrap_or("(no id)");
        if !wanted.contains(id) {
            problems.push(format!("{id} is not in the checklist"));
        }
        if !answered.insert(id) {
            problems.push(format!("{id} is answered more than once"));
        }
        match entry["status"].as_str() {
            Some("satisfied") => {
                for (field, name) in [("quote", "a quote"), ("location", "a location")] {
                    if !filled(entry, field) {
                        problems.push(format!("{id} is satisfied without {name}"));
                    }
                }
            }
            Some("not_satisfied") if !filled(entry, "missing") => {
                problems.push(format!("{id} is not satisfied without the missing element"));
            }
            Some("not_satisfied") => {}
            other => problems.push(format!("{id} has status {}", other.unwrap_or("(none)"))),
        }
    }
    problems.extend(
        wanted
            .difference(&answered)
            .map(|id| format!("{id} is not answered")),
    );
    problems
}

fn stored_rows(connection: &Connection, sha: &str) -> Result<Vec<Row>> {
    let mut statement = connection.prepare(
        "SELECT label,pdf_page,needs_review,text,candidate FROM paragraphs WHERE document_sha256=?1 ORDER BY ordinal",
    )?;
    let rows = statement
        .query_map([sha], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get::<_, String>(4)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    rows.into_iter()
        .map(|(label, page, review, text, candidate)| {
            Ok((label, page, review, text, serde_json::from_str(&candidate)?))
        })
        .collect()
}

fn stored_pages(connection: &Connection, sha: &str) -> Result<Vec<(u32, String)>> {
    let mut statement =
        connection.prepare("SELECT page,text FROM pages WHERE document_sha256=?1 ORDER BY page")?;
    let pages = statement
        .query_map([sha], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<std::result::Result<_, _>>()?;
    Ok(pages)
}

/// The checklist for one AASB S2 PDF in the snapshot.
pub(crate) fn checklist(connection: &Connection, sha: &str) -> Result<Value> {
    valid_sha(sha)?;
    require_paragraph_table(connection)?;
    let (_, metadata) = aliases_metadata(connection, sha)?;
    let compilation = s2_compilation(&metadata).ok_or("the document is not AASB S2")?;
    let (rows, pages) = (
        stored_rows(connection, sha)?,
        stored_pages(connection, sha)?,
    );
    let deleted = pages
        .iter()
        .filter(|(_, text)| deleted_ifrs_text(text))
        .map(|(page, _)| *page)
        .collect();
    let listed = listed(&rows, &deleted);
    let requirements = requirements(&compilation, &rows, &listed);
    Ok(
        json!({"standard":"AASB S2","compilation":compilation,"document_sha256":sha,"rule":RULE,
        "requirement_count":requirements.len(),"requirements":requirements,
        "unlisted_pages":unlisted_pages(&pages, &rows, &listed, &deleted),
        "result_schema":{"document_sha256":sha,"results":[
            {"id":"one requirement id","status":"satisfied","quote":"exact words from the report","location":"its page or section in the report"},
            {"id":"one requirement id","status":"not_satisfied","missing":"the element the report lacks"}]}}),
    )
}

/// Checks the filled-in gap analysis in the file at `path` against the checklist for `sha`.
pub(crate) fn gap_check(connection: &Connection, sha: &str, path: &str) -> Result<Value> {
    let result: Value = serde_json::from_str(&std::fs::read_to_string(path)?)?;
    gap_check_value(connection, sha, &result)
}

/// Checks a filled-in gap analysis against the checklist for `sha`.
pub(crate) fn gap_check_value(connection: &Connection, sha: &str, result: &Value) -> Result<Value> {
    let checklist = checklist(connection, sha)?;
    let problems = problems(&checklist, result);
    if !problems.is_empty() {
        let shown: Vec<_> = problems.iter().take(20).cloned().collect();
        return Err(format!(
            "the gap analysis does not match the checklist ({} problems): {}",
            problems.len(),
            shown.join("; ")
        )
        .into());
    }
    let satisfied = result["results"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|entry| entry["status"] == "satisfied")
        .count();
    let count = checklist["requirement_count"].as_u64().unwrap_or(0);
    Ok(
        json!({"valid":true,"requirement_count":count,"satisfied":satisfied,
        "not_satisfied":count.saturating_sub(satisfied as u64),
        "unlisted_pages":checklist["unlisted_pages"],
        "boundary":"Valid means every listed requirement is answered in the agreed shape. Whether a quote is in the report and meets the requirement is not checked, nor are requirements on unlisted_pages."}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(label: &str, page: u32, text: &str) -> Row {
        let candidate = json!({"fragments":[{"pdf_page":page,"text":text}]});
        (label.into(), page, false, text.into(), candidate)
    }

    fn all_requirements(compilation: &str, rows: &[Row]) -> Vec<Value> {
        requirements(compilation, rows, &listed(rows, &BTreeSet::new()))
    }

    fn sample() -> Vec<Row> {
        vec![
            row(
                "1",
                7,
                "1 The fabricated objective is to require useful information.",
            ),
            row(
                "13",
                9,
                "13 An entity shall disclose fabricated information about:",
            ),
            row(
                "14",
                9,
                "14 An entity **shall\ndisclose** fabricated targets.",
            ),
            row(
                "Aus14.1",
                9,
                "Aus14.1 A fabricated entity Shall Also Disclose its basis.",
            ),
            row(
                "B5",
                20,
                "B5 The fabricated guidance explains paragraph 13.",
            ),
            row(
                "B6",
                20,
                "B6 An entity shall disclose the fabricated assumptions.",
            ),
            row(
                "BC40",
                40,
                "BC40 The Board decided an entity shall disclose fabricated plans.",
            ),
            row(
                "IE3",
                50,
                "IE3 Fabricated Entity A shall disclose its fabricated plan.",
            ),
            row("13", 61, "13 A fabricated footnote anchored as 13."),
        ]
    }

    #[test]
    fn requirements_are_paragraphs_that_state_a_disclosure() {
        let found = all_requirements("Fabricated compilation, 12/25", &sample());
        let labels: Vec<_> = found.iter().map(|r| r["paragraph"].clone()).collect();
        assert_eq!(labels, ["13", "14", "Aus14.1", "B6"]);
        assert_eq!(found[0]["id"], "AASB S2|Fabricated compilation, 12/25|13");
        assert_eq!(found[0]["compilation"], "Fabricated compilation, 12/25");
        assert_eq!(found[0]["pdf_page"], 9);
        // A label stored twice cannot be keyed without review.
        assert_eq!(found[0]["needs_review"], true);
        assert_eq!(found[1]["needs_review"], false);
        // No requirement carries the standard's text.
        assert!(found.iter().all(|r| r.get("text").is_none()));
    }

    /// The shape of the deleted text's first page.
    const DELETED_PAGE: &str = "## Deleted IFRS S2 text\n*Deleted IFRS S2 text is not part of AASB S2.*\n32 An entity shall disclose fabricated industry metrics.\n**AASB S2-compiled DELETED IFRS S2 TEXT**";

    #[test]
    fn deleted_ifrs_text_pages_carry_its_heading_or_footer() {
        assert!(deleted_ifrs_text(DELETED_PAGE));
        assert!(deleted_ifrs_text("text\n## AASB S2 DELETED IFRS S2 TEXT"));
        // A contents entry naming the section is not the section.
        assert!(!deleted_ifrs_text(
            "**COMPILATION DETAILS** **DELETED IFRS S2 TEXT**"
        ));
    }

    #[test]
    fn deleted_ifrs_text_is_not_listed_and_unlisted_pages_are_named() {
        let connection = Connection::open_in_memory().unwrap();
        crate::schema(&connection).unwrap();
        let sha = "a".repeat(64);
        let metadata = json!([{"standard":"AASB S2","version_label":"Fabricated, 12/25"}]);
        connection
            .execute(
                "INSERT INTO documents VALUES(?1,5,'[]',?2,?1)",
                rusqlite::params![sha, metadata.to_string()],
            )
            .unwrap();
        for (page, text) in [
            (9, "13 An entity shall disclose fabricated risks."),
            // Extraction lost the label of the second requirement on this page.
            (
                11,
                "21 Fabricated text.\n# Fabricated heading\nAn entity shall disclose fabricated resilience.",
            ),
            // An appendix reuses label 13 for another requirement.
            (34, "13 An entity shall disclose fabricated targets."),
            (40, "BC40 The Board decided an entity shall disclose plans."),
            (49, DELETED_PAGE),
        ] {
            connection
                .execute(
                    "INSERT INTO pages(document_sha256,page,text,text_sha256,provenance,needs_review)
                    VALUES(?1,?2,?3,?4,'native',0)",
                    rusqlite::params![sha, page, text, crate::hash(text.as_bytes())],
                )
                .unwrap();
        }
        let pages = crate::document_pages(&connection, &sha).unwrap();
        crate::insert_paragraphs(&connection, &sha, &pages, None).unwrap();
        let found = checklist(&connection, &sha).unwrap();
        let labels: Vec<_> = found["requirements"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| (r["paragraph"].clone(), r["pdf_page"].clone()))
            .collect();
        assert_eq!(labels, [(json!("13"), json!(9))]);
        assert_eq!(found["unlisted_pages"], json!([11, 34]));
    }

    #[test]
    fn only_aasb_s2_metadata_names_a_compilation() {
        let s2 =
            r#"[{"standard":"AASB S2","version_label":"AASB S2 – Compilation – December 2025"}]"#;
        assert_eq!(
            s2_compilation(s2).as_deref(),
            Some("AASB S2 – Compilation – December 2025")
        );
        assert_eq!(
            s2_compilation(r#"[{"standard":"AASB S1","version_label":"S1"}]"#),
            None
        );
        assert_eq!(s2_compilation("not json"), None);
    }

    #[test]
    fn a_gap_analysis_must_answer_every_requirement_in_shape() {
        let requirements = all_requirements("C", &sample());
        let checklist = json!({"document_sha256":"ab","requirements":requirements});
        let good = json!({"document_sha256":"ab","results":[
            {"id":"AASB S2|C|13","status":"satisfied","quote":"We disclose our fabricated risks.","location":"page 12"},
            {"id":"AASB S2|C|14","status":"not_satisfied","missing":"No fabricated targets are stated."},
            {"id":"AASB S2|C|Aus14.1","status":"not_satisfied","missing":"No basis is given."},
            {"id":"AASB S2|C|B6","status":"satisfied","quote":"Assumptions are listed.","location":"Section 4.2"}]});
        assert_eq!(problems(&checklist, &good), Vec::<String>::new());
        let cases = [
            (json!({"document_sha256":"cd"}), "document"),
            (
                json!({"document_sha256":"ab","results":[]}),
                "AASB S2|C|13 is not answered",
            ),
            (
                json!({"document_sha256":"ab","results":[{"id":"AASB S2|C|99","status":"satisfied","quote":"q","location":"p 1"}]}),
                "AASB S2|C|99 is not in the checklist",
            ),
            (
                json!({"document_sha256":"ab","results":[{"id":"AASB S2|C|13","status":"satisfied","location":"p 1"}]}),
                "AASB S2|C|13 is satisfied without a quote",
            ),
            (
                json!({"document_sha256":"ab","results":[{"id":"AASB S2|C|13","status":"satisfied","quote":"q","location":" "}]}),
                "AASB S2|C|13 is satisfied without a location",
            ),
            (
                json!({"document_sha256":"ab","results":[{"id":"AASB S2|C|13","status":"not_satisfied"}]}),
                "AASB S2|C|13 is not satisfied without the missing element",
            ),
            (
                json!({"document_sha256":"ab","results":[{"id":"AASB S2|C|13","status":"partly"}]}),
                "AASB S2|C|13 has status partly",
            ),
            (
                json!({"document_sha256":"ab","results":[
                    {"id":"AASB S2|C|13","status":"not_satisfied","missing":"m"},
                    {"id":"AASB S2|C|13","status":"not_satisfied","missing":"m"}]}),
                "AASB S2|C|13 is answered more than once",
            ),
        ];
        for (result, expected) in cases {
            let found = problems(&checklist, &result);
            assert!(
                found.iter().any(|p| p.contains(expected)),
                "{expected}: {found:?}"
            );
        }
    }
}
