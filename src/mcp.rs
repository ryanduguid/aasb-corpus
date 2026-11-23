//! `aasb-corpus mcp SNAPSHOT`: the read-only commands as MCP tools over stdio.
//!
//! One JSON-RPC message per line on stdin and stdout, as the MCP stdio transport
//! sets out, using the `initialize` handshake of revisions 2025-11-25,
//! 2025-06-18 and 2024-11-05. The snapshot is fixed at start-up and no tool takes a path, so a
//! call can read only that snapshot; an argument a tool does not declare is
//! refused. Nothing is written to stderr whilst serving, so no page text reaches
//! a log.
use super::{
    QueryOptions, Result, catalogue, context_count, open, paragraph, search, verify_quote,
};
use serde_json::{Map, Value, json};
use std::io::{BufRead, Write};
use std::path::Path;

/// The handshake revisions served, newest first.
/// 2025-03-26 is left out: it requires accepting JSON-RPC batches, which this
/// server refuses.
const VERSIONS: [&str; 3] = ["2025-11-25", "2025-06-18", "2024-11-05"];

fn tools() -> Value {
    let filters = json!({
        "standard": {"type": "string", "description": "Complete principal-standard identifier, such as AASB 16"},
        "version": {"type": "string", "description": "A document_sha256 from list_aasb_documents"},
        "limit": {"type": "integer", "minimum": 1, "maximum": 100, "default": 20},
        "offset": {"type": "integer", "minimum": 0, "default": 0}
    });
    let mut search_properties = filters.as_object().cloned().unwrap_or_default();
    search_properties.insert(
        "query".into(),
        json!({"type": "string", "description": "SQLite FTS5 query; words match as written"}),
    );
    search_properties.insert(
        "stem".into(),
        json!({"type": "boolean", "default": false, "description": "Also match other forms of each word"}),
    );
    search_properties.insert(
        "paragraphs".into(),
        json!({"type": "boolean", "default": false, "description": "Rank stored paragraphs, with their labels, instead of pages"}),
    );
    let document =
        json!({"type": "string", "description": "The document_sha256 of one captured PDF"});
    json!([
        {"name": "search_aasb_pages",
         "description": "Rank pages of the captured AASB PDFs for a query, with physical page numbers and version metadata.",
         "inputSchema": {"type": "object", "properties": search_properties, "required": ["query"], "additionalProperties": false},
         "annotations": {"readOnlyHint": true, "openWorldHint": false}},
        {"name": "list_aasb_documents",
         "description": "List captured documents with their hashes, aliases and version labels. Catalogue dates do not determine applicability.",
         "inputSchema": {"type": "object", "properties": filters, "additionalProperties": false},
         "annotations": {"readOnlyHint": true, "openWorldHint": false}},
        {"name": "read_aasb_paragraph",
         "description": "Look up a paragraph label in one captured PDF; only status found is a located source span.",
         "inputSchema": {"type": "object", "properties": {"document_sha256": document, "label": {"type": "string", "description": "Such as 26, B19, Aus25.1 or BC1"},
                         "context": {"type": "integer", "minimum": 1, "maximum": 5, "description": "Neighbouring paragraphs to return on each side"}},
                         "required": ["document_sha256", "label"], "additionalProperties": false},
         "annotations": {"readOnlyHint": true, "openWorldHint": false}},
        {"name": "verify_aasb_quote",
         "description": "Grade a case-sensitive quotation against a captured PDF; verified is true only for an exact match on the named page.",
         "inputSchema": {"type": "object", "properties": {"document_sha256": document, "page": {"type": "integer", "minimum": 1}, "quote": {"type": "string"}},
                         "required": ["document_sha256", "quote"], "additionalProperties": false},
         "annotations": {"readOnlyHint": true, "openWorldHint": false}}
    ])
}

/// The tool's arguments, refusing any it does not declare and any of the wrong type.
fn arguments(
    call: &Value,
    strings: &[&str],
    integers: &[&str],
    flags: &[&str],
) -> Result<Map<String, Value>> {
    let arguments = match call.get("arguments") {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(arguments)) => arguments.clone(),
        Some(_) => return Err("arguments must be an object".into()),
    };
    for (key, value) in &arguments {
        let fits = if strings.contains(&key.as_str()) {
            value.is_string()
        } else if integers.contains(&key.as_str()) {
            value.is_u64()
        } else if flags.contains(&key.as_str()) {
            value.is_boolean()
        } else {
            return Err(format!("unknown argument: {key}").into());
        };
        if !fits {
            return Err(format!("argument {key} has the wrong type").into());
        }
    }
    Ok(arguments)
}

fn required<'a>(arguments: &'a Map<String, Value>, key: &str) -> Result<&'a str> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing argument: {key}").into())
}

/// The filters as the command line spells them, so both share one validation.
fn options(arguments: &Map<String, Value>) -> Result<QueryOptions> {
    let mut words = Vec::new();
    for key in ["standard", "version", "limit", "offset"] {
        match arguments.get(key) {
            Some(Value::String(text)) => words.extend([format!("--{key}"), text.clone()]),
            Some(number @ Value::Number(_)) => {
                words.extend([format!("--{key}"), number.to_string()]);
            }
            _ => {}
        }
    }
    QueryOptions::parse(&words)
}

fn read_paragraph(root: &Path, connection: &rusqlite::Connection, call: &Value) -> Result<Value> {
    let a = arguments(call, &["document_sha256", "label"], &["context"], &[])?;
    let context = match a.get("context") {
        Some(count) => context_count(&["--context".into(), count.to_string()])?,
        None => None,
    };
    paragraph(
        root,
        connection,
        required(&a, "document_sha256")?,
        required(&a, "label")?,
        context,
    )
}

fn call_tool(
    root: &Path,
    connection: &rusqlite::Connection,
    name: &str,
    call: &Value,
) -> Result<Value> {
    const PAGING: [&str; 2] = ["limit", "offset"];
    match name {
        "search_aasb_pages" => {
            let a = arguments(
                call,
                &["query", "standard", "version"],
                &PAGING,
                &["stem", "paragraphs"],
            )?;
            let mut options = options(&a)?;
            options.stem = a.get("stem").and_then(Value::as_bool).unwrap_or(false);
            options.paragraphs = a
                .get("paragraphs")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            search(connection, required(&a, "query")?, &options)
        }
        "list_aasb_documents" => catalogue(
            connection,
            &options(&arguments(call, &["standard", "version"], &PAGING, &[])?)?,
        ),
        "read_aasb_paragraph" => read_paragraph(root, connection, call),
        "verify_aasb_quote" => {
            let a = arguments(call, &["document_sha256", "quote"], &["page"], &[])?;
            let page = match a.get("page").and_then(Value::as_u64) {
                Some(page) => Some(u32::try_from(page).map_err(|_| "page is too large")?),
                None => None,
            };
            verify_quote(
                root,
                connection,
                required(&a, "document_sha256")?,
                page,
                required(&a, "quote")?,
            )
        }
        _ => Err(format!("unknown tool: {name}").into()),
    }
}

/// The reply to one request: `Ok(result)` or `Err((code, message))`.
fn reply(
    root: &Path,
    connection: &rusqlite::Connection,
    method: &str,
    params: &Value,
) -> std::result::Result<Value, (i64, String)> {
    match method {
        "initialize" => {
            let asked = params.get("protocolVersion").and_then(Value::as_str);
            let version = VERSIONS
                .into_iter()
                .find(|v| Some(*v) == asked)
                .unwrap_or(VERSIONS[0]);
            Ok(
                json!({"protocolVersion": version, "capabilities": {"tools": {}},
                "serverInfo": {"name": "aasb-corpus", "version": env!("CARGO_PKG_VERSION")},
                "instructions": "Read-only search of one local AASB snapshot. Results cite exact PDF hashes and physical pages; check any quotation with verify_aasb_quote. Nothing here determines accounting applicability."}),
            )
        }
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tools()})),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            if !tools()
                .as_array()
                .is_some_and(|t| t.iter().any(|tool| tool["name"] == name))
            {
                return Err((-32602, format!("unknown tool: {name}")));
            }
            // A refused or failed call is a tool result, so the model can correct it.
            Ok(match call_tool(root, connection, name, params) {
                Ok(value) => json!({"content": [{"type": "text", "text": value.to_string()}],
                    "structuredContent": value, "isError": false}),
                Err(error) => {
                    json!({"content": [{"type": "text", "text": error.to_string()}], "isError": true})
                }
            })
        }
        _ => Err((-32601, format!("method not found: {method}"))),
    }
}

/// Serve requests from stdin until it closes.
pub fn serve(root: &Path) -> Result<()> {
    let connection = open(root)?;
    let stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.split(b'\n') {
        let line = line?;
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let response = match serde_json::from_slice::<Value>(&line) {
            Err(_) => {
                json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": "parse error"}})
            }
            Ok(message) => {
                let method = message.get("method").and_then(Value::as_str);
                // A request id is a string or an integer, never null.
                let id = message.get("id");
                let valid_id = id.filter(|id| id.is_string() || id.is_i64() || id.is_u64());
                match (id, method) {
                    // A notification, initialized included, needs no reply.
                    (None, Some(_)) => continue,
                    (Some(id), Some(method))
                        if valid_id.is_some() && message.get("jsonrpc") == Some(&json!("2.0")) =>
                    {
                        let params = message.get("params").cloned().unwrap_or(Value::Null);
                        match reply(root, &connection, method, &params) {
                            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
                            Err((code, text)) => {
                                json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": text}})
                            }
                        }
                    }
                    _ => json!({"jsonrpc": "2.0", "id": valid_id.cloned().unwrap_or(Value::Null),
                        "error": {"code": -32600, "message": "invalid request"}}),
                }
            }
        };
        writeln!(stdout, "{response}")?;
        stdout.flush()?;
    }
    Ok(())
}
