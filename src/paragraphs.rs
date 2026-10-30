use crate::{Page, Result, hash};
use serde_json::{Value, json};

// Labels identify extracted text; uncertain boundaries must remain review items.
fn valid_label(label: &str) -> bool {
    let value = label.strip_prefix("Aus").unwrap_or(label);
    let prefix = value.bytes().take_while(u8::is_ascii_uppercase).count();
    if prefix > 3 {
        return false;
    }
    let number = &value[prefix..];
    !number.is_empty()
        && number.split('.').all(|part| {
            let digits = part.bytes().take_while(u8::is_ascii_digit).count();
            digits > 0
                && (digits == part.len()
                    || (digits + 1 == part.len() && part.as_bytes()[digits].is_ascii_uppercase()))
        })
}

fn plain_start(line: &str) -> &str {
    line.trim_start_matches([' ', '\t', '#', '*', '_'])
}

fn label_at(line: &str) -> Option<&str> {
    let line = plain_start(line);
    let end = line.find(char::is_whitespace)?;
    let label = line[..end].trim_end_matches(['*', '_']);
    let body = plain_start(&line[end..]);
    if valid_label(label)
        && body.chars().any(char::is_alphabetic)
        && !body.starts_with(['-', '–', '—', '|'])
    {
        Some(label)
    } else {
        None
    }
}

fn footer(line: &str) -> bool {
    let plain = plain_start(line);
    plain
        .to_ascii_lowercase()
        .starts_with("authorised version ")
        || (plain.starts_with("AASB ")
            && (line.trim_start().starts_with('#')
                || ["STANDARD", "APPENDIX", "COMPILATION", "CONTENTS"]
                    .iter()
                    .any(|word| plain.contains(word))))
}

fn reference_footnote(line: &str) -> bool {
    let Some((number, body)) = line.trim().split_once(char::is_whitespace) else {
        return false;
    };
    number.bytes().all(|b| b.is_ascii_digit())
        && !number.is_empty()
        && (body.starts_with("Corresponding AASB Standard:")
            || body.starts_with("Corresponding AASB Standards:"))
}

fn heading(line: &str) -> bool {
    let line = line.trim();
    let plain = line.trim_matches('*');
    line.starts_with('#')
        || plain.starts_with("Appendix ")
        || (line.starts_with("**")
            && line.ends_with("**")
            && plain.split_whitespace().count() < 18
            && !plain.ends_with(['.', ':', ';', ')', ']']))
}

fn follows(label: &str, next: &str) -> bool {
    let parts = |value: &str| {
        let value = value.strip_prefix("Aus").unwrap_or(value);
        let prefix = value.bytes().take_while(u8::is_ascii_uppercase).count();
        let numbers: Option<Vec<_>> = value[prefix..]
            .split('.')
            .map(|part| {
                let digits = part.trim_end_matches(|c: char| c.is_ascii_uppercase());
                Some((
                    digits.parse::<u32>().ok()?,
                    part.bytes().last().filter(u8::is_ascii_uppercase),
                ))
            })
            .collect();
        Some((value[..prefix].to_owned(), numbers?))
    };
    let Some((prefix, current)) = parts(label) else {
        return false;
    };
    let Some((next_prefix, following)) = parts(next) else {
        return false;
    };
    if prefix != next_prefix {
        return false;
    }
    if following.len() > current.len() {
        return following.len() == current.len() + 1
            && following[..current.len()] == current
            && following.last() == Some(&(1, None));
    }
    let last = following.len() - 1;
    let (number, suffix) = current[last];
    let (next_number, next_suffix) = following[last];
    current[..last] == following[..last]
        && ((number.checked_add(1) == Some(next_number) && next_suffix.is_none())
            || (number == next_number
                && match (suffix, next_suffix) {
                    (None, Some(b'A')) => true,
                    (Some(a), Some(b)) => a.checked_add(1) == Some(b),
                    _ => false,
                }))
}

struct Line<'a> {
    page: &'a Page,
    number: usize,
    text: &'a str,
    skip: bool,
}

pub(crate) fn lookup(pages: &[Page], label: &str) -> Result<Value> {
    if label.len() > 32 || !label.is_ascii() || !valid_label(label) {
        return Err("use an exact paragraph label such as 35, B19, Aus25.1 or BC1; request the parent paragraph for lettered subparagraphs".into());
    }
    let mut lines = Vec::new();
    for page in pages {
        let contents = page.text.lines().any(|line| {
            let plain = plain_start(line).trim_end_matches('*').trim();
            plain == "CONTENTS" || (footer(line) && plain.contains("CONTENTS"))
        });
        for (index, text) in page.text.lines().enumerate() {
            lines.push(Line {
                page,
                number: index + 1,
                text,
                skip: contents || footer(text) || reference_footnote(text),
            });
        }
    }
    let anchors: Vec<_> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| !line.skip)
        .filter_map(|(i, line)| label_at(line.text).map(|label| (i, label)))
        .collect();
    let starts: Vec<_> = anchors
        .iter()
        .enumerate()
        .filter(|(_, (_, found))| *found == label)
        .collect();
    let mut candidates = Vec::new();
    for (position, (start, _)) in starts.iter().take(20) {
        let next = anchors.get(position + 1);
        let mut end = next.map_or(lines.len(), |(index, _)| *index);
        let mut reasons = Vec::new();
        if !next.is_some_and(|(_, next_label)| follows(label, next_label)) {
            reasons.push("No adjacent numbered paragraph confirms the ending.");
        }
        if let Some(index) = (*start + 1..end).find(|i| !lines[*i].skip && heading(lines[*i].text))
        {
            if lines[index + 1..end]
                .iter()
                .any(|line| !line.skip && !line.text.trim().is_empty() && !heading(line.text))
            {
                reasons.push("A section heading interrupts text before the next paragraph anchor.");
            }
            end = index;
        }
        let selected = &lines[*start..end];
        if selected.windows(2).any(|pair| {
            pair[0].page.page != pair[1].page.page
                && pair[0].page.page.checked_add(1) != Some(pair[1].page.page)
        }) {
            reasons.push("A physical page is missing within the candidate span.");
        }
        if selected
            .iter()
            .any(|line| !line.skip && (line.page.needs_review || line.page.provenance != "native"))
        {
            reasons.push("The source span includes OCR or a page flagged for review.");
        }
        if selected.iter().any(|line| {
            !line.skip && (line.text.trim_start().starts_with('|') || line.text.contains("<table"))
        }) {
            reasons.push("The candidate includes a table whose layout requires source review.");
        }
        let mut fragments = Vec::new();
        let mut chunk = Vec::<&Line<'_>>::new();
        let flush = |chunk: &mut Vec<&Line<'_>>, fragments: &mut Vec<Value>| {
            while chunk.last().is_some_and(|line| line.text.trim().is_empty()) {
                chunk.pop();
            }
            if let (Some(first), Some(last)) = (chunk.first(), chunk.last()) {
                let text = chunk
                    .iter()
                    .map(|line| line.text)
                    .collect::<Vec<_>>()
                    .join("\n");
                fragments.push(json!({"pdf_page":first.page.page,"start_line":first.number,"end_line":last.number,
                    "text":text,"page_text_sha256":hash(first.page.text.as_bytes()),
                    "provenance":first.page.provenance,"needs_review":first.page.needs_review}));
            }
            chunk.clear();
        };
        for line in selected {
            if line.skip {
                flush(&mut chunk, &mut fragments);
                continue;
            }
            if chunk
                .last()
                .is_some_and(|previous| previous.page.page != line.page.page)
            {
                flush(&mut chunk, &mut fragments);
            }
            if !chunk.is_empty() || !line.text.trim().is_empty() {
                chunk.push(line);
            }
        }
        flush(&mut chunk, &mut fragments);
        let mut footnotes: Vec<_> = selected
            .iter()
            .filter(|line| reference_footnote(line.text))
            .map(|line| {
                json!({"pdf_page":line.page.page,"line":line.number,"text":line.text,
                "provenance":line.page.provenance,"needs_review":line.page.needs_review})
            })
            .collect();
        let bytes: usize = fragments
            .iter()
            .chain(&footnotes)
            .map(|f| f["text"].as_str().map_or(0, str::len))
            .sum();
        if bytes > 65_536 {
            reasons.push("The candidate exceeds 64 KiB; read the cited pages for its text.");
            for fragment in fragments.iter_mut().chain(&mut footnotes) {
                fragment["text"] = Value::Null;
            }
        }
        candidates.push(
            json!({"needs_review":!reasons.is_empty(),"review_reasons":reasons,
            "next_paragraph":next.map(|(_, label)| *label),"fragments":fragments,"reference_footnotes":footnotes}),
        );
    }
    let status = match starts.len() {
        0 => "not_found",
        1 if candidates[0]["needs_review"] == false => "found",
        1 => "needs_review",
        _ => "ambiguous",
    };
    Ok(
        json!({"paragraph":label,"status":status,"candidate_count":starts.len(),"candidates":candidates,
        "candidates_truncated":starts.len() > 20,
        "boundary":"Lookup uses extracted line anchors in the selected PDF, not a substring search or applicability decision. Repeated labels remain ambiguous. Read physical PDF pages for missing labels, layout or uncertain boundaries."}),
    )
}

fn tokens(text: &str) -> Vec<String> {
    crate::literal(text)
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_owned)
        .collect()
}

/// True when the fragment's words appear contiguously in `reading`, starting and ending on
/// reading word boundaries. Case is kept and markup and punctuation are ignored. The reader
/// may split one fragment word into adjacent pieces (for example at a kerning gap), but no
/// reading word may be skipped, joined across fragment words or matched in part.
fn contiguous(fragment: &str, reading: &str) -> bool {
    let (fragment, reading) = (tokens(fragment), tokens(reading));
    let matches_at = |mut next: usize| {
        fragment.iter().all(|word| {
            let mut rest = word.as_str();
            while !rest.is_empty() {
                match reading
                    .get(next)
                    .and_then(|piece| rest.strip_prefix(piece.as_str()))
                {
                    Some(remaining) => (rest, next) = (remaining, next + 1),
                    None => return false,
                }
            }
            true
        })
    };
    !fragment.is_empty() && (0..reading.len()).any(matches_at)
}

/// Confirms a `found` paragraph against an independent reading of the same PDF pages.
/// Without that reading the status becomes `found_unconfirmed`; a mismatch needs review.
/// A found result without fragments, fragment text or physical pages is an error.
pub(crate) fn confirm(result: &mut Value, reading: Option<&(String, Vec<String>)>) -> Result<()> {
    if result["status"] != "found" {
        result["second_reader"] = json!({"status":"not_checked"});
        return Ok(());
    }
    let mut fragments = Vec::new();
    for fragment in result["candidates"][0]["fragments"]
        .as_array()
        .into_iter()
        .flatten()
    {
        match (fragment["text"].as_str(), fragment["pdf_page"].as_u64()) {
            (Some(text), Some(page)) if page > 0 => fragments.push((text.to_owned(), page)),
            _ => return Err("found paragraph has a fragment without text or a page".into()),
        }
    }
    if fragments.is_empty() {
        return Err("found paragraph has no fragments".into());
    }
    let Some((reader, pages)) = reading else {
        result["status"] = json!("found_unconfirmed");
        result["second_reader"] = json!({"status":"unavailable"});
        return Ok(());
    };
    let mut unconfirmed: Vec<u64> = fragments
        .iter()
        .filter(|(text, page)| {
            !usize::try_from(*page - 1)
                .ok()
                .and_then(|index| pages.get(index))
                .is_some_and(|reading| contiguous(text, reading))
        })
        .map(|(_, page)| *page)
        .collect();
    unconfirmed.dedup();
    if unconfirmed.is_empty() {
        result["second_reader"] = json!({"status":"confirmed","reader":reader});
        return Ok(());
    }
    let candidate = &mut result["candidates"][0];
    candidate["needs_review"] = json!(true);
    if let Some(reasons) = candidate["review_reasons"].as_array_mut() {
        reasons.push(json!(
            "A second reader did not confirm the fragment's words in order on its page."
        ));
    }
    result["status"] = json!("needs_review");
    result["second_reader"] =
        json!({"status":"mismatch","reader":reader,"unconfirmed_pages":unconfirmed});
    Ok(())
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

    #[test]
    fn second_reader_words_must_appear_contiguously_on_the_page() {
        let fragment = "**35** A fabricated requirement applies.";
        assert!(contiguous(
            fragment,
            "Header\n35 A fabri cated require-\nment\napplies. 36"
        ));
        assert!(!contiguous(fragment, "35 A fabricated requirement."));
        assert!(!contiguous(
            fragment,
            "35 A requirement applies fabricated."
        ));
        assert!(!contiguous(
            fragment,
            "35 a fabricated requirement applies."
        ));
        // No match inside longer words, across skipped words or across joined words.
        assert!(!contiguous("one cat", "stone category"));
        assert!(!contiguous("alpha beta", "alpha unrelated beta"));
        assert!(!contiguous(
            "fabricated re-\nquirement",
            "fabricated requirement"
        ));
        assert!(!contiguous("--", "-- text"));
    }

    #[test]
    fn found_paragraphs_need_an_independent_reading_of_their_pages() {
        let pages = [page(
            1,
            "35 A fabricated requirement.\n36 Next requirement.",
        )];
        let checked = |reading: Option<&(String, Vec<String>)>| {
            let mut result = lookup(&pages, "35").unwrap();
            assert_eq!(result["status"], "found");
            confirm(&mut result, reading).unwrap();
            result
        };
        let reader = |text: &str| ("fabricated 1".to_owned(), vec![text.to_owned()]);
        let confirmed = checked(Some(&reader("35 A fabricated requirement. 36 Next")));
        assert_eq!(confirmed["status"], "found");
        assert_eq!(confirmed["second_reader"]["status"], "confirmed");
        assert_eq!(confirmed["second_reader"]["reader"], "fabricated 1");
        let mismatch = checked(Some(&reader("35 A requirement. 36 Next")));
        assert_eq!(mismatch["status"], "needs_review");
        assert_eq!(mismatch["second_reader"]["status"], "mismatch");
        assert_eq!(mismatch["second_reader"]["unconfirmed_pages"], json!([1]));
        assert_eq!(mismatch["candidates"][0]["needs_review"], true);
        let missing_page = checked(Some(&("fabricated 1".to_owned(), vec![])));
        assert_eq!(missing_page["status"], "needs_review");
        let unavailable = checked(None);
        assert_eq!(unavailable["status"], "found_unconfirmed");
        assert_eq!(unavailable["second_reader"]["status"], "unavailable");
        let repeated = [pages[0].text.as_str(), "35 Again.\n36 Next."];
        let repeated: Vec<_> = (1..).zip(repeated).map(|(n, t)| page(n, t)).collect();
        let mut ambiguous = lookup(&repeated, "35").unwrap();
        confirm(&mut ambiguous, None).unwrap();
        assert_eq!(ambiguous["status"], "ambiguous");
        assert_eq!(ambiguous["second_reader"]["status"], "not_checked");
    }

    #[test]
    fn malformed_found_results_cannot_be_confirmed() {
        let mut found = lookup(&[page(1, "35 A fabricated requirement.\n36 Next.")], "35").unwrap();
        // A second valid fragment ensures a malformed one cannot simply be skipped.
        let valid = found["candidates"][0]["fragments"][0].clone();
        found["candidates"][0]["fragments"]
            .as_array_mut()
            .unwrap()
            .push(valid);
        let reading = (
            "fabricated 1".to_owned(),
            vec!["35 A fabricated requirement.".to_owned()],
        );
        let fragment = "/candidates/0/fragments/0";
        let mutations: [(&str, Value); 4] = [
            ("/candidates/0/fragments", json!([])),
            ("/candidates/0/fragments", Value::Null),
            (&format!("{fragment}/text"), Value::Null),
            (&format!("{fragment}/pdf_page"), Value::Null),
        ];
        for (pointer, value) in mutations {
            let mut result = found.clone();
            *result.pointer_mut(pointer).unwrap() = value;
            assert!(confirm(&mut result, Some(&reading)).is_err(), "{pointer}");
            assert!(confirm(&mut result, None).is_err(), "{pointer}");
        }
        let mut blank = found.clone();
        blank["candidates"][0]["fragments"][0]["text"] = json!("--");
        confirm(&mut blank, Some(&reading)).unwrap();
        assert_eq!(blank["status"], "needs_review");
    }

    #[test]
    fn exact_labels_do_not_match_references_contents_or_longer_labels() {
        let pages = [
            page(1, "# CONTENTS\n35 Reference heading 9\n36 Other heading 10"),
            page(
                2,
                "34 A fabricated reference to paragraph 35.\n\n350 A separate fabricated paragraph.\n\n**35** A fabricated requirement.\n(a) A condition.\n\n36 The next requirement.",
            ),
        ];
        let found = lookup(&pages, "35").unwrap();
        assert_eq!(found["status"], "found");
        assert_eq!(found["candidate_count"], 1);
        let fragment = &found["candidates"][0]["fragments"][0];
        assert_eq!(fragment["pdf_page"], 2);
        assert_eq!(fragment["start_line"], 5);
        assert_eq!(fragment["end_line"], 6);
        assert!(fragment["text"].as_str().unwrap().ends_with("A condition."));
        assert!(
            lookup(&pages, "35(a)")
                .unwrap_err()
                .to_string()
                .contains("parent")
        );
    }

    #[test]
    fn paragraphs_continue_across_pages_with_exact_source_spans() {
        let pages = [
            page(
                8,
                "B18 Previous requirement.\n\nB19 A fabricated requirement begins.\n(a) First condition.\n\n## AASB 15-compiled APPENDIX B\nAuthorised Version F123 registered 01/01/2026",
            ),
            page(
                9,
                "## AASB 15-compiled APPENDIX B\nContinuation of the first condition.\n(b) Second condition.\n\nB20 Next requirement.",
            ),
        ];
        let found = lookup(&pages, "B19").unwrap();
        assert_eq!(found["status"], "found");
        let fragments = found["candidates"][0]["fragments"].as_array().unwrap();
        assert_eq!(fragments.len(), 2);
        assert_eq!(fragments[0]["start_line"], 3);
        assert_eq!(fragments[1]["start_line"], 2);
        assert_eq!(fragments[1]["end_line"], 3);
        for fragment in fragments {
            let p = &pages[fragment["pdf_page"].as_u64().unwrap() as usize - 8];
            let start = fragment["start_line"].as_u64().unwrap() as usize - 1;
            let end = fragment["end_line"].as_u64().unwrap() as usize;
            assert_eq!(
                fragment["text"],
                p.text
                    .lines()
                    .skip(start)
                    .take(end - start)
                    .collect::<Vec<_>>()
                    .join("\n")
            );
            assert_eq!(fragment["page_text_sha256"], hash(p.text.as_bytes()));
        }
    }

    #[test]
    fn aus_and_appendix_labels_are_distinct() {
        let pages = [page(
            1,
            "25 Base requirement.\nAus25.1 Australian requirement.\nAus25.2 Following requirement.\n26 Another requirement.\nB19 Appendix requirement.\nB20 Another appendix requirement.",
        )];
        assert_eq!(lookup(&pages, "Aus25.1").unwrap()["status"], "found");
        assert_eq!(lookup(&pages, "B19").unwrap()["status"], "found");
        assert_eq!(lookup(&pages, "19").unwrap()["status"], "not_found");
        for label in ["", "35 OR 36", "../35", "é35", "B", "1..2"] {
            assert!(lookup(&pages, label).is_err());
        }
    }

    #[test]
    fn boundary_confirmation_checks_dotted_and_lettered_sequences() {
        for (a, b) in [
            ("B19", "B20"),
            ("25", "Aus25.1"),
            ("Aus25.1", "Aus25.2"),
            ("Aus25.2", "26"),
            ("35", "35A"),
            ("35A", "35B"),
        ] {
            assert!(follows(a, b), "{a} -> {b}");
        }
        for (a, b) in [
            ("Aus25.1", "Aus25.9"),
            ("B19", "B21"),
            ("35", "35C"),
            ("35A", "35A"),
            ("B19", "BC20"),
        ] {
            assert!(!follows(a, b), "{a} -> {b}");
        }
    }

    #[test]
    fn duplicate_ocr_and_uncertain_spans_are_never_silently_resolved() {
        let mut ocr = page(2, "35 Another fabricated occurrence.\n36 Next requirement.");
        ocr.needs_review = true;
        ocr.provenance = "ocr".into();
        let pages = [
            page(1, "35 A fabricated occurrence.\n36 Next requirement."),
            ocr,
        ];
        assert_eq!(lookup(&pages, "35").unwrap()["status"], "ambiguous");
        assert_eq!(lookup(&pages[1..], "35").unwrap()["status"], "needs_review");
        for text in [
            "35 Final unbounded text.",
            "35 Text.\n# New section\nUnanchored content.\n36 Next.",
            "35 Text.\n39 Gap.",
            "35 Text.\n| Column | Value |\n36 Next.",
        ] {
            assert_eq!(
                lookup(&[page(1, text)], "35").unwrap()["status"],
                "needs_review"
            );
        }
        assert_eq!(
            lookup(
                &[page(1, "35 Begins here."), page(3, "Continues.\n36 Next.")],
                "35"
            )
            .unwrap()["status"],
            "needs_review"
        );
    }

    #[test]
    fn headings_and_correspondence_footnotes_are_separate_from_paragraph_text() {
        let pages = [page(
            1,
            "157 A fabricated requirement.\n21 Corresponding AASB Standard: AASB 15.\n# Following section\n\n158 The next requirement.",
        )];
        let found = lookup(&pages, "157").unwrap();
        assert_eq!(found["status"], "found");
        assert_eq!(
            found["candidates"][0]["fragments"][0]["text"],
            "157 A fabricated requirement."
        );
        assert_eq!(found["candidates"][0]["reference_footnotes"][0]["line"], 2);
        assert_eq!(lookup(&pages, "21").unwrap()["status"], "not_found");
    }

    #[test]
    fn oversized_and_repeated_candidates_keep_references_without_unbounded_text() {
        let text = format!(
            "35 A fabricated requirement.\n21 Corresponding AASB Standard: {}.\n36 Next.",
            "a".repeat(66_000)
        );
        let found = lookup(&[page(1, &text)], "35").unwrap();
        assert_eq!(found["status"], "needs_review");
        assert_eq!(found["candidates"][0]["fragments"][0]["text"], Value::Null);
        assert_eq!(
            found["candidates"][0]["reference_footnotes"][0]["text"],
            Value::Null
        );
        let pages: Vec<_> = (1..=21)
            .map(|n| page(n, "35 A fabricated requirement.\n36 Next."))
            .collect();
        let found = lookup(&pages, "35").unwrap();
        assert_eq!(found["candidate_count"], 21);
        assert_eq!(found["candidates"].as_array().unwrap().len(), 20);
        assert_eq!(found["candidates_truncated"], true);
        assert_eq!(found["status"], "ambiguous");
    }
}
