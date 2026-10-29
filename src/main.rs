use rusqlite::{Connection, OpenFlags, params};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::error::Error;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Component, Path};
use unicode_normalization::UnicodeNormalization;

mod paragraphs;

type Result<T> = std::result::Result<T, Box<dyn Error>>;

#[derive(Deserialize)]
struct Sources {
    files: Vec<Source>,
}
#[derive(Deserialize)]
struct Source {
    filename: String,
    source_url: String,
    sha256: String,
    page_count: u32,
}
#[derive(Deserialize)]
struct Extraction {
    source_manifest_sha256: String,
    documents: Vec<Document>,
    failures: Vec<Value>,
}
#[derive(Deserialize)]
struct Document {
    sha256: String,
    page_count: u32,
    pages_sha256: String,
}
#[derive(Deserialize)]
struct Page {
    page: u32,
    text: String,
    provenance: String,
    needs_review: bool,
}

fn hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
fn file_hash(path: &Path) -> Result<String> {
    Ok(hash(&fs::read(path)?))
}
fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}
fn filename(value: &str) -> Result<&str> {
    if value.is_empty()
        || value.contains(['/', '\\', ':'])
        || !matches!(
            Path::new(value).components().next(),
            Some(Component::Normal(_))
        )
        || Path::new(value).components().count() != 1
    {
        return Err("source filename must be a single relative filename".into());
    }
    Ok(value)
}
fn valid_sha(value: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err("invalid SHA-256 identifier".into());
    }
    Ok(())
}

// Adapted from fadden/as_at.py::_literal and quote_in_text at the recorded upstream revision.
fn literal(text: &str) -> String {
    text.nfkc().collect::<String>().replace('\u{2010}', "-")
}
fn words(text: &str) -> Vec<String> {
    literal(text)
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

// Fewer consecutive words than this are too common to locate a quotation.
const PARTIAL_MIN_WORDS: usize = 4;

/// Grades a quotation against one page, strongest first: `exact` bytes; `normalised` under
/// the compatibility rules or after collapsing whitespace; `partial` for the longest run of
/// consecutive quotation words; otherwise `none`. Comparison stays case-sensitive.
fn grade_quote(quote: &str, text: &str) -> Result<Value> {
    let quote_words = words(quote);
    if quote_words.is_empty() {
        return Err("a blank quotation is not valid".into());
    }
    if text.contains(quote) {
        return Ok(json!({"grade":"exact"}));
    }
    let page_words = words(text);
    if literal(text).contains(&literal(quote))
        || page_words
            .windows(quote_words.len())
            .any(|window| window == quote_words)
    {
        return Ok(json!({"grade":"normalised"}));
    }
    // Longest common run of words: run[j] is the run ending at quotation word j.
    let (mut length, mut end) = (0, 0);
    let mut run = vec![0; quote_words.len() + 1];
    for word in &page_words {
        for j in (1..=quote_words.len()).rev() {
            run[j] = if &quote_words[j - 1] == word {
                run[j - 1] + 1
            } else {
                0
            };
            if run[j] > length {
                (length, end) = (run[j], j);
            }
        }
    }
    if length < PARTIAL_MIN_WORDS {
        return Ok(json!({"grade":"none"}));
    }
    Ok(
        json!({"grade":"partial","matched_words":length,"quote_words":quote_words.len(),
        "first_matched_word":end - length + 1}),
    )
}

fn grade_rank(grade: &Value) -> (usize, u64) {
    let rank = match grade["grade"].as_str() {
        Some("exact") => 3,
        Some("normalised") => 2,
        Some("partial") => 1,
        _ => 0,
    };
    (rank, grade["matched_words"].as_u64().unwrap_or(0))
}

fn schema(connection: &Connection) -> Result<()> {
    connection.execute_batch("
        CREATE TABLE documents(sha256 TEXT PRIMARY KEY, page_count INTEGER NOT NULL,
            aliases TEXT NOT NULL, metadata TEXT NOT NULL, pages_sha256 TEXT NOT NULL);
        CREATE TABLE pages(id INTEGER PRIMARY KEY, document_sha256 TEXT NOT NULL REFERENCES documents(sha256),
            page INTEGER NOT NULL, text TEXT NOT NULL, text_sha256 TEXT NOT NULL,
            provenance TEXT NOT NULL, needs_review INTEGER NOT NULL,
            UNIQUE(document_sha256,page));
        CREATE VIRTUAL TABLE pages_fts USING fts5(text, content='pages', content_rowid='id');
        PRAGMA user_version=1;
    ")?;
    Ok(())
}

fn metadata_for(catalogue: &Value, aliases: &[&Source]) -> Vec<Value> {
    ["versions", "amendments"]
        .iter()
        .flat_map(|key| catalogue[*key].as_array().into_iter().flatten())
        .filter(|row| {
            row["download_url"]
                .as_str()
                .is_some_and(|url| aliases.iter().any(|source| source.source_url == url))
        })
        .cloned()
        .collect()
}

fn build(root: &Path) -> Result<Value> {
    let target = root.join("index.sqlite3");
    let staging = root.join("index.sqlite3.partial");
    if target.exists() || staging.exists() {
        return Err("index destination exists; choose a new snapshot".into());
    }
    let result = build_staged(root, &staging, &target);
    if result.is_err() {
        // This call created any staging file; remove it so a corrected snapshot can be rebuilt.
        let _ = fs::remove_file(&staging);
    }
    result
}

fn build_staged(root: &Path, staging: &Path, target: &Path) -> Result<Value> {
    let source_path = root.join("source/manifest.json");
    let sources: Sources = read_json(&source_path)?;
    let extraction: Extraction = read_json(&root.join("extracted/manifest.json"))?;
    if !extraction.failures.is_empty()
        || file_hash(&source_path)? != extraction.source_manifest_sha256
    {
        return Err("extraction failures or changed source manifest".into());
    }
    let catalogue: Value = read_json(&root.join("catalogue.json"))?;
    let mut grouped: BTreeMap<&str, Vec<&Source>> = BTreeMap::new();
    for source in &sources.files {
        valid_sha(&source.sha256)?;
        if file_hash(&root.join("source").join(filename(&source.filename)?))? != source.sha256 {
            return Err(format!("source changed: {}", source.filename).into());
        }
        grouped.entry(&source.sha256).or_default().push(source);
    }
    if extraction.documents.len() != grouped.len() {
        return Err("extraction document coverage differs".into());
    }
    let mut connection = Connection::open(staging)?;
    connection.execute_batch("PRAGMA foreign_keys=ON;")?;
    let transaction = connection.transaction()?;
    schema(&transaction)?;
    let mut page_count = 0;
    for document in &extraction.documents {
        valid_sha(&document.sha256)?;
        let aliases = grouped
            .remove(document.sha256.as_str())
            .ok_or("missing or duplicate document identity")?;
        if aliases
            .iter()
            .any(|source| source.page_count != document.page_count)
        {
            return Err("source and extraction page counts differ".into());
        }
        let page_path = root
            .join("extracted")
            .join(&document.sha256)
            .join("pages.jsonl");
        if file_hash(&page_path)? != document.pages_sha256 {
            return Err("extracted pages changed".into());
        }
        let alias_json: Vec<Value> = aliases
            .iter()
            .map(|source| json!({"filename":source.filename,"source_url":source.source_url}))
            .collect();
        transaction.execute(
            "INSERT INTO documents VALUES (?1,?2,?3,?4,?5)",
            params![
                document.sha256,
                document.page_count,
                serde_json::to_string(&alias_json)?,
                serde_json::to_string(&metadata_for(&catalogue, &aliases))?,
                document.pages_sha256
            ],
        )?;
        let mut expected = 1;
        for line in BufReader::new(File::open(&page_path)?).lines() {
            let page: Page = serde_json::from_str(&line?)?;
            if page.page != expected {
                return Err("missing, duplicated or reordered page".into());
            }
            transaction.execute(
                "INSERT INTO pages(document_sha256,page,text,text_sha256,provenance,needs_review)
                VALUES (?1,?2,?3,?4,?5,?6)",
                params![
                    document.sha256,
                    page.page,
                    page.text,
                    hash(page.text.as_bytes()),
                    page.provenance,
                    page.needs_review || page.text.trim().is_empty()
                ],
            )?;
            expected += 1;
            page_count += 1;
        }
        if expected != document.page_count + 1 || file_hash(&page_path)? != document.pages_sha256 {
            return Err("page coverage differs or extraction changed during indexing".into());
        }
    }
    transaction.execute("INSERT INTO pages_fts(pages_fts) VALUES('rebuild')", [])?;
    transaction.execute(
        "INSERT INTO pages_fts(pages_fts) VALUES('integrity-check')",
        [],
    )?;
    transaction.commit()?;
    let integrity: String = connection.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        return Err(format!("SQLite integrity check: {integrity}").into());
    }
    connection.close().map_err(|(_, error)| error)?;
    fs::rename(staging, target)?;
    Ok(
        json!({"documents":extraction.documents.len(),"source_files":sources.files.len(),"pages":page_count,"index":target}),
    )
}

fn open(root: &Path) -> Result<Connection> {
    let connection =
        Connection::open_with_flags(root.join("index.sqlite3"), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version != 1 {
        return Err("unsupported corpus schema".into());
    }
    Ok(connection)
}

fn stats(connection: &Connection) -> Result<Value> {
    let count = |sql: &str| -> Result<i64> { Ok(connection.query_row(sql, [], |row| row.get(0))?) };
    let mut empty_pages = 0;
    let mut statement = connection.prepare("SELECT text FROM pages WHERE needs_review=1")?;
    for text in statement.query_map([], |row| row.get::<_, String>(0))? {
        if text?.trim().is_empty() {
            empty_pages += 1;
        }
    }
    Ok(json!({"documents":count("SELECT count(*) FROM documents")?,
        "pages":count("SELECT count(*) FROM pages")?,
        "review_pages":count("SELECT count(*) FROM pages WHERE needs_review=1")?,
        "empty_pages":empty_pages}))
}

#[derive(Debug)]
struct QueryOptions {
    standard: Option<String>,
    version: Option<String>,
    limit: u32,
    offset: i64,
}

impl Default for QueryOptions {
    fn default() -> Self {
        Self {
            standard: None,
            version: None,
            limit: 20,
            offset: 0,
        }
    }
}

impl QueryOptions {
    fn parse(arguments: &[String]) -> Result<Self> {
        let mut options = Self::default();
        let mut seen = std::collections::BTreeSet::new();
        let (pairs, remainder) = arguments.as_chunks::<2>();
        for pair in pairs {
            if !seen.insert(pair[0].as_str()) {
                return Err(format!("duplicate option: {}", pair[0]).into());
            }
            match pair[0].as_str() {
                "--standard" if !pair[1].trim().is_empty() => {
                    options.standard = Some(pair[1].clone())
                }
                "--version" => {
                    valid_sha(&pair[1])?;
                    options.version = Some(pair[1].clone());
                }
                "--limit" => options.limit = pair[1].parse()?,
                "--offset" => options.offset = pair[1].parse()?,
                _ => return Err(format!("invalid option: {}", pair[0]).into()),
            }
        }
        if !remainder.is_empty() {
            return Err("each option requires a value".into());
        }
        if !(1..=100).contains(&options.limit)
            || options.offset < 0
            || options
                .offset
                .checked_add(i64::from(options.limit))
                .is_none()
        {
            return Err("limit must be 1..100 and offset a non-negative supported integer".into());
        }
        Ok(options)
    }

    fn filters(&self) -> Value {
        json!({"standard": self.standard, "version": self.version})
    }
}

fn standard_key(value: &str) -> String {
    value
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_uppercase)
        .collect()
}

fn matching_documents(connection: &Connection, options: &QueryOptions) -> Result<Vec<Value>> {
    let mut statement = connection
        .prepare("SELECT sha256,page_count,aliases,metadata FROM documents ORDER BY sha256")?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, u32>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    let mut documents = Vec::new();
    let mut version_exists = options.version.is_none();
    for row in rows {
        let (sha, count, aliases, metadata) = row?;
        if options
            .version
            .as_ref()
            .is_some_and(|version| version != &sha)
        {
            continue;
        }
        version_exists = true;
        let metadata: Vec<Value> = serde_json::from_str(&metadata)?;
        if options.standard.as_ref().is_some_and(|standard| {
            !metadata.iter().any(|m| {
                m["standard"]
                    .as_str()
                    .is_some_and(|s| standard_key(s) == standard_key(standard))
            })
        }) {
            continue;
        }
        documents.push(json!({"document_sha256":sha,"page_count":count,
            "sources":serde_json::from_str::<Value>(&aliases)?,"version_metadata":metadata}));
    }
    if !version_exists {
        return Err("version is not present in this snapshot".into());
    }
    Ok(documents)
}

fn catalogue(connection: &Connection, options: &QueryOptions) -> Result<Value> {
    let documents = matching_documents(connection, options)?;
    let total = documents.len();
    let results: Vec<_> = documents
        .into_iter()
        .skip(usize::try_from(options.offset).unwrap_or(usize::MAX))
        .take(options.limit as usize)
        .collect();
    let has_more = i128::from(options.offset) + i128::from(options.limit) < total as i128;
    Ok(
        json!({"results":results,"filters":options.filters(),"total_documents":total,
        "limit":options.limit,"offset":options.offset,"has_more":has_more,
        "next_offset":has_more.then_some(options.offset + i64::from(options.limit)),
        "boundary":"Version identifiers are exact PDF hashes. Catalogue dates do not determine applicability. Standard filters select principal documents, not related amendments."}),
    )
}

fn search(connection: &Connection, query: &str, options: &QueryOptions) -> Result<Value> {
    if query.trim().is_empty() {
        return Err("search query must not be blank".into());
    }
    let selected = if options.standard.is_some() || options.version.is_some() {
        let documents: Vec<_> = matching_documents(connection, options)?
            .into_iter()
            .map(|document| document["document_sha256"].clone())
            .collect();
        Some(serde_json::to_string(&documents)?)
    } else {
        None
    };
    let mut statement = connection.prepare(
        "SELECT p.document_sha256,p.page,
        snippet(pages_fts,0,'[',']',' … ',40),p.provenance,p.needs_review,d.aliases,d.metadata
        FROM pages_fts JOIN pages p ON p.id=pages_fts.rowid
        JOIN documents d ON d.sha256=p.document_sha256
        WHERE pages_fts MATCH ?1
        AND (?2 IS NULL OR p.document_sha256 IN (SELECT value FROM json_each(?2)))
        ORDER BY bm25(pages_fts),p.document_sha256,p.page LIMIT ?3 OFFSET ?4",
    )?;
    let rows = statement.query_map(
        params![query, selected, options.limit + 1, options.offset],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, u32>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, bool>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
            ))
        },
    )?;
    let mut hits = Vec::new();
    for row in rows {
        let (sha, page, snippet, provenance, needs_review, aliases, metadata) = row?;
        hits.push(json!({"document_sha256":sha,"pdf_page":page,"snippet":snippet,
            "provenance":provenance,"needs_review":needs_review,
            "sources":serde_json::from_str::<Value>(&aliases)?,"version_metadata":serde_json::from_str::<Value>(&metadata)?}));
    }
    let has_more = hits.len() > options.limit as usize;
    hits.truncate(options.limit as usize);
    Ok(
        json!({"query":query,"results":hits,"limit":options.limit,"offset":options.offset,
        "filters":options.filters(),"has_more":has_more,
        "next_offset":has_more.then_some(options.offset + i64::from(options.limit)),
        "boundary":"Search covers captured versions; it does not determine reporting-period applicability."}),
    )
}

fn paragraph(connection: &Connection, sha: &str, label: &str) -> Result<Value> {
    valid_sha(sha)?;
    let (aliases, metadata): (String, String) = connection.query_row(
        "SELECT aliases,metadata FROM documents WHERE sha256=?1",
        [sha],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let mut statement = connection.prepare("SELECT page,text,provenance,needs_review FROM pages WHERE document_sha256=?1 ORDER BY page")?;
    let pages: Vec<Page> = statement
        .query_map([sha], |r| {
            Ok(Page {
                page: r.get(0)?,
                text: r.get(1)?,
                provenance: r.get(2)?,
                needs_review: r.get(3)?,
            })
        })?
        .collect::<std::result::Result<_, _>>()?;
    let mut result = paragraphs::lookup(&pages, label)?;
    result["document_sha256"] = json!(sha);
    result["sources"] = serde_json::from_str(&aliases)?;
    result["version_metadata"] = serde_json::from_str(&metadata)?;
    Ok(result)
}

fn page(connection: &Connection, sha: &str, number: u32) -> Result<Value> {
    valid_sha(sha)?;
    let (text, provenance, needs_review, aliases, metadata) = connection.query_row(
        "SELECT p.text,p.provenance,p.needs_review,d.aliases,d.metadata FROM pages p
        JOIN documents d ON d.sha256=p.document_sha256 WHERE p.document_sha256=?1 AND p.page=?2",
        params![sha, number],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, bool>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        },
    )?;
    Ok(
        json!({"document_sha256":sha,"pdf_page":number,"text":text,"provenance":provenance,
        "needs_review":needs_review,"sources":serde_json::from_str::<Value>(&aliases)?,
        "version_metadata":serde_json::from_str::<Value>(&metadata)?}),
    )
}

/// Only an exact match on a named native page is verified; a search without a page,
/// normalised or partial grades and OCR or review pages only locate candidates.
fn verify_quote(
    connection: &Connection,
    sha: &str,
    number: Option<u32>,
    quote: &str,
) -> Result<Value> {
    valid_sha(sha)?;
    let (aliases, metadata): (String, String) = connection.query_row(
        "SELECT aliases,metadata FROM documents WHERE sha256=?1",
        [sha],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let mut statement = connection.prepare(
        "SELECT page,text,text_sha256,provenance,needs_review FROM pages
        WHERE document_sha256=?1 AND (?2 IS NULL OR page=?2) ORDER BY page",
    )?;
    let mut pages = Vec::new();
    for row in statement.query_map(params![sha, number], |r| {
        Ok((
            r.get::<_, u32>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, bool>(4)?,
        ))
    })? {
        let (page, text, digest, provenance, needs_review) = row?;
        if hash(text.as_bytes()) != digest {
            return Err("indexed text digest differs".into());
        }
        let mut result = grade_quote(quote, &text)?;
        result["pdf_page"] = json!(page);
        result["page_text_sha256"] = json!(digest);
        result["provenance"] = json!(provenance);
        result["needs_review"] = json!(needs_review);
        pages.push(result);
    }
    if pages.is_empty() {
        return Err("page is not present in this document".into());
    }
    let best = pages.iter().map(grade_rank).max().unwrap_or_default();
    let mut result = if number.is_some() {
        pages.swap_remove(0)
    } else {
        let located: Vec<_> = pages
            .into_iter()
            .filter(|page| best.0 > 0 && grade_rank(page) == best)
            .collect();
        json!({"pdf_page":null,"grade":(["none","partial","normalised","exact"][best.0]),
            "located_page_count":located.len(),"pages_truncated":located.len() > 20,
            "pages":located.into_iter().take(20).collect::<Vec<_>>()})
    };
    let verified = number.is_some()
        && result["grade"] == "exact"
        && result["provenance"] == "native"
        && result["needs_review"] == false;
    result["verified"] = json!(verified);
    // Retained for existing callers, with the stricter meaning.
    result["matches"] = json!(verified);
    result["document_sha256"] = json!(sha);
    result["sources"] = serde_json::from_str(&aliases)?;
    result["version_metadata"] = serde_json::from_str(&metadata)?;
    result["boundary"] = json!(
        "Only an exact match on a named native page of the selected PDF is verified. Normalised and partial grades, OCR or review pages and searches without a page locate candidates for review. A text match does not establish accounting applicability."
    );
    Ok(result)
}

fn verify(root: &Path, connection: &Connection) -> Result<Value> {
    let integrity: String = connection.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        return Err("SQLite integrity check failed".into());
    }
    let sources: Sources = read_json(&root.join("source/manifest.json"))?;
    for source in sources.files {
        if file_hash(&root.join("source").join(filename(&source.filename)?))? != source.sha256 {
            return Err(format!("source digest differs: {}", source.filename).into());
        }
    }
    let mut statement =
        connection.prepare("SELECT sha256,page_count,pages_sha256 FROM documents")?;
    for row in statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, u32>(1)?,
            row.get::<_, String>(2)?,
        ))
    })? {
        let (sha, expected, digest) = row?;
        valid_sha(&sha)?;
        if file_hash(&root.join("extracted").join(&sha).join("pages.jsonl"))? != digest {
            return Err("extraction digest differs".into());
        }
        let actual: u32 = connection.query_row(
            "SELECT count(*) FROM pages WHERE document_sha256=?1",
            [&sha],
            |row| row.get(0),
        )?;
        if actual != expected {
            return Err("indexed page count differs".into());
        }
    }
    let mut statement = connection.prepare("SELECT text,text_sha256 FROM pages")?;
    for row in statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })? {
        let (text, digest) = row?;
        if hash(text.as_bytes()) != digest {
            return Err("indexed text digest differs".into());
        }
    }
    Ok(
        json!({"verified":true,"index_sha256":file_hash(&root.join("index.sqlite3"))?,
        "stats":stats(connection)?}),
    )
}

fn run() -> Result<Value> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if arguments.len() < 2 {
        return Err(
            "usage: aasb-corpus <build|stats|catalogue|search|paragraph|page|verify-quote|verify> SNAPSHOT [arguments]"
                .into(),
        );
    }
    let root = Path::new(&arguments[1]);
    if arguments[0] == "build" && arguments.len() == 2 {
        return build(root);
    }
    let connection = open(root)?;
    match arguments[0].as_str() {
        "stats" if arguments.len() == 2 => stats(&connection),
        "catalogue" => catalogue(&connection, &QueryOptions::parse(&arguments[2..])?),
        "search" if arguments.len() >= 3 => search(
            &connection,
            &arguments[2],
            &QueryOptions::parse(&arguments[3..])?,
        ),
        "paragraph" if arguments.len() == 4 => paragraph(&connection, &arguments[2], &arguments[3]),
        "page" if arguments.len() == 4 => page(&connection, &arguments[2], arguments[3].parse()?),
        "verify" if arguments.len() == 2 => verify(root, &connection),
        "verify-quote" if arguments.len() == 4 => {
            verify_quote(&connection, &arguments[2], None, &arguments[3])
        }
        "verify-quote" if arguments.len() == 5 => verify_quote(
            &connection,
            &arguments[2],
            Some(arguments[3].parse()?),
            &arguments[4],
        ),
        _ => Err("invalid command or arguments".into()),
    }
}

fn main() {
    match run() {
        Ok(value) => println!(
            "{}",
            serde_json::to_string_pretty(&value).expect("JSON output")
        ),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Fixture snapshot folder, removed when the test finishes.
    struct Fixture(std::path::PathBuf);
    impl std::ops::Deref for Fixture {
        type Target = Path;
        fn deref(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn fixture() -> Fixture {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-data")
            .join(format!("{}-{unique}", std::process::id()));
        fs::create_dir_all(root.join("source")).unwrap();
        let content =
            b"Fabricated source bytes from the Croft Manor library for an index contract test";
        let sha = hash(content);
        fs::write(root.join("source/sample.pdf"), content).unwrap();
        let sources = json!({"files":[{"filename":"sample.pdf","source_url":"https://example.invalid/sample.pdf","sha256":sha,"page_count":1}]});
        fs::write(
            root.join("source/manifest.json"),
            serde_json::to_vec(&sources).unwrap(),
        )
        .unwrap();
        let folder = root.join("extracted").join(&sha);
        fs::create_dir_all(&folder).unwrap();
        let pages = "{\"page\":1,\"text\":\"A fabricated lease liability\",\"provenance\":\"native\",\"needs_review\":false}\n";
        fs::write(folder.join("pages.jsonl"), pages).unwrap();
        let extraction = json!({"source_manifest_sha256":file_hash(&root.join("source/manifest.json")).unwrap(),
            "documents":[{"sha256":sha,"page_count":1,"pages_sha256":hash(pages.as_bytes())}],"failures":[]});
        fs::write(
            root.join("extracted/manifest.json"),
            serde_json::to_vec(&extraction).unwrap(),
        )
        .unwrap();
        fs::write(
            root.join("catalogue.json"),
            "{\"versions\":[],\"amendments\":[]}",
        )
        .unwrap();
        Fixture(root)
    }
    #[test]
    fn complete_build_verifies_and_refuses_to_replace_an_index() {
        let root = fixture();
        assert_eq!(build(&root).unwrap()["pages"], 1);
        let connection = open(&root).unwrap();
        let verified = verify(&root, &connection).unwrap();
        assert_eq!(verified["verified"], true);
        assert_eq!(
            verified["index_sha256"],
            file_hash(&root.join("index.sqlite3")).unwrap()
        );
        assert!(build(&root).is_err());
        fs::write(root.join("source/sample.pdf"), b"Changed source").unwrap();
        assert!(verify(&root, &connection).is_err());
    }
    #[test]
    fn changed_extraction_cannot_publish_an_index() {
        let root = fixture();
        let extraction: Extraction = read_json(&root.join("extracted/manifest.json")).unwrap();
        fs::write(
            root.join("extracted")
                .join(&extraction.documents[0].sha256)
                .join("pages.jsonl"),
            "changed",
        )
        .unwrap();
        assert!(build(&root).is_err());
        assert!(!root.join("index.sqlite3").exists());
        // A failed build must not leave a staging file that blocks a corrected rebuild.
        assert!(!root.join("index.sqlite3.partial").exists());
    }
    fn grade(quote: &str, text: &str) -> Value {
        grade_quote(quote, text).unwrap()["grade"].clone()
    }
    #[test]
    fn quotation_is_case_sensitive_and_rejects_blank() {
        assert_eq!(grade("deduct from", "You can deduct from income"), "exact");
        assert_eq!(grade("Deduct from", "You can deduct from income"), "none");
        assert!(grade_quote("  ", "text").is_err());
    }
    #[test]
    fn compatibility_matching_preserves_the_tax_corpus_contract() {
        assert_eq!(grade("40-1", "40\u{2011}1"), "normalised");
        assert_eq!(grade("office", "of\u{fb01}ce"), "normalised");
        assert_eq!(
            grade("can deduct from", "You can\n deduct  from"),
            "normalised"
        );
    }
    #[test]
    fn partial_grades_report_the_longest_run_of_quotation_words() {
        let text = "A fabricated lessee measures the lease liability at cost.";
        let partial = grade_quote(
            "Always: a lessee measures the lease liability at fair value.",
            text,
        )
        .unwrap();
        assert_eq!(partial["grade"], "partial");
        assert_eq!(partial["matched_words"], 6);
        assert_eq!(partial["quote_words"], 10);
        assert_eq!(partial["first_matched_word"], 3);
        assert_eq!(grade("the lease liability differs", text), "none");
    }

    fn quote_fixture() -> (Connection, String) {
        let connection = Connection::open_in_memory().unwrap();
        schema(&connection).unwrap();
        let sha = "e".repeat(64);
        connection
            .execute("INSERT INTO documents VALUES(?1,3,'[]','[]',?1)", [&sha])
            .unwrap();
        for (number, text, provenance) in [
            (1, "A fabricated lease\nliability is measured.", "native"),
            (2, "A fabricated lease liability is measured.", "native"),
            (3, "A fabricated lease liability is measured.", "ocr"),
        ] {
            connection.execute("INSERT INTO pages(document_sha256,page,text,text_sha256,provenance,needs_review) VALUES(?1,?2,?3,?4,?5,?6)",
                params![sha,number,text,hash(text.as_bytes()),provenance,provenance == "ocr"]).unwrap();
        }
        (connection, sha)
    }
    #[test]
    fn only_exact_matches_on_a_named_native_page_are_verified() {
        let (connection, sha) = quote_fixture();
        let quote = "fabricated lease liability";
        let exact = verify_quote(&connection, &sha, Some(2), quote).unwrap();
        assert_eq!(exact["grade"], "exact");
        assert_eq!(exact["verified"], true);
        assert_eq!(exact["matches"], true);
        assert_eq!(exact["pdf_page"], 2);
        assert_eq!(
            exact["page_text_sha256"],
            hash(b"A fabricated lease liability is measured.")
        );
        assert_eq!(exact["document_sha256"], sha);
        for (page, grade) in [(1, "normalised"), (3, "exact")] {
            let found = verify_quote(&connection, &sha, Some(page), quote).unwrap();
            assert_eq!(found["grade"], grade);
            assert_eq!(found["verified"], false);
            assert_eq!(found["matches"], false);
        }
        let searched = verify_quote(&connection, &sha, None, quote).unwrap();
        assert_eq!(searched["grade"], "exact");
        assert_eq!(searched["verified"], false);
        assert_eq!(searched["located_page_count"], 2);
        assert_eq!(searched["pages"][0]["pdf_page"], 2);
        let missing = verify_quote(&connection, &sha, None, "an unrelated quotation").unwrap();
        assert_eq!(missing["grade"], "none");
        assert_eq!(missing["pages"], json!([]));
        assert!(verify_quote(&connection, &sha, Some(4), quote).is_err());
        connection
            .execute("UPDATE pages SET text='changed' WHERE page=2", [])
            .unwrap();
        assert!(verify_quote(&connection, &sha, Some(2), quote).is_err());
    }
    #[test]
    fn source_names_cannot_escape_the_snapshot() {
        for name in ["", "..", "../a.pdf", "a/b.pdf", "a\\b.pdf", "C:foo"] {
            assert!(filename(name).is_err());
        }
        assert_eq!(filename("001-source.pdf").unwrap(), "001-source.pdf");
    }
    #[test]
    fn fts_search_and_page_lookup_preserve_citations() {
        let connection = Connection::open_in_memory().unwrap();
        schema(&connection).unwrap();
        let sha = "a".repeat(64);
        connection.execute("INSERT INTO documents VALUES(?1,1,?2,'[]',?1)",params![sha,"[{\"filename\":\"sample.pdf\",\"source_url\":\"https://example.invalid/sample.pdf\"}]"]).unwrap();
        connection
            .execute(
                "INSERT INTO pages(document_sha256,page,text,text_sha256,provenance,needs_review)
            VALUES(?1,1,'A lease liability is measured here',?2,'native',0)",
                params![sha, hash(b"A lease liability is measured here")],
            )
            .unwrap();
        connection
            .execute("INSERT INTO pages_fts(pages_fts) VALUES('rebuild')", [])
            .unwrap();
        let found = search(&connection, "\"lease liability\"", &QueryOptions::default()).unwrap();
        assert_eq!(found["results"][0]["pdf_page"], 1);
        assert_eq!(found["results"][0]["sources"][0]["filename"], "sample.pdf");
        assert_eq!(
            page(&connection, &sha, 1).unwrap()["text"],
            "A lease liability is measured here"
        );
        assert!(page(&connection, &sha, 2).is_err());
        assert!(search(&connection, "   ", &QueryOptions::default()).is_err());
        assert!(search(&connection, "\"", &QueryOptions::default()).is_err());
    }
    #[test]
    fn whitespace_pages_are_counted_for_review() {
        let connection = Connection::open_in_memory().unwrap();
        schema(&connection).unwrap();
        let sha = "b".repeat(64);
        connection
            .execute("INSERT INTO documents VALUES(?1,1,'[]','[]',?1)", [&sha])
            .unwrap();
        connection
            .execute(
                "INSERT INTO pages(document_sha256,page,text,text_sha256,provenance,needs_review)
            VALUES(?1,1,?2,?3,'native',1)",
                params![sha, "\n\t\u{a0}", hash("\n\t\u{a0}".as_bytes())],
            )
            .unwrap();
        assert_eq!(stats(&connection).unwrap()["empty_pages"], 1);
    }

    fn search_fixture() -> Connection {
        let connection = Connection::open_in_memory().unwrap();
        schema(&connection).unwrap();
        for (id, standard) in [('a', "AASB 1"), ('b', "AASB 16"), ('c', "AASB 16")] {
            let sha = id.to_string().repeat(64);
            let metadata = json!([{"standard":standard,"version_label":format!("Fabricated {id}"),"applies_to_periods_text":"Fabricated period"}]);
            connection
                .execute(
                    "INSERT INTO documents VALUES(?1,3,'[]',?2,?1)",
                    params![sha, metadata.to_string()],
                )
                .unwrap();
            for number in 1..=3 {
                let text = "35 A fabricated lease liability requirement.\n36 Next requirement.";
                connection.execute("INSERT INTO pages(document_sha256,page,text,text_sha256,provenance,needs_review) VALUES(?1,?2,?3,?4,'native',0)",
                    params![sha,number,text,hash(text.as_bytes())]).unwrap();
            }
        }
        connection
            .execute("INSERT INTO pages_fts(pages_fts) VALUES('rebuild')", [])
            .unwrap();
        connection
    }

    #[test]
    fn standard_and_exact_version_filters_apply_before_pagination() {
        let connection = search_fixture();
        let mut options = QueryOptions {
            standard: Some("aasb16".into()),
            limit: 1,
            ..Default::default()
        };
        let first = search(&connection, "lease", &options).unwrap();
        assert_eq!(first["results"][0]["document_sha256"], "b".repeat(64));
        assert_eq!(
            first["results"][0]["version_metadata"][0]["standard"],
            "AASB 16"
        );
        assert_eq!(first["next_offset"], 1);
        options.version = Some("c".repeat(64));
        assert_eq!(
            catalogue(&connection, &options).unwrap()["total_documents"],
            1
        );
        assert_eq!(
            search(&connection, "lease", &options).unwrap()["results"][0]["document_sha256"],
            "c".repeat(64)
        );
        options.standard = Some("AASB 1".into());
        assert_eq!(
            search(&connection, "lease", &options).unwrap()["results"],
            json!([])
        );
        options.version = Some("d".repeat(64));
        assert!(search(&connection, "lease", &options).is_err());
    }

    #[test]
    fn pagination_visits_every_filtered_result_once_and_signals_the_end() {
        let connection = search_fixture();
        let mut options = QueryOptions {
            standard: Some("AASB 16".into()),
            limit: 2,
            ..Default::default()
        };
        let mut identities = std::collections::BTreeSet::new();
        loop {
            let found = search(&connection, "lease", &options).unwrap();
            for result in found["results"].as_array().unwrap() {
                assert!(identities.insert((
                    result["document_sha256"].as_str().unwrap().to_owned(),
                    result["pdf_page"].as_u64().unwrap()
                )));
            }
            if let Some(next) = found["next_offset"].as_i64() {
                options.offset = next;
            } else {
                assert_eq!(found["has_more"], false);
                break;
            }
        }
        assert_eq!(identities.len(), 6);
        options.offset = 6;
        let end = search(&connection, "lease", &options).unwrap();
        assert_eq!(end["results"], json!([]));
        assert_eq!(end["next_offset"], Value::Null);
        let options = QueryOptions {
            limit: 2,
            ..Default::default()
        };
        assert_eq!(catalogue(&connection, &options).unwrap()["next_offset"], 2);
        assert_eq!(
            paragraph(&connection, &"b".repeat(64), "35").unwrap()["status"],
            "ambiguous"
        );
    }

    #[test]
    fn query_options_reject_invalid_or_ambiguous_arguments() {
        let parse = |args: &[&str]| {
            QueryOptions::parse(&args.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>())
        };
        for args in [
            vec!["--limit", "0"],
            vec!["--limit", "101"],
            vec!["--offset", "-1"],
            vec!["--offset", "9223372036854775807"],
            vec!["--limit"],
            vec!["--version", "bad"],
            vec!["--standard", " "],
            vec!["--unknown", "x"],
            vec!["--limit", "1", "--limit", "2"],
        ] {
            assert!(parse(&args).is_err(), "{args:?}");
        }
        let options = parse(&["--standard", "AASB 16", "--limit", "3", "--offset", "2"]).unwrap();
        assert_eq!(options.limit, 3);
        assert_eq!(options.offset, 2);
    }
}
