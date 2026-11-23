//! Facts the build derives from the snapshot's own sources: paragraph citations in page
//! text, reporting tiers stated in catalogue titles and the period text of each version.
//! Anything a source does not state stays empty.
use crate::Page;
use serde_json::{Value, json};

/// One paragraph cited as `paragraph LABEL of AASB N` in a page's text.
#[derive(Debug, PartialEq)]
pub(crate) struct Citation {
    pub pdf_page: u32,
    pub line: usize,
    pub standard: String,
    pub label: String,
}

/// Strips markup and trailing punctuation from one word.
fn clean(word: &str) -> &str {
    word.trim_matches(['*', '_'])
        .trim_end_matches([',', ';', ':', '.', ')', '*', '_'])
}

/// `AASB N` when `words` start with it; N is digits or `S` and digits.
fn standard(words: &[(usize, &str)]) -> Option<String> {
    let [(_, of), (_, aasb), (_, number), ..] = words else {
        return None;
    };
    let number = clean(number);
    let digits = number.strip_prefix('S').unwrap_or(number);
    (clean(of) == "of"
        && clean(aasb) == "AASB"
        && !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit()))
    .then(|| format!("AASB {number}"))
}

/// The labels listed from the start of `words`, and how many words they use. A range cites
/// its endpoints only; a lettered subparagraph such as `35(a)` cites its parent.
fn labels<'a>(words: &[(usize, &'a str)]) -> (Vec<&'a str>, usize) {
    let mut labels = Vec::new();
    let mut used = 0;
    for (_, word) in words {
        let word = clean(word);
        if !matches!(word, "and" | "or" | "to") {
            let parts: Vec<_> = word
                .split(['–', '-'])
                .map(|part| part.split('(').next().unwrap_or_default())
                .collect();
            if !parts
                .iter()
                .all(|part| crate::paragraphs::valid_label(part))
            {
                break;
            }
            labels.extend(parts);
        }
        used += 1;
    }
    (labels, used)
}

/// Every `paragraph(s) LABEL[, LABEL and LABEL] of AASB N` in the pages' text.
pub(crate) fn citations(pages: &[Page]) -> Vec<Citation> {
    let mut found = Vec::new();
    for page in pages {
        let words: Vec<(usize, &str)> = page
            .text
            .lines()
            .enumerate()
            .flat_map(|(index, line)| line.split_whitespace().map(move |w| (index + 1, w)))
            .collect();
        for (start, (line, word)) in words.iter().enumerate() {
            let word = clean(word).to_ascii_lowercase();
            if !matches!(word.as_str(), "paragraph" | "paragraphs") {
                continue;
            }
            let (listed, used) = labels(&words[start + 1..]);
            let Some(standard) = standard(&words[start + 1 + used..]) else {
                continue;
            };
            found.extend(listed.into_iter().map(|label| Citation {
                pdf_page: page.page,
                line: *line,
                standard: standard.clone(),
                label: label.to_owned(),
            }));
        }
    }
    found
}

fn names(metadata: &[Value]) -> impl Iterator<Item = &str> {
    metadata
        .iter()
        .flat_map(|row| [&row["standard"], &row["title"]])
        .filter_map(Value::as_str)
}

/// What kind of document cites: from its catalogue rows only.
pub(crate) fn kind(metadata: &[Value]) -> &'static str {
    let named = |test: &dyn Fn(&str) -> bool| names(metadata).any(test);
    if named(&|name| {
        name.strip_prefix("AASB ")
            .unwrap_or(name)
            .starts_with("Interpretation")
    }) {
        "interpretation"
    } else if named(&|name| name.contains("Agenda Decision")) {
        "agenda_decision"
    } else if metadata.iter().any(|row| row["standard"].is_string()) {
        "standard"
    } else if named(&|name| name.contains("Amendments")) {
        "amending_standard"
    } else {
        "other"
    }
}

/// The tier a title states: `Tier N Entities` or a closing `Tier N Disclosures`.
fn stated_tier(title: &str) -> Option<u64> {
    let (_, rest) = title.split_once("Tier ")?;
    let (number, rest) = rest.split_once(' ')?;
    let tier = number.parse().ok()?;
    (rest.starts_with("Entities") || rest.trim_end() == "Disclosures").then_some(tier)
}

/// The reporting tier a document's catalogue title states, with that title; else null.
pub(crate) fn reporting_tier(catalogue: &Value, metadata: &[Value]) -> Value {
    let standards: Vec<&str> = metadata
        .iter()
        .filter_map(|row| row["standard"].as_str())
        .collect();
    let listed = catalogue["listings"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|row| {
            row["standard"]
                .as_str()
                .is_some_and(|s| standards.contains(&s))
        })
        .filter_map(|row| row["title"].as_str());
    let own = metadata.iter().filter_map(|row| row["title"].as_str());
    let tiers: Vec<(u64, &str)> = listed
        .chain(own)
        .filter_map(|title| Some((stated_tier(title)?, title)))
        .collect();
    match tiers.first() {
        Some((tier, title)) if tiers.iter().all(|(other, _)| other == tier) => {
            json!({"tier":tier,"basis":title})
        }
        _ => json!({"tier":null,"basis":null}),
    }
}

const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// The ISO date written as `1 January 2027` straight after `marker`.
fn date_after(text: &str, marker: &str) -> Option<String> {
    let (_, rest) = text.split_once(marker)?;
    let mut words = rest.split_whitespace();
    let day: u32 = words.next()?.parse().ok()?;
    let name = words.next()?;
    let month = MONTHS.iter().position(|m| *m == name)? + 1;
    let year: u32 = clean(words.next()?).parse().ok()?;
    (1..=31)
        .contains(&day)
        .then(|| format!("{year:04}-{month:02}-{day:02}"))
}

/// The period bounds one catalogue text states, as ISO dates. `complete` is false when the
/// text names a date in a way not read here.
#[derive(Clone, Default, PartialEq)]
struct Periods {
    from: Option<String>,
    before: Option<String>,
    ends_before: Option<String>,
    ends_from: Option<String>,
    complete: bool,
}

/// Reads the mandatory periods; an early-application sentence is optional and not read.
fn periods(text: &str) -> Periods {
    let text = text.split("Early application").next().unwrap_or_default();
    let found = Periods {
        from: date_after(text, "beginning on or after "),
        before: date_after(text, "but before ").or_else(|| date_after(text, "beginning before ")),
        ends_before: date_after(text, "end before "),
        ends_from: date_after(text, "end on or after "),
        complete: false,
    };
    let read = [
        &found.from,
        &found.before,
        &found.ends_before,
        &found.ends_from,
    ]
    .iter()
    .filter(|date| date.is_some())
    .count();
    let stated = text
        .split_whitespace()
        .filter(|word| MONTHS.contains(&clean(word)))
        .count();
    Periods {
        complete: read == stated,
        ..found
    }
}

/// One text's status on `day`, or `None` when the text is not fully read.
fn status(periods: &Periods, day: &str) -> Option<&'static str> {
    let passed = |date: &Option<String>| date.as_deref().is_some_and(|d| d <= day);
    if !periods.complete {
        None
    } else if periods.from.as_deref().is_some_and(|f| f > day) {
        Some("not_yet_effective")
    } else if passed(&periods.before) || passed(&periods.ends_before) {
        Some("superseded")
    } else {
        periods.from.is_some().then_some("current")
    }
}

/// The periods a document's catalogue rows state, and its status at the catalogue's
/// retrieval date: `not_yet_effective` before its first period may begin, `superseded` once
/// no period beginning on that date is covered, else `current`. The status needs every row
/// fully read and agreeing; the dates need every row to state the same periods. `superseded_by` is filled once every document is
/// known.
pub(crate) fn validity(catalogue: &Value, metadata: &[Value]) -> Value {
    let texts: Vec<&str> = metadata
        .iter()
        .filter_map(|row| row["applies_to_periods_text"].as_str())
        .collect();
    let stated: Vec<Periods> = texts.iter().map(|text| periods(text)).collect();
    let agreed = stated
        .first()
        .filter(|first| stated.iter().all(|p| p == *first))
        .cloned()
        .unwrap_or_default();
    let as_at = catalogue["retrieved_date"].as_str();
    let statuses: Vec<_> = stated
        .iter()
        .map(|p| as_at.and_then(|day| status(p, day)))
        .collect();
    let status = statuses
        .first()
        .filter(|first| statuses.iter().all(|s| s == *first))
        .copied()
        .flatten();
    json!({"status":status,"as_at":as_at,"applies_from":agreed.from,
        "applies_before":agreed.before,"periods_ending_before":agreed.ends_before,
        "periods_ending_on_or_after":agreed.ends_from,"superseded_by":[],"source_text":texts})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(number: u32, text: &str) -> Page {
        Page {
            page: number,
            text: text.into(),
            provenance: "native".into(),
            needs_review: false,
        }
    }

    fn cited(text: &str) -> Vec<(String, String)> {
        citations(&[page(1, text)])
            .into_iter()
            .map(|c| (c.standard, c.label))
            .collect()
    }

    fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items
            .iter()
            .map(|(s, l)| ((*s).to_owned(), (*l).to_owned()))
            .collect()
    }

    #[test]
    fn citations_name_a_standard_and_each_listed_label() {
        assert_eq!(
            cited("as required by paragraph 26 of AASB 16."),
            pairs(&[("AASB 16", "26")])
        );
        assert_eq!(
            cited("paragraphs 26, 27 and Aus27.1 of\n**AASB 16** Leases"),
            pairs(&[("AASB 16", "26"), ("AASB 16", "27"), ("AASB 16", "Aus27.1")])
        );
        // A range cites its endpoints only; a subparagraph cites its parent.
        assert_eq!(
            cited("paragraphs B19–B21 of AASB 3 and paragraph 35(a) of AASB S2,"),
            pairs(&[("AASB 3", "B19"), ("AASB 3", "B21"), ("AASB S2", "35")])
        );
        assert_eq!(
            cited("Paragraph 9 of AASB 10 is amended."),
            pairs(&[("AASB 10", "9")])
        );
        assert!(cited("paragraph 26 of this Standard").is_empty());
        assert!(cited("paragraph 5 and Appendix B of AASB 3").is_empty());
        assert!(cited("paragraphs of AASB 16").is_empty());
        let found = citations(&[
            page(1, "Intro.\n"),
            page(2, "x\nsee paragraph 9 of AASB 10"),
        ]);
        assert_eq!((found[0].pdf_page, found[0].line), (2, 2));
    }

    #[test]
    fn kinds_come_from_catalogue_rows() {
        assert_eq!(kind(&[json!({"standard":"AASB 16"})]), "standard");
        assert_eq!(
            kind(&[json!({"title":"AASB 2026-1 Amendments to Australian Accounting Standards"})]),
            "amending_standard"
        );
        assert_eq!(
            kind(&[json!({"standard":"AASB Interpretation 12"})]),
            "interpretation"
        );
        assert_eq!(
            kind(&[json!({"title":"Agenda Decision: Fabricated topic"})]),
            "agenda_decision"
        );
        assert_eq!(kind(&[]), "other");
    }

    fn catalogue() -> Value {
        json!({"retrieved_date":"2026-10-15","listings":[
            {"standard":"AASB 1060","title":"General Purpose Financial Statements – Simplified Disclosures for For-Profit and Not-for-Profit Tier 2 Entities"},
            {"standard":"AASB 1061","title":"General Purpose Financial Statements – Not-for-Profit Private Sector Tier 3 Entities"},
            {"standard":"AASB 16","title":"Leases"}]})
    }

    #[test]
    fn tiers_are_read_from_titles_and_otherwise_left_empty() {
        let tier = |row: Value| reporting_tier(&catalogue(), &[row])["tier"].clone();
        assert_eq!(tier(json!({"standard":"AASB 1060"})), 2);
        assert_eq!(tier(json!({"standard":"AASB 1061"})), 3);
        assert_eq!(
            tier(
                json!({"title":"AASB 2024-1 Amendments – Supplier Finance Arrangements: Tier 2 Disclosures"})
            ),
            2
        );
        assert_eq!(tier(json!({"standard":"AASB 16"})), Value::Null);
        assert_eq!(
            tier(
                json!({"title":"AASB 2021-6 Amendments – Disclosure of Accounting Policies: Tier 2 and Other Australian Accounting Standards"})
            ),
            Value::Null
        );
        assert_eq!(
            reporting_tier(&catalogue(), &[json!({"standard":"AASB 1060"})])["basis"],
            "General Purpose Financial Statements – Simplified Disclosures for For-Profit and Not-for-Profit Tier 2 Entities"
        );
    }

    fn status(text: Option<&str>) -> Value {
        validity(&catalogue(), &[json!({"applies_to_periods_text":text})])
    }

    #[test]
    fn validity_restates_catalogue_periods_at_the_retrieval_date() {
        let current = status(Some(
            "Applies to periods beginning on or after 1 July 2026 but before 1 January 2027",
        ));
        assert_eq!(current["status"], "current");
        assert_eq!(current["applies_from"], "2026-07-01");
        assert_eq!(current["applies_before"], "2027-01-01");
        assert_eq!(current["as_at"], "2026-10-15");
        let future = status(Some(
            "Applies to periods beginning on or after 1 January 2027",
        ));
        assert_eq!(future["status"], "not_yet_effective");
        assert_eq!(future["applies_before"], Value::Null);
        let old = status(Some(
            "Applies to periods beginning on or after 1 January 2025 but before 1 July 2026",
        ));
        assert_eq!(old["status"], "superseded");
        let amendment = status(Some(
            "beginning on or after 1 July 2025 that end on or after 28 February 2026",
        ));
        assert_eq!(amendment["applies_from"], "2025-07-01");
        assert_eq!(amendment["status"], "current");
        assert_eq!(amendment["periods_ending_on_or_after"], "2026-02-28");
    }

    #[test]
    fn period_end_bounds_count_and_unread_dates_leave_the_status_empty() {
        // A version for periods that end before a past date no longer applies to new periods.
        let ended = status(Some(
            "Applies to periods beginning on or after 1 January 2024 that end before 30 June 2024",
        ));
        assert_eq!(ended["status"], "superseded");
        assert_eq!(ended["periods_ending_before"], "2024-06-30");
        // Any date the text states in a way not read here leaves the status empty.
        // Early application is optional, so only the mandatory periods count.
        let early = status(Some(
            "Annual reporting periods beginning on or after 1 July 2021 but before 1 January 2027. Early application is permitted for periods beginning on or after 1 January 2020.",
        ));
        assert_eq!(early["status"], "current");
        // Rows for one PDF that state different periods but the same status keep it.
        let rows = validity(
            &catalogue(),
            &[
                json!({"applies_to_periods_text":"beginning on or after 1 January 2027"}),
                json!({"applies_to_periods_text":"beginning on or after 1 January 2028"}),
            ],
        );
        assert_eq!(rows["status"], "not_yet_effective");
        assert_eq!(rows["applies_from"], Value::Null);
        let unread = status(Some(
            "Applies to periods beginning on or after 1 January 2024 unless deferred to 1 July 2028",
        ));
        assert_eq!(unread["status"], Value::Null);
        assert_eq!(unread["applies_from"], "2024-01-01");
        for unstated in [
            status(None),
            status(Some("Voluntary for entities that elect to apply it.")),
        ] {
            assert_eq!(unstated["status"], Value::Null);
        }
        let disagree = validity(
            &catalogue(),
            &[
                json!({"applies_to_periods_text":"beginning on or after 1 January 2027"}),
                json!({"applies_to_periods_text":"beginning on or after 1 January 2025"}),
            ],
        );
        assert_eq!(disagree["status"], Value::Null);
        let undated = validity(
            &json!({}),
            &[json!({"applies_to_periods_text":"beginning on or after 1 January 2027"})],
        );
        assert_eq!(undated["status"], Value::Null);
        assert_eq!(undated["applies_from"], "2027-01-01");
    }
}
