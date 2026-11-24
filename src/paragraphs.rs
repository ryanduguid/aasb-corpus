use crate::{Page, Result, hash};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

// Labels identify extracted text; uncertain boundaries must remain review items.
pub(crate) fn valid_label(label: &str) -> bool {
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
    // PDF typography can promote lettered clauses and nested Roman clauses
    // to headings. They still belong to the numbered parent paragraph.
    if plain_start(line)
        .strip_prefix('(')
        .and_then(|rest| rest.split_once(')'))
        .is_some_and(|(label, _)| {
            (label.len() == 1 && label.as_bytes()[0].is_ascii_lowercase())
                || matches!(label, "ii" | "iii" | "iv" | "vi" | "vii" | "viii" | "ix")
        })
    {
        return false;
    }
    let plain = line.trim_matches('*');
    line.starts_with('#')
        || plain.starts_with("Appendix ")
        || (line.starts_with("**")
            && line.ends_with("**")
            && plain.split_whitespace().count() < 18
            && !plain.ends_with(['.', ':', ';', ')', ']']))
}

/// A label's letter prefix (without `Aus`) and its dotted numbers with any letter suffix.
type Parts = (String, Vec<(u32, Option<u8>)>);

fn parts(value: &str) -> Option<Parts> {
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
}

/// True when `later` can continue the series `earlier` belongs to: the same letter prefix
/// and a leading number that does not go back. A change or restart marks another part of
/// the PDF, such as an appendix or reproduced text.
pub(crate) fn same_series(earlier: &str, later: &str) -> bool {
    match (parts(earlier), parts(later)) {
        (Some((prefix, first)), Some((next_prefix, next))) => {
            prefix == next_prefix && next[0].0 >= first[0].0
        }
        _ => false,
    }
}

fn follows(label: &str, next: &str) -> bool {
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

/// One extracted line, or the part of it from `start` (a byte offset) when a deleted
/// paragraph's marker and the next label share the line.
struct Line<'a> {
    page: &'a Page,
    number: usize,
    full: &'a str,
    start: usize,
    text: &'a str,
    skip: bool,
    /// The paragraph label this line starts, if it anchors one.
    anchor: Option<&'a str>,
}

/// Splits `C1 [Deleted by the AASB] AusC1.1 ...` after the marker when a label follows.
fn after_deleted_marker(line: &str) -> Option<usize> {
    let label = label_at(line)?;
    let after_label = line.find(label)? + label.len();
    let marker = after_label + line[after_label..].find('[')?;
    if !plain_start(&line[after_label..]).starts_with("[Deleted") {
        return None;
    }
    let close = marker + line[marker..].find(']')? + 1;
    let rest = close + line[close..].find(|c: char| !c.is_whitespace())?;
    label_at(&line[rest..]).map(|_| rest)
}

/// Copyright notice pages number their clauses like paragraphs; contents pages list labels.
fn front_matter(page: &Page) -> bool {
    page.text.lines().any(|line| {
        let plain = plain_start(line).trim_end_matches(['*', '_']).trim();
        plain == "CONTENTS"
            || (footer(line) && plain.contains("CONTENTS"))
            || plain == "COPYRIGHT"
            || (plain.starts_with("AASB ") && plain.ends_with("COPYRIGHT"))
    })
}

/// The leading number of a label without a letter prefix, such as 13 for `13A` or `Aus13.1`.
fn plain_number(label: &str) -> Option<u32> {
    parts(label)
        .filter(|(prefix, _)| prefix.is_empty())
        .map(|(_, numbers)| numbers[0].0)
}

/// A numbered footnote at the foot of a page: a bare number that follows neither of the last
/// two anchored labels and is below any plain number among them, after body text on the page
/// rather than a heading. After it the page holds only wrapped text, higher footnotes, footers
/// and running headers. Its line and any wrapped lines after it are not paragraph text.
fn footnote_at(recent: [Option<&str>; 2], lines: &[&str], index: usize) -> bool {
    let (Some(label), [Some(_), _]) = (label_at(lines[index]), recent) else {
        return false;
    };
    let footnote = |label: &str| {
        label.bytes().all(|b| b.is_ascii_digit())
            && recent.iter().flatten().all(|anchored| {
                !follows(anchored, label)
                    && plain_number(anchored)
                        .is_none_or(|number| plain_number(label) < Some(number))
            })
    };
    let running =
        |line: &str| footer(line) || (heading(line) && plain_start(line).starts_with("AASB "));
    footnote(label)
        && lines[..index]
            .iter()
            .rev()
            .find(|line| !line.trim().is_empty() && !running(line))
            .is_some_and(|line| !heading(line))
        && lines[index + 1..].iter().all(|line| {
            // An underlined label is a paragraph an amending standard inserts.
            running(line)
                || (!heading(line)
                    && label_at(line.trim_start().trim_start_matches("<u>")).is_none_or(|later| {
                        footnote(later) && plain_number(later) > plain_number(label)
                    }))
        })
}

/// Byte offsets after each sentence end in `text`, and whether it is a plain `. `. Otherwise a
/// footnote marker or closing bold sits between the full stop and the space, as extraction
/// gives `entity.¹** 2 This` and `risks.** 6 To`.
fn sentence_ends(text: &str) -> impl Iterator<Item = (usize, bool)> {
    text.match_indices('.').filter_map(move |(at, _)| {
        let rest =
            text[at + 1..].trim_start_matches(['¹', '²', '³', '⁴', '⁵', '⁶', '⁷', '⁸', '⁹', '⁰']);
        let rest = rest.strip_prefix("**").unwrap_or(rest);
        let plain = text.len() - rest.len() == at + 1;
        rest.strip_prefix(' ')
            .map(|after| (text.len() - after.len(), plain))
    })
}

/// Labels that start a sentence inside `text`, outside tables, with their byte offsets and
/// whether a plain `. ` ends the sentence before them. A label followed by lower case is a
/// cross-reference.
fn sentence_labels(text: &str) -> impl Iterator<Item = (usize, &str, bool)> {
    let table = text.trim_start().starts_with('|') || text.contains("<table");
    sentence_ends(text)
        .filter(move |_| !table)
        .filter_map(move |(at, plain)| {
            let label = label_at(&text[at..])?;
            let body = plain_start(&text[at..]);
            let after = plain_start(&body[body.find(char::is_whitespace)?..]);
            (after.starts_with(char::is_uppercase) || after.starts_with("[Deleted"))
                .then_some((at, label, plain))
        })
}

/// A label alone in the first cell of a table row with text in the next cell, as extraction
/// gives `|4|Climate-related ...|||` for a paragraph laid out beside a table. Also returns
/// whether every later cell is empty, so the row holds nothing but the paragraph.
fn table_label(line: &str) -> Option<(&str, bool)> {
    let mut cells = line.trim().strip_prefix('|')?.split('|');
    let label = plain_start(cells.next()?)
        .trim_end()
        .trim_end_matches(['*', '_']);
    let body = plain_start(cells.next()?);
    (valid_label(label) && (body.starts_with(char::is_uppercase) || body.starts_with("[Deleted")))
        .then(|| (label, cells.all(|cell| cell.trim().is_empty())))
}

/// A table row's label, which anchors only when it follows the current label and starts no
/// line, sentence or other row in the PDF.
fn row_anchor<'a>(
    line: &'a str,
    current: Option<&str>,
    occurrences: &HashMap<&str, Starts>,
) -> Option<&'a str> {
    table_label(line).map(|(label, _)| label).filter(|label| {
        current.is_some_and(|current| follows(current, label))
            && matches!(occurrences.get(label), Some((0, 1, _)))
    })
}

/// How often a label starts a line or a sentence after a plain `. `, how often a table row,
/// and how often a sentence after a footnote marker or closing bold.
type Starts = (usize, usize, usize);

/// How often each label starts a line, a sentence or a table row in the PDF, outside front
/// matter and the `skipped` lines (page and line numbers).
fn label_starts<'a>(
    pages: &'a [Page],
    skipped: &HashSet<(u32, usize)>,
) -> HashMap<&'a str, Starts> {
    let mut occurrences = HashMap::new();
    for page in pages.iter().filter(|page| !front_matter(page)) {
        for (index, line) in page.text.lines().enumerate() {
            if skipped.contains(&(page.page, index + 1)) {
                continue;
            }
            // A row does not count against a label inside a line: a Basis for Conclusions
            // table can list paragraphs that the main text holds mid-line.
            for (label, plain) in label_at(line)
                .map(|label| (label, true))
                .into_iter()
                .chain(sentence_labels(line).map(|(_, label, plain)| (label, plain)))
            {
                let starts = occurrences.entry(label).or_insert((0, 0, 0));
                if plain {
                    starts.0 += 1;
                } else {
                    starts.2 += 1;
                }
            }
            if let Some((label, _)) = table_label(line) {
                occurrences.entry(label).or_insert((0, 0, 0)).1 += 1;
            }
        }
    }
    occurrences
}

/// The joined text of a candidate's fragments, which a paragraph search matches.
fn joined(fragments: &[Value]) -> String {
    fragments
        .iter()
        .filter_map(|fragment| fragment["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// A label inside a line anchors only when it is the only occurrence of that label at a line
/// start or a sentence start in the PDF, so it never makes a lookup ambiguous. Lines that a
/// first reading skips, such as page-foot footnotes, do not count. A label after a footnote
/// marker or closing bold also needs no other start or row, and does not count against a
/// label after a plain `. `: reproduced text can repeat a main-text label after bold.
fn page_lines(pages: &[Page]) -> Vec<Line<'_>> {
    let first = lines_with(pages, &label_starts(pages, &HashSet::new()));
    let skipped = first
        .iter()
        .filter(|line| line.skip)
        .map(|line| (line.page.page, line.number))
        .collect();
    lines_with(pages, &label_starts(pages, &skipped))
}

/// The byte offset in `text` of the first sentence label that follows `current` and anchors.
fn following(text: &str, current: &str, occurrences: &HashMap<&str, Starts>) -> Option<usize> {
    sentence_labels(text)
        .find(|(_, inner, plain)| {
            follows(current, inner)
                && occurrences.get(inner).is_some_and(|starts| {
                    if *plain {
                        starts.0 == 1
                    } else {
                        *starts == (0, 0, 1)
                    }
                })
        })
        .map(|(at, _, _)| at)
}

fn lines_with<'a>(pages: &'a [Page], occurrences: &HashMap<&str, Starts>) -> Vec<Line<'a>> {
    let mut lines = Vec::new();
    // The last two anchored labels, the current one first.
    let mut recent: [Option<&str>; 2] = [None, None];
    for page in pages {
        let skip_page = front_matter(page);
        let texts: Vec<&str> = page.text.lines().collect();
        let (mut anchored_here, mut in_footnote) = (false, false);
        for (index, full) in texts.iter().copied().enumerate() {
            in_footnote =
                in_footnote || (!skip_page && anchored_here && footnote_at(recent, &texts, index));
            let skip = skip_page || in_footnote || footer(full) || reference_footnote(full);
            let mut start = 0;
            loop {
                let rest = &full[start..];
                // Table rows never split, so a row's label is always at its start.
                let row = row_anchor(full, recent[0], occurrences).filter(|_| !skip);
                if let Some(label) = label_at(rest).filter(|_| !skip).or(row) {
                    recent = [Some(label), recent[0]];
                    anchored_here = true;
                }
                let split = (!skip)
                    .then(|| {
                        after_deleted_marker(rest)
                            .or_else(|| following(rest, recent[0]?, occurrences))
                    })
                    .flatten();
                let end = split.map_or(full.len(), |split| full[..start + split].trim_end().len());
                let text = &full[start..end];
                lines.push(Line {
                    page,
                    number: index + 1,
                    full,
                    start,
                    text,
                    skip,
                    anchor: row.or_else(|| label_at(text).filter(|_| !skip)),
                });
                match split {
                    Some(split) => start += split,
                    None => break,
                }
            }
        }
    }
    lines
}

/// True when a label that would follow `label` starts inside `text` after a sentence end.
fn label_inside(label: &str, text: &str) -> bool {
    sentence_ends(text)
        .filter_map(|(at, _)| label_at(&text[at..]))
        .any(|inner| follows(label, inner))
}

/// The candidate `lookup` reports and the text a paragraph search matches, which an
/// oversized candidate keeps although its output withholds it.
fn candidate(
    lines: &[Line<'_>],
    label: &str,
    start: usize,
    next: Option<&(usize, &str)>,
) -> (Value, String) {
    let mut end = next.map_or(lines.len(), |(index, _)| *index);
    let mut reasons = Vec::new();
    if !next.is_some_and(|(_, next_label)| follows(label, next_label)) {
        reasons.push("No adjacent numbered paragraph confirms the ending.");
    }
    if let Some(index) = (start + 1..end).find(|i| !lines[*i].skip && heading(lines[*i].text)) {
        if lines[index + 1..end]
            .iter()
            .any(|line| !line.skip && !line.text.trim().is_empty() && !heading(line.text))
        {
            reasons.push("A section heading interrupts text before the next paragraph anchor.");
        }
        end = index;
    }
    let selected = &lines[start..end];
    reasons.extend(span_reasons(selected));
    if selected
        .iter()
        .any(|line| !line.skip && label_inside(label, line.text))
    {
        reasons.push(
            "A following paragraph label appears inside a line of this candidate; read the source page for where it ends.",
        );
    }
    let mut fragments = fragments(selected);
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
    let search = joined(&fragments);
    if bytes > 65_536 {
        reasons.push("The candidate exceeds 64 KiB; read the cited pages for its text.");
        for fragment in fragments.iter_mut().chain(&mut footnotes) {
            fragment["text"] = Value::Null;
        }
    }
    let candidate = json!({"needs_review":!reasons.is_empty(),"review_reasons":reasons,
    "next_paragraph":next.map(|(_, label)| *label),"fragments":fragments,"reference_footnotes":footnotes});
    (candidate, search)
}

fn span_reasons(selected: &[Line<'_>]) -> Vec<&'static str> {
    let mut reasons = Vec::new();
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
    // A row holding only the candidate's own label and text is a paragraph, not a table.
    let own_row = |line: &Line<'_>| {
        table_label(line.text).is_some_and(|(label, alone)| alone && line.anchor == Some(label))
    };
    if selected.iter().any(|line| {
        !line.skip
            && !own_row(line)
            && (line.text.trim_start().starts_with('|') || line.text.contains("<table"))
    }) {
        reasons.push("The candidate includes a table whose layout requires source review.");
    }
    reasons
}

fn fragments(selected: &[Line<'_>]) -> Vec<Value> {
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
            let mut fragment = json!({"pdf_page":first.page.page,"start_line":first.number,"end_line":last.number,
                "text":text,"page_text_sha256":hash(first.page.text.as_bytes()),
                "provenance":first.page.provenance,"needs_review":first.page.needs_review});
            // Columns are one-based characters, present only when a line is split.
            if first.start > 0 {
                fragment["start_column"] = json!(first.full[..first.start].chars().count() + 1);
            }
            let end = last.start + last.text.len();
            if end < last.full.len() {
                fragment["end_column"] = json!(last.full[..end].chars().count());
            }
            fragments.push(fragment);
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
    fragments
}

pub(crate) fn check_label(label: &str) -> Result<()> {
    if label.len() > 32 || !label.is_ascii() || !valid_label(label) {
        return Err("use an exact paragraph label such as 35, B19, Aus25.1 or BC1; request the parent paragraph for lettered subparagraphs".into());
    }
    Ok(())
}

/// One anchored paragraph: its label, the physical page of its anchor, its candidate and
/// its search text.
pub(crate) struct Anchored {
    pub label: String,
    pub pdf_page: u32,
    pub candidate: Value,
    pub text: String,
}

/// Every anchored paragraph of one PDF in reading order, delimited as `lookup` delimits it.
pub(crate) fn index(pages: &[Page]) -> Vec<Anchored> {
    let lines = page_lines(pages);
    let anchors: Vec<_> = lines
        .iter()
        .enumerate()
        .filter_map(|(i, line)| line.anchor.map(|label| (i, label)))
        .collect();
    anchors
        .iter()
        .enumerate()
        .map(|(position, (start, label))| {
            let (candidate, text) = candidate(&lines, label, *start, anchors.get(position + 1));
            Anchored {
                label: (*label).to_owned(),
                pdf_page: lines[*start].page.page,
                candidate,
                text,
            }
        })
        .collect()
}

/// The lookup result for `label` from every candidate in reading order.
pub(crate) fn resolve(label: &str, candidates: Vec<Value>) -> Value {
    let count = candidates.len();
    let status = match count {
        0 => "not_found",
        1 if candidates[0]["needs_review"] == false => "found",
        1 => "needs_review",
        _ => "ambiguous",
    };
    let candidates: Vec<_> = candidates.into_iter().take(20).collect();
    json!({"paragraph":label,"status":status,"candidate_count":count,"candidates":candidates,
    "candidates_truncated":count > 20,
    "boundary":"Lookup uses extracted line anchors in the selected PDF, not a substring search or applicability decision. Repeated labels remain ambiguous. Read physical PDF pages for missing labels, layout or uncertain boundaries."})
}

pub(crate) fn lookup(pages: &[Page], label: &str) -> Result<Value> {
    check_label(label)?;
    let candidates = index(pages)
        .into_iter()
        .filter(|anchored| anchored.label == label)
        .map(|anchored| anchored.candidate)
        .collect();
    Ok(resolve(label, candidates))
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

fn found_fragments(result: &Value) -> Result<Vec<(String, u64)>> {
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
    Ok(fragments)
}

fn flag_for_review(result: &mut Value, reason: &str) {
    let candidate = &mut result["candidates"][0];
    candidate["needs_review"] = json!(true);
    if let Some(reasons) = candidate["review_reasons"].as_array_mut() {
        reasons.push(json!(reason));
    }
    result["status"] = json!("needs_review");
}

/// Confirms a `found` paragraph against an independent reading of the same PDF pages.
/// Without that reading the status becomes `found_unconfirmed`; a mismatch or a fragment
/// without words needs review.
/// A found result without fragments, fragment text or physical pages is an error.
pub(crate) fn confirm(result: &mut Value, reading: Option<&(String, Vec<String>)>) -> Result<()> {
    if result["status"] != "found" {
        result["second_reader"] = json!({"status":"not_checked"});
        return Ok(());
    }
    let fragments = found_fragments(result)?;
    if fragments.iter().any(|(text, _)| tokens(text).is_empty()) {
        flag_for_review(
            result,
            "A fragment has no words for a second reader to confirm.",
        );
        result["second_reader"] = json!({"status":"not_checked"});
        return Ok(());
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
    flag_for_review(
        result,
        "A second reader did not confirm the fragment's words in order on its page.",
    );
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
    fn wordless_fragments_need_review_without_a_reading() {
        let mut blank = lookup(&[page(1, "35 A fabricated requirement.\n36 Next.")], "35").unwrap();
        blank["candidates"][0]["fragments"][0]["text"] = json!("--");
        confirm(&mut blank, None).unwrap();
        assert_eq!(blank["status"], "needs_review");
        assert_eq!(blank["candidates"][0]["needs_review"], true);
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
    fn heading_markup_on_subparagraphs_keeps_the_parent_complete() {
        for text in [
            "## 67 Fabricated requirement:\n\n## (a) First condition\n\n## (b) Second condition\n\n68 Next requirement.",
            "67 Fabricated requirement:\n**(a) First condition**\n**(b) Second condition**\n**(iv) Nested condition**\n68 Next requirement.",
        ] {
            let found = lookup(&[page(1, text)], "67").unwrap();
            assert_eq!(found["status"], "found");
            assert_eq!(found["candidates"][0]["next_paragraph"], "68");
            let retained = found["candidates"][0]["fragments"][0]["text"]
                .as_str()
                .unwrap();
            assert_eq!(
                retained,
                text.split("\n68 Next requirement.")
                    .next()
                    .unwrap()
                    .trim_end()
            );
        }
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
    fn copyright_notice_clauses_are_not_paragraphs() {
        let pages = [
            page(
                2,
                "COPYRIGHT\n1 Fabricated notice clause.\n2 Fabricated notice clause.\n3 Fabricated notice clause.",
            ),
            page(3, "4 Fabricated notice clause.\n**AASB 99 COPYRIGHT**"),
            page(5, "3 Fabricated scope paragraph.\n4 Next paragraph."),
        ];
        let found = lookup(&pages, "3").unwrap();
        assert_eq!(found["status"], "found");
        assert_eq!(found["candidates"][0]["fragments"][0]["pdf_page"], 5);
        assert_eq!(lookup(&pages, "4").unwrap()["candidate_count"], 1);
        assert_eq!(lookup(&pages, "1").unwrap()["status"], "not_found");
    }

    #[test]
    fn a_label_after_a_deleted_marker_anchors_on_the_same_line() {
        let line = "C1 [Deleted by the AASB] AusC1.1 A fabricated effective date of 1 January";
        let pages = [page(
            1,
            &format!("{line}\n\n2025. Fabricated earlier application.\nC1A Next paragraph."),
        )];
        let found = lookup(&pages, "AusC1.1").unwrap();
        assert_eq!(found["status"], "found");
        let fragment = &found["candidates"][0]["fragments"][0];
        assert_eq!(
            (fragment["start_line"].clone(), fragment["end_line"].clone()),
            (json!(1), json!(3))
        );
        assert_eq!(fragment["start_column"], 26);
        assert!(fragment.get("end_column").is_none());
        assert_eq!(
            fragment["text"],
            format!("{}\n\n2025. Fabricated earlier application.", &line[25..])
        );
        let deleted = lookup(&pages, "C1").unwrap();
        assert_eq!(deleted["status"], "found");
        let fragment = &deleted["candidates"][0]["fragments"][0];
        assert_eq!(fragment["text"], "C1 [Deleted by the AASB]");
        assert_eq!(fragment["end_column"], 24);
        assert!(fragment.get("start_column").is_none());
        // Text after a marker that is not a label stays with the deleted paragraph.
        let plain = [page(
            1,
            "5 [Deleted by the AASB] Fabricated heading\n6 Next.",
        )];
        assert_eq!(lookup(&plain, "5").unwrap()["status"], "found");
        assert!(
            lookup(&plain, "5").unwrap()["candidates"][0]["fragments"][0]
                .get("end_column")
                .is_none()
        );
    }

    #[test]
    fn a_following_label_after_a_sentence_end_anchors_mid_line() {
        let first = "C1B Fabricated text. It ends here. C2 For the fabricated purposes.";
        let pages = [page(
            1,
            &format!(
                "{first}\n\nC3 Fabricated relief. C4 In the first fabricated period.\n(a) A condition.\nC5 Next."
            ),
        )];
        let c1b = lookup(&pages, "C1B").unwrap();
        assert_eq!(c1b["status"], "found");
        let fragment = &c1b["candidates"][0]["fragments"][0];
        assert_eq!(fragment["text"], "C1B Fabricated text. It ends here.");
        assert_eq!(fragment["end_column"], 34);
        let c2 = lookup(&pages, "C2").unwrap();
        assert_eq!(c2["status"], "found");
        let fragment = &c2["candidates"][0]["fragments"][0];
        assert_eq!(fragment["text"], &first[35..]);
        assert_eq!(fragment["start_column"], 36);
        assert_eq!(lookup(&pages, "C3").unwrap()["status"], "found");
        let c4 = lookup(&pages, "C4").unwrap();
        assert_eq!(c4["status"], "found");
        assert_eq!(
            c4["candidates"][0]["fragments"][0]["text"],
            "C4 In the first fabricated period.\n(a) A condition."
        );
    }

    #[test]
    fn a_sentence_end_inside_bold_or_after_a_footnote_marker_anchors_mid_line() {
        let first =
            "**1 Fabricated aim for a test entity.¹** 2 This rule asks for fabricated words.";
        let third = "**3 Fabricated scope words.** 4 To meet it, an entity shall add words.";
        // A page-foot footnote numbered like a paragraph does not stop it anchoring mid-line.
        let pages = [
            page(
                1,
                &format!(
                    "## Objective\n\n{first}\n\n## Scope\n\n{third}\n5 Next.\n1 Fabricated footnote words."
                ),
            ),
            page(
                2,
                "B1 Fabricated guidance words.\n\n2 Fabricated footnote words.\n\n**AASB S2-compiled APPENDIX B**",
            ),
        ];
        for (label, text) in [
            ("1", "**1 Fabricated aim for a test entity.¹**"),
            ("2", "2 This rule asks for fabricated words."),
            ("3", "**3 Fabricated scope words.**"),
            ("4", "4 To meet it, an entity shall add words."),
        ] {
            let found = lookup(&pages, label).unwrap();
            assert_eq!(found["status"], "found", "{label}");
            assert_eq!(found["candidates"][0]["fragments"][0]["text"], text);
        }
    }

    #[test]
    fn mid_line_references_and_numbers_do_not_anchor() {
        for text in [
            // The same after a sentence end inside bold or after a footnote marker.
            "C3 Fabricated text.** C4 and C5 also apply.\nC4 Next.",
            "C3 Fabricated text.¹** C9 Applies instead.\nC4 Next.",
            // A reference that ends a sentence, then an unrelated number.
            "C3 See paragraph C4. 2025 was a year.\nC4 Next.",
            // The next label begins a sentence but is a reference: lower case follows it.
            "C3 Fabricated text. C4 and C5 also apply.\nC4 Next.",
            // A label out of sequence is a reference, not the next paragraph.
            "C3 Fabricated text. C9 Applies instead.\nC4 Next.",
            "C3 Fabricated text.\n| Cell ends. C4 In a table. |\nC4 Next.",
            // A label repeated elsewhere in the PDF, as reproduced text repeats it, stays
            // inside its line so that it cannot make a lookup ambiguous.
            "C3 Fabricated text. C4 In the main text.\nC5 Next.\n## Reproduced\nC4 Copy.\nC5 Next.",
            // A copy after bold leaves the main text's label its anchor.
            "C3 Fabricated text. C4 In the main text.\nC5 Next.\n## Reproduced\nC3 Copy.** C4 Copy.\nC5 Next.",
        ] {
            let pages = [page(1, text)];
            assert_eq!(
                lookup(&pages, "C4").unwrap()["candidate_count"],
                1,
                "{text}"
            );
            assert_eq!(
                lookup(&pages, "C9").unwrap()["status"],
                "not_found",
                "{text}"
            );
        }
        // A following label with lower case after it is still flagged for review.
        let flagged = lookup(
            &[page(
                1,
                "C3 Fabricated text. C4 and C5 also apply.\nC4 Next.",
            )],
            "C3",
        )
        .unwrap();
        let reasons = flagged["candidates"][0]["review_reasons"].to_string();
        assert!(reasons.contains("inside a line"), "{reasons}");
        // So is one after bold that reproduced text repeats.
        let copy = lookup(
            &[page(
                1,
                "C3 Fabricated text. C4 In the main text.\nC5 Next.\n## Reproduced\nC3 Copy.** C4 Copy.\nC5 Next.",
            )],
            "C3",
        )
        .unwrap();
        let reasons = copy["candidates"][1]["review_reasons"].to_string();
        assert!(reasons.contains("inside a line"), "{reasons}");
    }

    #[test]
    fn out_of_sequence_footnotes_at_a_page_foot_are_not_paragraphs() {
        let pages = [
            page(5, "3 Fabricated scope paragraph.\n4 Next paragraph."),
            page(
                9,
                "BC28 Fabricated basis.\nBC29 Fabricated feedback.\n\n3 *Fabricated Act 2001*, section 1. Fabricated\nconvention continues.\n\n**AASB 99-compiled BASIS FOR CONCLUSIONS**",
            ),
            page(10, "BC30 Next basis."),
        ];
        let found = lookup(&pages, "3").unwrap();
        assert_eq!(found["status"], "found");
        assert_eq!(found["candidates"][0]["fragments"][0]["pdf_page"], 5);
        let bc29 = lookup(&pages, "BC29").unwrap();
        assert_eq!(bc29["status"], "found");
        assert_eq!(bc29["candidates"][0]["next_paragraph"], "BC30");
        let text = bc29["candidates"][0]["fragments"].to_string();
        assert!(
            !text.contains("Fabricated Act") && !text.contains("convention"),
            "{text}"
        );
    }

    #[test]
    fn paragraphs_at_a_page_foot_that_are_not_footnotes_still_anchor() {
        for (text, label) in [
            // A gap in the sequence: a later number is not a footnote.
            (
                "12 Fabricated text.\n13 Fabricated text.\n\n15 Fabricated later paragraph.",
                "15",
            ),
            // A series that restarts after a heading.
            (
                "BC29 Fabricated basis.\n\n## Fabricated heading\n\n1 Fabricated objective.",
                "1",
            ),
            // A footnote is not followed by a lettered paragraph on its page.
            (
                "BC29 Fabricated basis.\n3 Fabricated text.\nB1 Fabricated guidance.",
                "3",
            ),
        ] {
            let found = lookup(&[page(1, text)], label).unwrap();
            assert_eq!(found["candidate_count"], 1, "{text}");
        }
        // A page of numbered paragraphs with no anchor above them holds no footnote.
        let pages = [
            page(1, "C9 Fabricated transition."),
            page(
                2,
                "# Fabricated deleted text\n*Fabricated note.*\n47 Fabricated text.\n48 Fabricated text.",
            ),
        ];
        assert_eq!(lookup(&pages, "47").unwrap()["status"], "found");
        // An amending standard numbers its instructions between reproduced paragraphs; an
        // instruction followed by an underlined inserted paragraph is not a footnote.
        let pages = [
            page(
                5,
                "5 Paragraph 93 is amended.\n28 Fabricated reproduced text.",
            ),
            page(
                6,
                "93 Fabricated reproduced text.\n\n6 Paragraph AusC6.1 is added.\n\n<u>AusC6.1 Fabricated inserted text.</u>\n\n## AASB 9999-9 STANDARD",
            ),
            page(7, "7 Paragraph 99 is deleted."),
        ];
        assert_eq!(lookup(&pages, "6").unwrap()["status"], "found");
    }

    #[test]
    fn a_label_alone_in_a_table_rows_first_cell_anchors_in_sequence() {
        // The shape extraction gives a paragraph laid out beside a table's last row.
        let text = "**3 This Standard applies to:**\n\n**(a) fabricated risks, which are:**\n\n|||(i)|fabricated physical risks; and|\n|---|---|---|---|\n||(b)|(ii) fabricated opportunities.|fabricated transition risks; and|\n|4|Fabricated risks that cannot affect prospects are outside its scope.|||\n\n## Fabricated heading\n\n**5 The fabricated objective.**";
        let pages = [page(7, text)];
        let four = lookup(&pages, "4").unwrap();
        assert_eq!(four["status"], "found");
        assert_eq!(
            four["candidates"][0]["fragments"][0]["text"],
            "|4|Fabricated risks that cannot affect prospects are outside its scope.|||"
        );
        // Paragraph 3 now ends at 4, but its own garbled table rows still need review.
        let three = &lookup(&pages, "3").unwrap()["candidates"][0];
        assert_eq!(three["next_paragraph"], "4");
        let reasons = three["review_reasons"].to_string();
        assert!(!reasons.contains("No adjacent"), "{reasons}");
        assert!(reasons.contains("table"), "{reasons}");
        // A Basis for Conclusions table listing a paragraph does not stop the paragraph
        // anchoring inside a line.
        let pages = [
            page(
                9,
                "Aus1.15 Fabricated text. Aus1.15.1 In respect of fabricated entities.\nAus1.16 Next.",
            ),
            page(
                57,
                "BC5 Fabricated basis.\n|Aus1.15.1|To clarify fabricated text.|",
            ),
        ];
        assert_eq!(lookup(&pages, "Aus1.15.1").unwrap()["status"], "found");
        // A row whose other cells hold text anchors but stays a review item.
        let row = "3 Fabricated text.\n|4|Fabricated cell.|Other cell.|\n5 Next.";
        assert_eq!(
            lookup(&[page(1, row)], "4").unwrap()["status"],
            "needs_review"
        );
    }

    #[test]
    fn ordinary_numbered_table_rows_do_not_anchor() {
        for text in [
            // Out of sequence with the current paragraph.
            "12 Fabricated text.\n|1|Revenue|100|\n|2|Expenses|50|\n13 Next.",
            // Lower case after the number: a quantity, not a paragraph.
            "3 Fabricated text.\n|4|units sold|\n5 Next.",
            // The label also starts a line elsewhere, so the row would make it ambiguous.
            "3 Fabricated text.\n|4|Fabricated cell.|\n4 Fabricated paragraph.\n5 Next.",
            // The first cell holds more than a label.
            "3 Fabricated text.\n|4 and 5|Fabricated cell.|\n6 Next.",
        ] {
            let pages = [page(1, text)];
            for label in ["1", "2", "4"] {
                let found = lookup(&pages, label).unwrap();
                let expected = u64::from(label == "4" && text.contains("\n4 Fabricated"));
                assert_eq!(found["candidate_count"], expected, "{label} in {text}");
            }
        }
    }

    #[test]
    fn series_continue_until_the_label_prefix_changes_or_restarts() {
        for (a, b) in [
            ("25", "Aus25.1"),
            ("Aus25.1", "27"),
            ("B1", "B9"),
            ("C1", "AusC1.1"),
        ] {
            assert!(same_series(a, b), "{a} -> {b}");
        }
        for (a, b) in [("105", "B1"), ("C6", "C1"), ("5", "1"), ("BC9", "B10")] {
            assert!(!same_series(a, b), "{a} -> {b}");
        }
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
