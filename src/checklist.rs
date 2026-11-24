//! An AASB S2 disclosure checklist derived from the stored paragraph table, and a check that a
//! filled-in gap analysis answers every requirement in the agreed shape. Neither reads or
//! judges a report, and the checklist carries labels and pages, never the standard's text.

use crate::{Result, aliases_metadata, require_paragraph_table, valid_sha};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// The documented rule a stored paragraph must meet to become a requirement.
const RULE: &str = "A requirement is a paragraph outside the Basis for Conclusions, illustrative examples and implementation guidance (labels BC, IE, IG) whose text says that an entity 'shall disclose' or 'shall also disclose', ignoring case, emphasis and line breaks. One requirement per label; a label stored more than once in the PDF needs review.";

/// A stored paragraph: its label, physical page, review flag and search text.
type Row = (String, u32, bool, String);

/// The compilation's version label when `metadata` describes AASB S2.
fn s2_compilation(metadata: &str) -> Option<String> {
    let entries: Vec<Value> = serde_json::from_str(metadata).ok()?;
    entries
        .iter()
        .find(|entry| entry["standard"] == "AASB S2")?["version_label"]
        .as_str()
        .map(str::to_owned)
}

fn states_a_disclosure(label: &str, text: &str) -> bool {
    let prefix: String = label
        .strip_prefix("Aus")
        .unwrap_or(label)
        .chars()
        .take_while(char::is_ascii_uppercase)
        .collect();
    let words = text
        .replace(['*', '_'], "")
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    !matches!(prefix.as_str(), "BC" | "IE" | "IG")
        && (words.contains("shall disclose") || words.contains("shall also disclose"))
}

/// The requirements among `rows`, in reading order.
fn requirements(compilation: &str, rows: &[Row]) -> Vec<Value> {
    let mut stored = BTreeMap::<&str, usize>::new();
    for (label, ..) in rows {
        *stored.entry(label.as_str()).or_default() += 1;
    }
    let mut seen = BTreeSet::new();
    rows.iter()
        .filter(|(label, _, _, text)| states_a_disclosure(label, text) && seen.insert(label))
        .map(|(label, page, review, _)| {
            json!({"id":format!("AASB S2|{compilation}|{label}"),"standard":"AASB S2",
                "compilation":compilation,"paragraph":label,"pdf_page":page,
                "needs_review":*review || stored[label.as_str()] > 1})
        })
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
        "SELECT label,pdf_page,needs_review,text FROM paragraphs WHERE document_sha256=?1 ORDER BY ordinal",
    )?;
    let rows = statement
        .query_map([sha], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<std::result::Result<_, _>>()?;
    Ok(rows)
}

/// The checklist for one AASB S2 PDF in the snapshot.
pub(crate) fn checklist(connection: &Connection, sha: &str) -> Result<Value> {
    valid_sha(sha)?;
    require_paragraph_table(connection)?;
    let (_, metadata) = aliases_metadata(connection, sha)?;
    let compilation = s2_compilation(&metadata).ok_or("the document is not AASB S2")?;
    let requirements = requirements(&compilation, &stored_rows(connection, sha)?);
    Ok(
        json!({"standard":"AASB S2","compilation":compilation,"document_sha256":sha,"rule":RULE,
        "requirement_count":requirements.len(),"requirements":requirements,
        "result_schema":{"document_sha256":sha,"results":[
            {"id":"one requirement id","status":"satisfied","quote":"exact words from the report","location":"its page or section in the report"},
            {"id":"one requirement id","status":"not_satisfied","missing":"the element the report lacks"}]}}),
    )
}

/// Checks the filled-in gap analysis in the file at `path` against the checklist for `sha`.
pub(crate) fn gap_check(connection: &Connection, sha: &str, path: &str) -> Result<Value> {
    let checklist = checklist(connection, sha)?;
    let result: Value = serde_json::from_str(&std::fs::read_to_string(path)?)?;
    let problems = problems(&checklist, &result);
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
        "boundary":"Valid means every requirement is answered in the agreed shape. Whether a quote is in the report and meets the requirement is not checked."}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(label: &str, page: u32, text: &str) -> Row {
        (label.into(), page, false, text.into())
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
        let found = requirements("Fabricated compilation, 12/25", &sample());
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
        let requirements = requirements("C", &sample());
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
