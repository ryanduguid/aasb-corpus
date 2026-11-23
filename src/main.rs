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

mod derived;
mod mcp;
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
        CREATE VIRTUAL TABLE pages_stem USING fts5(text, content='pages', content_rowid='id',
            tokenize='porter unicode61');
        CREATE TABLE paragraphs(id INTEGER PRIMARY KEY,
            document_sha256 TEXT NOT NULL REFERENCES documents(sha256), ordinal INTEGER NOT NULL,
            label TEXT NOT NULL, pdf_page INTEGER NOT NULL, needs_review INTEGER NOT NULL,
            candidate TEXT NOT NULL, text TEXT NOT NULL, UNIQUE(document_sha256,ordinal));
        CREATE INDEX paragraphs_label ON paragraphs(document_sha256,label);
        CREATE VIRTUAL TABLE paragraphs_fts USING fts5(text, content='paragraphs', content_rowid='id');
        CREATE VIRTUAL TABLE paragraphs_stem USING fts5(text, content='paragraphs', content_rowid='id',
            tokenize='porter unicode61');
        CREATE TABLE document_facts(document_sha256 TEXT PRIMARY KEY REFERENCES documents(sha256),
            validity TEXT NOT NULL, reporting_tier TEXT NOT NULL);
        CREATE TABLE citations(id INTEGER PRIMARY KEY,
            citing_document_sha256 TEXT NOT NULL REFERENCES documents(sha256),
            pdf_page INTEGER NOT NULL, line INTEGER NOT NULL, cited_standard TEXT NOT NULL,
            cited_label TEXT NOT NULL, kind TEXT NOT NULL);
        CREATE INDEX citations_cited ON citations(cited_standard,cited_label);
        PRAGMA user_version=4;
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
    let mut grouped = group_sources(root, &sources)?;
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
        page_count += index_document(&transaction, root, &catalogue, document, &aliases)?;
    }
    finish_tables(&transaction, &catalogue)?;
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

/// Builds and checks the search tables, then stores the facts that need every document.
fn finish_tables(connection: &Connection, catalogue: &Value) -> Result<()> {
    for table in [
        "pages_fts",
        "pages_stem",
        "paragraphs_fts",
        "paragraphs_stem",
    ] {
        connection.execute(
            &format!("INSERT INTO {table}({table}) VALUES('rebuild')"),
            [],
        )?;
        connection.execute(
            &format!("INSERT INTO {table}({table}) VALUES('integrity-check')"),
            [],
        )?;
    }
    insert_facts(connection, catalogue)
}

fn group_sources<'a>(
    root: &Path,
    sources: &'a Sources,
) -> Result<BTreeMap<&'a str, Vec<&'a Source>>> {
    let mut grouped: BTreeMap<&str, Vec<&Source>> = BTreeMap::new();
    for source in &sources.files {
        valid_sha(&source.sha256)?;
        if file_hash(&root.join("source").join(filename(&source.filename)?))? != source.sha256 {
            return Err(format!("source changed: {}", source.filename).into());
        }
        grouped.entry(&source.sha256).or_default().push(source);
    }
    Ok(grouped)
}

/// Inserts the document's pages and returns them in page order.
fn insert_pages(
    connection: &Connection,
    document: &Document,
    page_path: &Path,
) -> Result<Vec<Page>> {
    let mut pages = Vec::new();
    let mut expected = 1;
    for line in BufReader::new(File::open(page_path)?).lines() {
        let page: Page = serde_json::from_str(&line?)?;
        if page.page != expected {
            return Err("missing, duplicated or reordered page".into());
        }
        connection.execute(
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
        pages.push(page);
        expected += 1;
    }
    if expected != document.page_count + 1 || file_hash(page_path)? != document.pages_sha256 {
        return Err("page coverage differs or extraction changed during indexing".into());
    }
    Ok(pages)
}

/// Stores every anchored paragraph with the candidate `paragraph` reports for it.
fn insert_paragraphs(connection: &Connection, sha: &str, pages: &[Page]) -> Result<()> {
    let mut statement = connection.prepare(
        "INSERT INTO paragraphs(document_sha256,ordinal,label,pdf_page,needs_review,candidate,text)
        VALUES (?1,?2,?3,?4,?5,?6,?7)",
    )?;
    for (ordinal, anchored) in paragraphs::index(pages).into_iter().enumerate() {
        let candidate = &anchored.candidate;
        let text = candidate["fragments"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|fragment| fragment["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
        statement.execute(params![
            sha,
            i64::try_from(ordinal)?,
            anchored.label,
            anchored.pdf_page,
            candidate["needs_review"] == true,
            candidate.to_string(),
            text
        ])?;
    }
    Ok(())
}

fn index_document(
    connection: &Connection,
    root: &Path,
    catalogue: &Value,
    document: &Document,
    aliases: &[&Source],
) -> Result<u64> {
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
    let metadata = metadata_for(catalogue, aliases);
    connection.execute(
        "INSERT INTO documents VALUES (?1,?2,?3,?4,?5)",
        params![
            document.sha256,
            document.page_count,
            serde_json::to_string(&alias_json)?,
            serde_json::to_string(&metadata)?,
            document.pages_sha256
        ],
    )?;
    let pages = insert_pages(connection, document, &page_path)?;
    insert_paragraphs(connection, &document.sha256, &pages)?;
    insert_citations(connection, &document.sha256, &metadata, &pages)?;
    Ok(u64::from(document.page_count))
}

/// Stores each paragraph the document's pages cite, with the kind of citing document.
fn insert_citations(
    connection: &Connection,
    sha: &str,
    metadata: &[Value],
    pages: &[Page],
) -> Result<()> {
    let mut statement = connection.prepare(
        "INSERT INTO citations(citing_document_sha256,pdf_page,line,cited_standard,cited_label,kind)
        VALUES (?1,?2,?3,?4,?5,?6)",
    )?;
    let kind = derived::kind(metadata);
    for citation in derived::citations(pages) {
        statement.execute(params![
            sha,
            citation.pdf_page,
            i64::try_from(citation.line)?,
            standard_key(&citation.standard),
            citation.label,
            kind
        ])?;
    }
    Ok(())
}

/// The principal standards a document's catalogue rows name, as `standard_key`s.
fn standard_keys(metadata: &[Value]) -> Vec<String> {
    metadata
        .iter()
        .filter_map(|row| row["standard"].as_str())
        .map(standard_key)
        .collect()
}

/// Stores each document's validity and reporting tier. A version is superseded by the
/// versions of the same standard whose stated periods begin on its stated end date.
fn insert_facts(connection: &Connection, catalogue: &Value) -> Result<()> {
    let mut statement = connection.prepare("SELECT sha256,metadata FROM documents")?;
    let mut facts = Vec::new();
    for row in statement.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
        let (sha, metadata) = row?;
        let metadata: Vec<Value> = serde_json::from_str(&metadata)?;
        let validity = derived::validity(catalogue, &metadata);
        facts.push((
            sha,
            standard_keys(&metadata),
            validity,
            derived::reporting_tier(catalogue, &metadata),
        ));
    }
    for index in 0..facts.len() {
        let (_, keys, validity, _) = &facts[index];
        let successors: Vec<&String> = facts
            .iter()
            .filter(|(_, other_keys, other, _)| {
                validity["applies_before"].is_string()
                    && other["applies_from"] == validity["applies_before"]
                    && other_keys.iter().any(|key| keys.contains(key))
            })
            .map(|(sha, _, _, _)| sha)
            .collect();
        let successors = json!(successors);
        facts[index].2["superseded_by"] = successors;
    }
    for (sha, _, validity, tier) in facts {
        connection.execute(
            "INSERT INTO document_facts VALUES (?1,?2,?3)",
            params![sha, validity.to_string(), tier.to_string()],
        )?;
    }
    Ok(())
}

/// A document's stored validity and reporting tier; `None` before schema 4.
fn document_facts(connection: &Connection, sha: &str) -> Result<Option<(Value, Value)>> {
    if schema_version(connection)? < 4 {
        return Ok(None);
    }
    let (validity, tier): (String, String) = connection.query_row(
        "SELECT validity,reporting_tier FROM document_facts WHERE document_sha256=?1",
        [sha],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok(Some((
        serde_json::from_str(&validity)?,
        serde_json::from_str(&tier)?,
    )))
}

const DERIVED_BOUNDARY: &str = "validity restates the catalogue's period text against its retrieval date (as_at); it does not decide which version applies to a reporting period. reporting_tier is what a catalogue title states, and null where no title states one. cited_by lists pages of other standards' documents whose text cites this standard and label; a citation names no version, and a range cites only its endpoints.";

/// The pages of other documents whose text cites `label` of this document's standard,
/// grouped by the citing document's kind; at most 20 of each kind.
fn cited_by(connection: &Connection, metadata: &[Value], label: &str) -> Result<Value> {
    let keys = standard_keys(metadata);
    let mut statement = connection.prepare(
        "SELECT c.citing_document_sha256,c.pdf_page,c.line,c.kind,d.metadata FROM citations c
        JOIN documents d ON d.sha256=c.citing_document_sha256
        WHERE c.cited_standard IN (SELECT value FROM json_each(?1)) AND c.cited_label=?2
        ORDER BY c.kind,c.citing_document_sha256,c.pdf_page,c.line",
    )?;
    let mut groups: BTreeMap<&str, Vec<Value>> = [
        "interpretation",
        "agenda_decision",
        "amending_standard",
        "standard",
        "other",
    ]
    .into_iter()
    .map(|kind| (kind, Vec::new()))
    .collect();
    let mut total = 0;
    let rows = statement.query_map(params![serde_json::to_string(&keys)?, label], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, u32>(1)?,
            r.get::<_, u32>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
        ))
    })?;
    for row in rows {
        let (sha, page, line, kind, citing) = row?;
        let citing: Vec<Value> = serde_json::from_str(&citing)?;
        // Other versions of the same standard cite their own paragraphs.
        if standard_keys(&citing).iter().any(|key| keys.contains(key)) {
            continue;
        }
        total += 1;
        let title = citing
            .first()
            .map(|row| row.get("version_label").unwrap_or(&row["title"]).clone());
        if let Some(group) = groups
            .get_mut(kind.as_str())
            .filter(|group| group.len() < 20)
        {
            group.push(json!({"document_sha256":sha,"pdf_page":page,"line":line,"title":title}));
        }
    }
    let shown: usize = groups.values().map(Vec::len).sum();
    Ok(
        json!({"interpretations":groups["interpretation"],"agenda_decisions":groups["agenda_decision"],
        "amending_standards":groups["amending_standard"],"standards":groups["standard"],
        "other":groups["other"],"total":total,"truncated":shown < total}),
    )
}

fn schema_version(connection: &Connection) -> Result<i64> {
    Ok(connection.query_row("PRAGMA user_version", [], |row| row.get(0))?)
}

fn open(root: &Path) -> Result<Connection> {
    let connection =
        Connection::open_with_flags(root.join("index.sqlite3"), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    // Version 1 lacks the stem table and version 2 the paragraph table; both stay usable,
    // so citations keep their index digests.
    if !(1..=4).contains(&schema_version(&connection)?) {
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
    let mut result = json!({"documents":count("SELECT count(*) FROM documents")?,
        "pages":count("SELECT count(*) FROM pages")?,
        "review_pages":count("SELECT count(*) FROM pages WHERE needs_review=1")?,
        "empty_pages":empty_pages});
    if schema_version(connection)? >= 3 {
        result["paragraphs"] = json!(count("SELECT count(*) FROM paragraphs")?);
    }
    Ok(result)
}

#[derive(Debug)]
struct QueryOptions {
    standard: Option<String>,
    version: Option<String>,
    limit: u32,
    offset: i64,
    stem: bool,
    paragraphs: bool,
}

impl Default for QueryOptions {
    fn default() -> Self {
        Self {
            standard: None,
            version: None,
            limit: 20,
            offset: 0,
            stem: false,
            paragraphs: false,
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
        let mut document = json!({"document_sha256":sha,"page_count":count,
            "sources":serde_json::from_str::<Value>(&aliases)?,"version_metadata":metadata});
        if let Some((validity, tier)) = document_facts(connection, &sha)? {
            document["validity"] = validity;
            document["reporting_tier"] = tier;
        }
        documents.push(document);
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
        "boundary":"Version identifiers are exact PDF hashes. Catalogue dates do not determine applicability; validity only restates them against the catalogue's retrieval date. Standard filters select principal documents, not related amendments."}),
    )
}

fn search(connection: &Connection, query: &str, options: &QueryOptions) -> Result<Value> {
    if query.trim().is_empty() {
        return Err("search query must not be blank".into());
    }
    let stem = options.stem;
    if stem && schema_version(connection)? < 2 {
        return Err("this index has no stem table; rebuild it as the README describes".into());
    }
    if options.paragraphs {
        require_paragraph_table(connection)?;
    }
    let unit = if options.paragraphs {
        "paragraphs"
    } else {
        "pages"
    };
    let table = format!("{unit}_{}", if stem { "stem" } else { "fts" });
    let selected = if options.standard.is_some() || options.version.is_some() {
        let documents: Vec<_> = matching_documents(connection, options)?
            .into_iter()
            .map(|document| document["document_sha256"].clone())
            .collect();
        Some(serde_json::to_string(&documents)?)
    } else {
        None
    };
    let mut hits = search_hits(connection, &table, query, selected.as_deref(), options)?;
    let has_more = hits.len() > options.limit as usize;
    hits.truncate(options.limit as usize);
    let mut result = json!({"query":query,"results":hits,"limit":options.limit,"offset":options.offset,
        "filters":options.filters(),"has_more":has_more,
        "next_offset":has_more.then_some(options.offset + i64::from(options.limit)),
        "boundary":"Search covers captured versions; it does not determine reporting-period applicability."});
    if options.paragraphs {
        result["paragraphs"] = json!(true);
    }
    if stem {
        result["stemmed"] = json!(true);
        result["stem_boundary"] = json!(
            "Stemmed hits match word forms, not the query's words. Check any quotation with verify-quote."
        );
    }
    Ok(result)
}

fn search_hits(
    connection: &Connection,
    table: &str,
    query: &str,
    selected: Option<&str>,
    options: &QueryOptions,
) -> Result<Vec<Value>> {
    // Paragraph hits name their label and position; page hits their provenance.
    let (content, page, detail, position) = if options.paragraphs {
        (
            "paragraphs",
            "p.pdf_page",
            "json_array(p.label,p.ordinal)",
            "p.ordinal",
        )
    } else {
        ("pages", "p.page", "p.provenance", "p.page")
    };
    let mut statement = connection.prepare(&format!(
        "SELECT p.document_sha256,{page},
        snippet({table},0,'[',']',' … ',40),{detail},p.needs_review,d.aliases,d.metadata
        FROM {table} JOIN {content} p ON p.id={table}.rowid
        JOIN documents d ON d.sha256=p.document_sha256
        WHERE {table} MATCH ?1
        AND (?2 IS NULL OR p.document_sha256 IN (SELECT value FROM json_each(?2)))
        ORDER BY bm25({table}),p.document_sha256,{position} LIMIT ?3 OFFSET ?4"
    ))?;
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
        let (sha, page, snippet, detail, needs_review, aliases, metadata) = row?;
        let mut hit = json!({"document_sha256":sha,"pdf_page":page,"snippet":snippet,
            "needs_review":needs_review,
            "sources":serde_json::from_str::<Value>(&aliases)?,"version_metadata":serde_json::from_str::<Value>(&metadata)?});
        if options.paragraphs {
            let detail: Value = serde_json::from_str(&detail)?;
            hit["paragraph"] = detail[0].clone();
            hit["ordinal"] = detail[1].clone();
        } else {
            hit["provenance"] = json!(detail);
        }
        hits.push(hit);
    }
    Ok(hits)
}

#[derive(Deserialize)]
struct SecondReader {
    schema_version: u32,
    reader: String,
    reader_version: String,
    source_manifest_sha256: String,
    documents: Vec<Document>,
    failures: Vec<ReaderFailure>,
}
#[derive(Deserialize)]
struct ReaderFailure {
    sha256: String,
}
#[derive(Deserialize)]
struct Reading {
    page: u32,
    text: String,
}

fn check_second_reader(root: &Path, manifest: &SecondReader) -> Result<()> {
    if manifest.schema_version != 1
        || manifest.reader != "pypdf"
        || manifest.reader_version.trim().is_empty()
    {
        return Err("unsupported second-reader manifest".into());
    }
    let source_path = root.join("source/manifest.json");
    if file_hash(&source_path)? != manifest.source_manifest_sha256 {
        return Err("second reading was made from a different source manifest".into());
    }
    let sources: Sources = read_json(&source_path)?;
    let expected: std::collections::BTreeSet<_> =
        sources.files.iter().map(|s| s.sha256.as_str()).collect();
    let mut covered = std::collections::BTreeSet::new();
    let recorded = manifest.documents.iter().map(|d| d.sha256.as_str());
    let failed = manifest.failures.iter().map(|f| f.sha256.as_str());
    if !recorded.chain(failed).all(|digest| covered.insert(digest)) || covered != expected {
        return Err("second reading does not cover each source document exactly once".into());
    }
    Ok(())
}

/// Loads one PDF's independent page texts from `tools/second_reader.py`. `None` means the
/// snapshot has no `second-reader` folder or pypdf's failure on this PDF is recorded; a
/// changed, stale, incomplete or unknown reading is an error.
fn second_reading(
    root: &Path,
    sha: &str,
    page_count: usize,
) -> Result<Option<(String, Vec<String>)>> {
    let folder = root.join("second-reader");
    if !folder.exists() {
        return Ok(None);
    }
    let manifest: SecondReader = read_json(&folder.join("manifest.json"))?;
    check_second_reader(root, &manifest)?;
    if manifest.failures.iter().any(|f| f.sha256 == sha) {
        return Ok(None);
    }
    let document = manifest
        .documents
        .iter()
        .find(|d| d.sha256 == sha)
        .ok_or("document is not in the second reading")?;
    let bytes = fs::read(folder.join(sha).join("pages.jsonl"))?;
    if hash(&bytes) != document.pages_sha256 {
        return Err("second-reader pages changed".into());
    }
    let mut pages = Vec::new();
    for line in std::str::from_utf8(&bytes)?.lines() {
        let reading: Reading = serde_json::from_str(line)?;
        if reading.page as usize != pages.len() + 1 {
            return Err("second reading has a missing, duplicated or reordered page".into());
        }
        pages.push(reading.text);
    }
    if pages.len() != page_count || document.page_count as usize != page_count {
        return Err("second-reader page coverage differs".into());
    }
    Ok(Some((
        format!("{} {}", manifest.reader, manifest.reader_version),
        pages,
    )))
}

fn aliases_metadata(connection: &Connection, sha: &str) -> Result<(String, String)> {
    valid_sha(sha)?;
    Ok(connection.query_row(
        "SELECT aliases,metadata FROM documents WHERE sha256=?1",
        [sha],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?)
}

fn require_paragraph_table(connection: &Connection) -> Result<()> {
    if schema_version(connection)? < 3 {
        return Err("this index has no paragraph table; rebuild it as the README describes".into());
    }
    Ok(())
}

/// One PDF's stored paragraphs with ordinals in `range`, as `(ordinal, label, candidate)`.
fn stored_paragraphs(
    connection: &Connection,
    sha: &str,
    label: Option<&str>,
    range: (i64, i64),
) -> Result<Vec<(i64, String, Value)>> {
    let mut statement = connection.prepare(
        "SELECT ordinal,label,candidate FROM paragraphs WHERE document_sha256=?1
        AND (?2 IS NULL OR label=?2) AND ordinal BETWEEN ?3 AND ?4 ORDER BY ordinal",
    )?;
    let rows = statement.query_map(params![sha, label, range.0, range.1], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get::<_, String>(2)?))
    })?;
    let mut paragraphs = Vec::new();
    for row in rows {
        let (ordinal, label, candidate) = row?;
        paragraphs.push((ordinal, label, serde_json::from_str(&candidate)?));
    }
    Ok(paragraphs)
}

/// Adds up to `context` stored neighbours on each side of every returned candidate.
fn add_context(
    connection: &Connection,
    sha: &str,
    result: &mut Value,
    ordinals: &[i64],
    context: i64,
) -> Result<()> {
    let candidates = result["candidates"]
        .as_array_mut()
        .ok_or("lookup has no candidates")?;
    for (candidate, ordinal) in candidates.iter_mut().zip(ordinals) {
        let mut sides = [Vec::new(), Vec::new()];
        let range = (ordinal - context, ordinal + context);
        for (neighbour, label, mut row) in stored_paragraphs(connection, sha, None, range)? {
            if neighbour != *ordinal {
                row["paragraph"] = json!(label);
                row["ordinal"] = json!(neighbour);
                sides[usize::from(neighbour > *ordinal)].push(row);
            }
        }
        let [before, after] = sides;
        candidate["context"] = json!({"ordinal":ordinal,"before":before,"after":after});
    }
    result["context_boundary"] = json!(
        "Context paragraphs are stored neighbours in reading order. The second reader does not check them; look each one up to confirm it."
    );
    Ok(())
}

/// The candidates for `label` with their stored ordinals; an index without the paragraph
/// table resolves them from its pages and has no ordinals.
fn candidates(connection: &Connection, sha: &str, label: &str) -> Result<(Value, Vec<i64>)> {
    if schema_version(connection)? >= 3 {
        let stored = stored_paragraphs(connection, sha, Some(label), (0, i64::MAX))?;
        let ordinals = stored.iter().map(|(ordinal, _, _)| *ordinal).collect();
        let found = stored.into_iter().map(|(_, _, candidate)| candidate);
        return Ok((paragraphs::resolve(label, found.collect()), ordinals));
    }
    let pages = document_pages(connection, sha)?;
    Ok((paragraphs::lookup(&pages, label)?, Vec::new()))
}

fn document_pages(connection: &Connection, sha: &str) -> Result<Vec<Page>> {
    let mut statement = connection.prepare(
        "SELECT page,text,provenance,needs_review FROM pages WHERE document_sha256=?1 ORDER BY page",
    )?;
    let pages = statement
        .query_map([sha], |r| {
            Ok(Page {
                page: r.get(0)?,
                text: r.get(1)?,
                provenance: r.get(2)?,
                needs_review: r.get(3)?,
            })
        })?
        .collect::<std::result::Result<_, _>>()?;
    Ok(pages)
}

fn paragraph(
    root: &Path,
    connection: &Connection,
    sha: &str,
    label: &str,
    context: Option<i64>,
) -> Result<Value> {
    let (aliases, metadata) = aliases_metadata(connection, sha)?;
    paragraphs::check_label(label)?;
    if context.is_some() {
        require_paragraph_table(connection)?;
    }
    let page_count: u32 = connection.query_row(
        "SELECT page_count FROM documents WHERE sha256=?1",
        [sha],
        |r| r.get(0),
    )?;
    let (mut result, ordinals) = candidates(connection, sha, label)?;
    let sources: Value = serde_json::from_str(&aliases)?;
    // A reading of replaced or removed source bytes cannot confirm the indexed text.
    let problems = if result["status"] == "found" {
        source_problems(root, sha, &sources)?
    } else {
        Vec::new()
    };
    let reading = if result["status"] == "found" && problems.is_empty() {
        second_reading(root, sha, page_count as usize)?
    } else {
        None
    };
    paragraphs::confirm(&mut result, reading.as_ref())?;
    if !problems.is_empty() {
        result["second_reader"] = json!({"status":"source_changed","reasons":problems});
    }
    if let Some(context) = context {
        add_context(connection, sha, &mut result, &ordinals, context)?;
    }
    if let Some((validity, tier)) = document_facts(connection, sha)? {
        let metadata: Vec<Value> = serde_json::from_str(&metadata)?;
        result["cited_by"] = cited_by(connection, &metadata, label)?;
        result["validity"] = validity;
        result["reporting_tier"] = tier;
        result["derived_boundary"] = json!(DERIVED_BOUNDARY);
    }
    result["document_sha256"] = json!(sha);
    result["sources"] = sources;
    result["version_metadata"] = serde_json::from_str(&metadata)?;
    Ok(result)
}

/// Removes a flag that takes no value; reports whether it was present.
fn take_flag(arguments: &mut Vec<String>, flag: &str) -> bool {
    let position = arguments.iter().position(|argument| argument == flag);
    if let Some(position) = position {
        arguments.remove(position);
    }
    position.is_some()
}

/// Parses the paragraph `--context` count: 1 to 5 neighbours on each side.
fn context_count(arguments: &[String]) -> Result<Option<i64>> {
    match arguments {
        [] => Ok(None),
        [option, count] if option == "--context" => match count.parse() {
            Ok(count @ 1..=5) => Ok(Some(count)),
            _ => Err("context must be 1 to 5 paragraphs".into()),
        },
        _ => Err("invalid paragraph option; use --context N".into()),
    }
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

/// Rehashes each source file recorded for a PDF and returns one reason per faulty file;
/// empty when every file is unchanged.
fn source_problems(root: &Path, sha: &str, aliases: &Value) -> Result<Vec<String>> {
    let names = aliases.as_array().map(Vec::as_slice).unwrap_or_default();
    if names.is_empty() {
        return Ok(vec![
            "No source PDF file is recorded for this document.".into(),
        ]);
    }
    let mut problems = Vec::new();
    for alias in names {
        let name = filename(alias["filename"].as_str().unwrap_or_default())?;
        match fs::read(root.join("source").join(name)) {
            Err(_) => problems.push(format!("Source PDF file {name} is missing or unreadable.")),
            Ok(bytes) if hash(&bytes) != sha => problems.push(format!(
                "Source PDF file {name} no longer matches the document hash."
            )),
            Ok(_) => {}
        }
    }
    Ok(problems)
}

fn graded_pages(
    connection: &Connection,
    sha: &str,
    number: Option<u32>,
    quote: &str,
) -> Result<Vec<Value>> {
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
    Ok(pages)
}

/// Only an exact match on a named native page, with every source PDF file unchanged, is
/// verified; `unverified_reasons` lists every condition that is not met.
fn verify_quote(
    root: &Path,
    connection: &Connection,
    sha: &str,
    number: Option<u32>,
    quote: &str,
) -> Result<Value> {
    let (aliases, metadata) = aliases_metadata(connection, sha)?;
    let mut pages = graded_pages(connection, sha, number, quote)?;
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
    let sources: Value = serde_json::from_str(&aliases)?;
    let mut reasons: Vec<String> = Vec::new();
    if number.is_none() {
        reasons.push("No page was named; a search only locates candidate pages.".into());
    }
    if result["grade"] != "exact" {
        reasons.push("The quotation is not an exact match.".into());
    }
    if number.is_some() && (result["provenance"] != "native" || result["needs_review"] != false) {
        reasons.push("The page is OCR text or flagged for review.".into());
    }
    reasons.extend(source_problems(root, sha, &sources)?);
    let verified = reasons.is_empty();
    result["verified"] = json!(verified);
    result["unverified_reasons"] = json!(reasons);
    // Retained for existing callers, with the stricter meaning.
    result["matches"] = json!(verified);
    result["document_sha256"] = json!(sha);
    result["sources"] = sources;
    result["version_metadata"] = serde_json::from_str(&metadata)?;
    result["boundary"] = json!(
        "Only an exact match on a named native page of the selected PDF, with its source files unchanged, is verified. Normalised and partial grades, OCR or review pages and searches without a page locate candidates for review. A text match does not establish accounting applicability."
    );
    Ok(result)
}

/// The stored paragraphs must be exactly those the indexed pages yield today.
fn check_paragraphs(connection: &Connection, sha: &str) -> Result<()> {
    let expected: Vec<_> = paragraphs::index(&document_pages(connection, sha)?)
        .into_iter()
        .map(|a| (a.label, a.pdf_page, a.candidate))
        .collect();
    let mut statement = connection.prepare(
        "SELECT label,pdf_page,candidate FROM paragraphs WHERE document_sha256=?1 ORDER BY ordinal",
    )?;
    let mut stored = Vec::new();
    for row in statement.query_map([sha], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get::<_, String>(2)?))
    })? {
        let (label, page, candidate) = row?;
        stored.push((label, page, serde_json::from_str::<Value>(&candidate)?));
    }
    if stored != expected {
        return Err("paragraph table differs from the indexed pages; rebuild the index".into());
    }
    Ok(())
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
        if schema_version(connection)? >= 3 {
            check_paragraphs(connection, &sha)?;
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
            "usage: aasb-corpus <build|stats|catalogue|search|paragraph|page|verify-quote|verify|mcp> SNAPSHOT [arguments]"
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
        "search" if arguments.len() >= 3 => {
            let mut words = arguments[3..].to_vec();
            let stem = take_flag(&mut words, "--stem");
            let paragraphs = take_flag(&mut words, "--paragraphs");
            let mut options = QueryOptions::parse(&words)?;
            options.stem = stem;
            options.paragraphs = paragraphs;
            search(&connection, &arguments[2], &options)
        }
        "paragraph" if arguments.len() >= 4 => paragraph(
            root,
            &connection,
            &arguments[2],
            &arguments[3],
            context_count(&arguments[4..])?,
        ),
        "page" if arguments.len() == 4 => page(&connection, &arguments[2], arguments[3].parse()?),
        "verify" if arguments.len() == 2 => verify(root, &connection),
        "verify-quote" if arguments.len() == 4 => {
            verify_quote(root, &connection, &arguments[2], None, &arguments[3])
        }
        "verify-quote" if arguments.len() == 5 => verify_quote(
            root,
            &connection,
            &arguments[2],
            Some(arguments[3].parse()?),
            &arguments[4],
        ),
        _ => Err("invalid command or arguments".into()),
    }
}

fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    // The server owns stdout, so it prints no result of its own.
    let outcome = if arguments.len() == 2 && arguments[0] == "mcp" {
        mcp::serve(Path::new(&arguments[1])).map(|()| None)
    } else {
        run().map(Some)
    };
    match outcome {
        Ok(Some(value)) => println!(
            "{}",
            serde_json::to_string_pretty(&value).expect("JSON output")
        ),
        Ok(None) => {}
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
    fn verify_refuses_a_paragraph_table_that_differs_from_the_pages() {
        let root = fixture();
        build(&root).unwrap();
        let connection = Connection::open(root.join("index.sqlite3")).unwrap();
        assert_eq!(verify(&root, &connection).unwrap()["verified"], true);
        // The fixture page has no paragraph anchor, so any stored row is stale.
        connection
            .execute(
                "INSERT INTO paragraphs(document_sha256,ordinal,label,pdf_page,needs_review,candidate,text)
                SELECT sha256,0,'1',1,0,'{}','' FROM documents",
                [],
            )
            .unwrap();
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

    /// Fixture snapshot whose only source PDF is indexed with 3 fabricated pages.
    fn quote_fixture() -> (Fixture, Connection, String) {
        let root = fixture();
        let sources: Sources = read_json(&root.join("source/manifest.json")).unwrap();
        let sha = sources.files[0].sha256.clone();
        let connection = Connection::open_in_memory().unwrap();
        schema(&connection).unwrap();
        connection
            .execute(
                "INSERT INTO documents VALUES(?1,3,'[{\"filename\":\"sample.pdf\"}]','[]',?1)",
                [&sha],
            )
            .unwrap();
        for (number, text, provenance) in [
            (1, "A fabricated lease\nliability is measured.", "native"),
            (2, "A fabricated lease liability is measured.", "native"),
            (3, "A fabricated lease liability is measured.", "ocr"),
        ] {
            connection.execute("INSERT INTO pages(document_sha256,page,text,text_sha256,provenance,needs_review) VALUES(?1,?2,?3,?4,?5,?6)",
                params![sha,number,text,hash(text.as_bytes()),provenance,provenance == "ocr"]).unwrap();
        }
        (root, connection, sha)
    }
    #[test]
    fn only_exact_matches_on_a_named_native_page_are_verified() {
        let (root, connection, sha) = quote_fixture();
        let verify_quote = |page, quote| verify_quote(&root, &connection, &sha, page, quote);
        let quote = "fabricated lease liability";
        let exact = verify_quote(Some(2), quote).unwrap();
        assert_eq!(exact["grade"], "exact");
        assert_eq!(exact["verified"], true);
        assert_eq!(exact["unverified_reasons"], json!([]));
        assert_eq!(exact["matches"], true);
        assert_eq!(exact["pdf_page"], 2);
        assert_eq!(
            exact["page_text_sha256"],
            hash(b"A fabricated lease liability is measured.")
        );
        assert_eq!(exact["document_sha256"], sha);
        for (page, grade) in [(1, "normalised"), (3, "exact")] {
            let found = verify_quote(Some(page), quote).unwrap();
            assert_eq!(found["grade"], grade);
            assert_eq!(found["verified"], false);
            assert_eq!(found["matches"], false);
            assert_eq!(found["unverified_reasons"].as_array().unwrap().len(), 1);
        }
        let searched = verify_quote(None, quote).unwrap();
        assert_eq!(searched["grade"], "exact");
        assert_eq!(searched["verified"], false);
        assert_eq!(searched["located_page_count"], 2);
        assert_eq!(searched["pages"][0]["pdf_page"], 2);
        let missing = verify_quote(None, "an unrelated quotation").unwrap();
        assert_eq!(missing["grade"], "none");
        assert_eq!(missing["pages"], json!([]));
        assert!(verify_quote(Some(4), quote).is_err());
        connection
            .execute("UPDATE pages SET text='changed' WHERE page=2", [])
            .unwrap();
        assert!(verify_quote(Some(2), quote).is_err());
    }
    #[test]
    fn a_missing_or_replaced_source_pdf_is_never_verified() {
        let (root, connection, sha) = quote_fixture();
        let checked =
            || verify_quote(&root, &connection, &sha, Some(2), "lease liability").unwrap();
        assert_eq!(checked()["verified"], true);
        let source = root.join("source/sample.pdf");
        fs::write(&source, b"Replaced fabricated bytes").unwrap();
        let replaced = checked();
        assert_eq!(replaced["verified"], false);
        assert_eq!(replaced["matches"], false);
        assert!(
            replaced["unverified_reasons"][0]
                .as_str()
                .unwrap()
                .contains("no longer matches")
        );
        fs::remove_file(&source).unwrap();
        let removed = checked();
        assert_eq!(removed["verified"], false);
        assert!(
            removed["unverified_reasons"][0]
                .as_str()
                .unwrap()
                .contains("missing")
        );
    }
    #[test]
    fn every_missing_or_replaced_source_file_is_reported() {
        let (root, connection, sha) = quote_fixture();
        connection
            .execute(
                "UPDATE documents SET aliases='[{\"filename\":\"sample.pdf\"},{\"filename\":\"copy.pdf\"}]'",
                [],
            )
            .unwrap();
        fs::write(root.join("source/sample.pdf"), b"Replaced fabricated bytes").unwrap();
        let checked = verify_quote(&root, &connection, &sha, Some(2), "lease liability").unwrap();
        assert_eq!(checked["verified"], false);
        assert_eq!(
            checked["unverified_reasons"],
            json!([
                "Source PDF file sample.pdf no longer matches the document hash.",
                "Source PDF file copy.pdf is missing or unreadable."
            ])
        );
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
    fn paragraph_fixture() -> (Fixture, Connection, String) {
        let root = fixture();
        let sources: Sources = read_json(&root.join("source/manifest.json")).unwrap();
        let sha = sources.files[0].sha256.clone();
        let connection = Connection::open_in_memory().unwrap();
        schema(&connection).unwrap();
        let text = "35 A fabricated requirement.\n36 Next requirement.";
        connection
            .execute(
                "INSERT INTO documents VALUES(?1,1,'[{\"filename\":\"sample.pdf\"}]','[]',?1)",
                [&sha],
            )
            .unwrap();
        connection.execute("INSERT INTO pages(document_sha256,page,text,text_sha256,provenance,needs_review) VALUES(?1,1,?2,?3,'native',0)",
            params![sha, text, hash(text.as_bytes())]).unwrap();
        store_paragraphs(&connection);
        (root, connection, sha)
    }
    fn lookup_35(root: &Path, connection: &Connection, sha: &str) -> Result<Value> {
        paragraph(root, connection, sha, "35", None)
    }
    /// Stores the paragraphs and document facts of what was inserted so far, as `build` does.
    fn store_paragraphs(connection: &Connection) {
        connection
            .execute_batch("DELETE FROM paragraphs; DELETE FROM document_facts;")
            .unwrap();
        insert_facts(connection, &json!({})).unwrap();
        let mut statement = connection.prepare("SELECT sha256 FROM documents").unwrap();
        let documents: Vec<String> = statement
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        for sha in documents {
            let pages = document_pages(connection, &sha).unwrap();
            insert_paragraphs(connection, &sha, &pages).unwrap();
        }
    }
    #[test]
    fn paragraph_lookup_requires_an_unchanged_second_reading() {
        let (root, connection, sha) = paragraph_fixture();
        let source_manifest = root.join("source/manifest.json");
        let status = || lookup_35(&root, &connection, &sha).map(|r| r["status"].clone());
        assert_eq!(status().unwrap(), "found_unconfirmed");
        let folder = root.join("second-reader");
        fs::create_dir_all(folder.join(&sha)).unwrap();
        let entry = |digest: &str| json!([{"sha256":sha,"page_count":1,"pages_sha256":digest}]);
        let write = |pages: &str, change: &dyn Fn(&mut Value)| {
            fs::write(folder.join(&sha).join("pages.jsonl"), pages).unwrap();
            let mut manifest = json!({"schema_version":1,"reader":"pypdf","reader_version":"0-test",
                "source_manifest_sha256":file_hash(&source_manifest).unwrap(),
                "documents":entry(&hash(pages.as_bytes())),"failures":[]});
            change(&mut manifest);
            fs::write(folder.join("manifest.json"), manifest.to_string()).unwrap();
        };
        let reading = "{\"page\":1,\"text\":\"35 A fabri cated requirement. 36 Next\"}\n";
        write(reading, &|_| {});
        assert_eq!(status().unwrap(), "found");
        let other = "{\"page\":1,\"text\":\"35 Another requirement. 36 Next\"}\n";
        write(other, &|_| {});
        assert_eq!(status().unwrap(), "needs_review");
        // A recorded pypdf failure for this PDF is the only other unavailable case.
        write(reading, &|m| {
            m["documents"] = json!([]);
            m["failures"] = json!([{"filename":"sample.pdf","sha256":sha,"error":"fabricated"}]);
        });
        assert_eq!(status().unwrap(), "found_unconfirmed");
        let stale: [&dyn Fn(&mut Value); 7] = [
            &|m| m["documents"] = entry(&hash(other.as_bytes())),
            &|m| m["documents"] = json!([]),
            &|m| m["failures"] = json!([{"sha256":sha}]),
            &|m| m["schema_version"] = json!(2),
            &|m| m["reader"] = json!("another reader"),
            &|m| m["source_manifest_sha256"] = json!("0".repeat(64)),
            &|m| m["documents"][0]["page_count"] = json!(2),
        ];
        for change in stale {
            write(reading, change);
            assert!(status().is_err());
        }
        let two = format!("{reading}{{\"page\":2,\"text\":\"\"}}\n");
        write(&two, &|_| {});
        assert!(status().is_err());
        write(reading, &|_| {});
        let original = fs::read(&source_manifest).unwrap();
        fs::write(&source_manifest, [original.as_slice(), b" "].concat()).unwrap();
        assert!(status().is_err());
        fs::write(&source_manifest, original).unwrap();
        assert_eq!(status().unwrap(), "found");
        fs::remove_file(folder.join("manifest.json")).unwrap();
        assert!(status().is_err());
    }
    #[test]
    fn source_problems_are_reported_when_a_wordless_fragment_needs_review() {
        let (root, connection, sha) = paragraph_fixture();
        let first = "35 A fabricated requirement.";
        connection
            .execute(
                "UPDATE pages SET text=?1,text_sha256=?2",
                params![first, hash(first.as_bytes())],
            )
            .unwrap();
        let second = "--\n36 Next requirement.";
        connection.execute("INSERT INTO pages(document_sha256,page,text,text_sha256,provenance,needs_review) VALUES(?1,2,?2,?3,'native',0)",
            params![sha, second, hash(second.as_bytes())]).unwrap();
        store_paragraphs(&connection);
        fs::write(root.join("source/sample.pdf"), b"Replaced fabricated bytes").unwrap();
        let result = paragraph(&root, &connection, &sha, "35", None).unwrap();
        assert_eq!(result["candidates"][0]["fragments"][1]["text"], "--");
        assert_eq!(result["status"], "needs_review");
        assert_eq!(result["second_reader"]["status"], "source_changed");
        assert_eq!(
            result["second_reader"]["reasons"],
            json!(["Source PDF file sample.pdf no longer matches the document hash."])
        );
    }
    #[test]
    fn a_replaced_or_removed_source_pdf_is_never_confirmed() {
        let (root, connection, sha) = paragraph_fixture();
        let folder = root.join("second-reader");
        fs::create_dir_all(folder.join(&sha)).unwrap();
        let pages = "{\"page\":1,\"text\":\"35 A fabricated requirement. 36 Next\"}\n";
        fs::write(folder.join(&sha).join("pages.jsonl"), pages).unwrap();
        let manifest = json!({"schema_version":1,"reader":"pypdf","reader_version":"0-test",
            "source_manifest_sha256":file_hash(&root.join("source/manifest.json")).unwrap(),
            "documents":[{"sha256":sha,"page_count":1,"pages_sha256":hash(pages.as_bytes())}],
            "failures":[]});
        fs::write(folder.join("manifest.json"), manifest.to_string()).unwrap();
        let lookup = || paragraph(&root, &connection, &sha, "35", None).unwrap();
        assert_eq!(lookup()["status"], "found");
        let source = root.join("source/sample.pdf");
        fs::write(&source, b"Replaced fabricated bytes").unwrap();
        let replaced = lookup();
        assert_eq!(replaced["status"], "found_unconfirmed");
        assert_eq!(replaced["second_reader"]["status"], "source_changed");
        assert_eq!(
            replaced["second_reader"]["reasons"],
            json!(["Source PDF file sample.pdf no longer matches the document hash."])
        );
        fs::remove_file(&source).unwrap();
        let removed = lookup();
        assert_eq!(removed["status"], "found_unconfirmed");
        assert_eq!(
            removed["second_reader"]["reasons"],
            json!(["Source PDF file sample.pdf is missing or unreadable."])
        );
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
        store_paragraphs(&connection);
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
        store_paragraphs(&connection);
        assert_eq!(
            paragraph(
                Path::new("absent"),
                &connection,
                &"b".repeat(64),
                "35",
                None
            )
            .unwrap()["status"],
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
