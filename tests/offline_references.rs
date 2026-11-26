use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const DOCUMENTS: [(&str, &str, &str, &str); 3] = [
    (
        "old",
        "AASB 16",
        "Fabricated older",
        "26 Fabricated amber lease requirement.\n27 Next fabricated paragraph.",
    ),
    (
        "new",
        "AASB 16",
        "Fabricated newer",
        "26 Fabricated cobalt lease requirement.\n27 Next fabricated paragraph.",
    ),
    (
        "decoy",
        "AASB 1",
        "Fabricated decoy",
        "26 Fabricated decoy lease requirement.\n27 Next fabricated paragraph.",
    ),
];

fn hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn source(name: &str) -> Vec<u8> {
    // These are deliberately invalid PDF bytes, not accounting evidence.
    format!("Fabricated offline source: {name}").into_bytes()
}

fn write_json(path: &Path, value: Value) -> Vec<u8> {
    let bytes = serde_json::to_vec(&value).unwrap();
    fs::write(path, &bytes).unwrap();
    bytes
}

struct Fixture(PathBuf);

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

impl Fixture {
    fn new() -> Self {
        Self::with(&DOCUMENTS)
    }

    /// A one-page PDF per `(name, standard, version label, page text)`.
    fn with(documents: &[(&str, &str, &str, &str)]) -> Self {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let parent = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/test-data");
        fs::create_dir_all(&parent).unwrap();
        let root = parent.join(format!("references-{}-{unique}", std::process::id()));
        fs::create_dir(&root).unwrap();
        let fixture = Self(root);
        fs::create_dir(fixture.0.join("source")).unwrap();
        let mut sources = Vec::new();
        let mut extracted = Vec::new();
        let mut versions = Vec::new();
        for &(name, standard, label, text) in documents {
            let bytes = source(name);
            let sha = hash(&bytes);
            let url = format!("https://example.invalid/{name}.pdf");
            fs::write(fixture.0.join(format!("source/{name}.pdf")), bytes).unwrap();
            sources.push(json!({"filename":format!("{name}.pdf"),"source_url":url,
                "sha256":sha,"page_count":1}));
            versions.push(json!({"standard":standard,"version_label":label,"download_url":url}));
            let folder = fixture.0.join("extracted").join(&sha);
            fs::create_dir_all(&folder).unwrap();
            let pages = format!(
                "{}\n",
                json!({"page":1,"text":text,
                "provenance":"native","needs_review":false})
            );
            fs::write(folder.join("pages.jsonl"), &pages).unwrap();
            extracted.push(json!({"sha256":sha,"page_count":1,
                "pages_sha256":hash(pages.as_bytes())}));
        }
        let manifest = write_json(
            &fixture.0.join("source/manifest.json"),
            json!({"files":sources}),
        );
        write_json(
            &fixture.0.join("extracted/manifest.json"),
            json!({"source_manifest_sha256":hash(&manifest),"documents":extracted,"failures":[]}),
        );
        write_json(
            &fixture.0.join("catalogue.json"),
            json!({"versions":versions,"amendments":[]}),
        );
        fixture
    }

    fn call(&self, command: &str, arguments: &[&str]) -> Value {
        let output = Command::new(env!("CARGO_BIN_EXE_aasb-corpus"))
            .arg(command)
            .arg(&self.0)
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

    fn add_fabricated_reader_metadata(&self) {
        // This exercises the reader-metadata contract; it does not run pypdf.
        let mut documents = Vec::new();
        for (name, _, _, text) in DOCUMENTS {
            let sha = hash(&source(name));
            let folder = self.0.join("second-reader").join(&sha);
            fs::create_dir_all(&folder).unwrap();
            let pages = format!("{}\n", json!({"page":1,"text":text}));
            fs::write(folder.join("pages.jsonl"), &pages).unwrap();
            documents
                .push(json!({"sha256":sha,"page_count":1,"pages_sha256":hash(pages.as_bytes())}));
        }
        write_json(
            &self.0.join("second-reader/manifest.json"),
            json!({
            "schema_version":1,"reader":"pypdf","reader_version":"0-test",
            "source_manifest_sha256":hash(&fs::read(self.0.join("source/manifest.json")).unwrap()),
            "documents":documents,"failures":[]}),
        );
    }
}

fn identities(result: &Value) -> BTreeSet<String> {
    result["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["document_sha256"].as_str().unwrap().to_owned())
        .collect()
}

/// One fabricated document's catalogue row, page and unconfirmed paragraph 26.
fn check_document(fixture: &Fixture, name: &str, standard: &str, label: &str, text: &str) {
    let sha = hash(&source(name));
    let catalogue = fixture.call("catalogue", &["--standard", standard, "--version", &sha]);
    assert_eq!(catalogue["results"].as_array().unwrap().len(), 1);
    assert_eq!(
        catalogue["results"][0]["version_metadata"][0]["version_label"],
        label
    );
    let page = fixture.call("page", &[&sha, "1"]);
    assert_eq!(page["text"], text);
    assert_eq!(page["document_sha256"], sha);
    assert_eq!(page["pdf_page"], 1);
    let paragraph = fixture.call("paragraph", &[&sha, "26"]);
    assert_eq!(paragraph["status"], "found_unconfirmed");
    assert_eq!(paragraph["second_reader"]["status"], "unavailable");
    assert_eq!(paragraph["candidate_count"], 1);
    let candidate = &paragraph["candidates"][0];
    assert_eq!(candidate["next_paragraph"], "27");
    assert_eq!(candidate["fragments"].as_array().unwrap().len(), 1);
    let fragment = &candidate["fragments"][0];
    assert_eq!(fragment["text"], text.lines().next().unwrap());
    assert_eq!(fragment["pdf_page"], 1);
    assert_eq!(fragment["start_line"], 1);
    assert_eq!(fragment["end_line"], 1);
    assert_eq!(fragment["page_text_sha256"], hash(text.as_bytes()));
}

#[test]
fn fabricated_versions_and_near_identifiers_have_exact_references() {
    let fixture = Fixture::new();
    let built = fixture.call("build", &[]);
    assert_eq!(built["documents"], 3);
    assert_eq!(built["pages"], 3);
    let old = hash(&source("old"));
    let new = hash(&source("new"));
    let decoy = hash(&source("decoy"));
    let leases = BTreeSet::from([old.clone(), new.clone()]);
    assert_eq!(
        identities(&fixture.call("catalogue", &["--standard", "AASB 16"])),
        leases
    );
    assert_eq!(
        identities(&fixture.call("catalogue", &["--standard", "AASB 1"])),
        BTreeSet::from([decoy.clone()])
    );
    assert_eq!(
        identities(&fixture.call("search", &["lease", "--standard", "AASB 16"])),
        leases
    );
    assert_eq!(
        identities(&fixture.call("search", &["lease", "--standard", "AASB 1"])),
        BTreeSet::from([decoy])
    );
    for (term, version, expected) in [
        ("amber", old.as_str(), BTreeSet::from([old.clone()])),
        ("cobalt", old.as_str(), BTreeSet::new()),
        ("cobalt", new.as_str(), BTreeSet::from([new.clone()])),
        ("amber", new.as_str(), BTreeSet::new()),
    ] {
        let found = fixture.call(
            "search",
            &[term, "--standard", "AASB 16", "--version", version],
        );
        assert_eq!(identities(&found), expected, "{term} in {version}");
        for row in found["results"].as_array().unwrap() {
            assert_eq!(row["pdf_page"], 1);
        }
    }
    for (name, standard, label, text) in DOCUMENTS {
        check_document(&fixture, name, standard, label, text);
    }
    fixture.add_fabricated_reader_metadata();
    for (name, _, _, _) in DOCUMENTS {
        let paragraph = fixture.call("paragraph", &[&hash(&source(name)), "26"]);
        assert_eq!(paragraph["status"], "found");
        assert_eq!(paragraph["second_reader"]["status"], "confirmed");
    }
}

#[test]
fn stemmed_search_finds_other_word_forms_and_exact_search_does_not() {
    let fixture = Fixture::new();
    fixture.call("build", &[]);
    let exact = fixture.call("search", &["leases"]);
    assert_eq!(exact["results"].as_array().unwrap().len(), 0);
    assert!(exact.get("stemmed").is_none());
    let stemmed = fixture.call("search", &["leases", "--stem"]);
    assert_eq!(stemmed["results"].as_array().unwrap().len(), 3);
    assert_eq!(stemmed["stemmed"], true);
    assert!(
        stemmed["results"][0]["snippet"]
            .as_str()
            .unwrap()
            .contains("[lease]")
    );
    let filtered = fixture.call("search", &["leases", "--standard", "AASB 1", "--stem"]);
    assert_eq!(
        identities(&filtered),
        BTreeSet::from([hash(&source("decoy"))])
    );
    assert_eq!(
        fixture.call("search", &["lease"])["results"],
        fixture.call("search", &["lease", "--stem"])["results"],
    );
}

#[test]
fn an_older_index_keeps_exact_search_and_refuses_stemming() {
    let fixture = Fixture::new();
    fixture.call("build", &[]);
    let index = fixture.0.join("index.sqlite3");
    let connection = rusqlite::Connection::open(&index).unwrap();
    connection
        .execute_batch("DROP TABLE pages_stem; PRAGMA user_version=1;")
        .unwrap();
    drop(connection);
    assert_eq!(
        fixture.call("search", &["lease"])["results"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    let output = Command::new(env!("CARGO_BIN_EXE_aasb-corpus"))
        .args(["search", fixture.0.to_str().unwrap(), "lease", "--stem"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no stem table"));
}

#[test]
fn stem_is_a_search_flag_with_no_value_and_only_one_use() {
    let fixture = Fixture::new();
    fixture.call("build", &[]);
    let root = fixture.0.to_str().unwrap();
    for arguments in [
        vec!["search", root, "lease", "--stem", "--stem"],
        vec!["search", root, "lease", "--stem", "yes"],
        vec!["catalogue", root, "--stem"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_aasb-corpus"))
            .args(&arguments)
            .output()
            .unwrap();
        assert!(!output.status.success(), "{arguments:?}");
    }
}

/// README: 'No command uploads documents, calls a model provider or downloads
/// content.' `strace -f` sees any network syscall, from any library or child
/// process, on one run of every command. That proves these runs, not unexercised
/// paths. Without strace the test skips with a message.
#[cfg(target_os = "linux")]
#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one linear strace run over every command; splitting it hides the sequence"
)]
fn every_command_makes_no_network_syscall() {
    if Command::new("strace").arg("-V").output().is_err() {
        eprintln!("skipped: strace is not installed");
        return;
    }
    let fixture = Fixture::new();
    let root = fixture.0.to_str().unwrap();
    let sha = hash(&source("new"));
    let log = fixture.0.join("no_network.strace");
    let log = log.to_str().unwrap();
    let session = mcp_lines(&tool_calls(&sha));
    for (command, input) in [
        (vec!["build", root], String::new()),
        (vec!["stats", root], String::new()),
        (vec!["catalogue", root], String::new()),
        (vec!["search", root, "lease"], String::new()),
        (vec!["paragraph", root, &sha, "26"], String::new()),
        (vec!["page", root, &sha, "1"], String::new()),
        (vec!["verify", root], String::new()),
        (
            vec!["verify-quote", root, &sha, "1", "Fabricated cobalt"],
            String::new(),
        ),
        (vec!["mcp", root], session),
    ] {
        let mut child = Command::new("strace")
            .args(["-f", "-qq", "-e", "trace=%network,execve", "-o", log])
            .arg(env!("CARGO_BIN_EXE_aasb-corpus"))
            .args(&command)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        if command[0] == "mcp" {
            // Every tool answered, so the trace covers each one.
            let replies: Vec<Value> = String::from_utf8(output.stdout)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(replies.len(), 6);
            assert!(
                replies
                    .iter()
                    .all(|r| r["result"]["isError"] == json!(false)),
                "{replies:?}"
            );
        }
        // The binary's own execve shows the tracer was attached; anything more is
        // a network syscall or a child process.
        let trace = fs::read_to_string(log).unwrap();
        let calls: Vec<&str> = trace.lines().collect();
        assert_eq!(calls.len(), 1, "{}: {trace}", command[0]);
        assert!(
            calls[0].contains("execve(") && calls[0].ends_with("= 0"),
            "{trace}"
        );
    }
}

/// One `tools/call` request per tool, ids 3 to 6, over the fabricated "new" PDF.
fn tool_calls(sha: &str) -> Vec<Value> {
    [
        (
            "search_aasb_pages",
            json!({"query":"cobalt","standard":"AASB 16","limit":5}),
        ),
        ("list_aasb_documents", json!({"standard":"AASB 16"})),
        (
            "read_aasb_paragraph",
            json!({"document_sha256":sha,"label":"26"}),
        ),
        (
            "verify_aasb_quote",
            json!({"document_sha256":sha,"page":1,"quote":"Fabricated cobalt"}),
        ),
        ("read_aasb_page", json!({"document_sha256":sha,"page":1})),
        ("get_aasb_stats", json!({})),
    ]
    .into_iter()
    .zip(3..)
    .map(|((name, arguments), id)| {
        json!({"jsonrpc":"2.0","id":id,"method":"tools/call",
            "params":{"name":name,"arguments":arguments}})
    })
    .collect()
}

fn mcp_lines(requests: &[Value]) -> String {
    requests.iter().map(|r| format!("{r}\n")).collect()
}

/// Run `mcp ROOT` on `requests` until stdin closes; return its replies and stderr.
fn mcp(root: &Path, requests: &[Value]) -> (Vec<Value>, String) {
    mcp_input(root, &mcp_lines(requests))
}

/// Run `mcp ROOT` on raw `input` lines until stdin closes; return its replies and stderr.
fn mcp_input(root: &Path, input: &str) -> (Vec<Value>, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_aasb-corpus"))
        .arg("mcp")
        .arg(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    let replies = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    (replies, String::from_utf8(output.stderr).unwrap())
}

fn tool_names(reply: &Value) -> Vec<&str> {
    reply["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect()
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "each tool's reply is compared with its command's output in one table"
)]
fn mcp_tools_answer_as_the_commands_do() {
    let fixture = Fixture::new();
    fixture.add_fabricated_reader_metadata();
    fixture.call("build", &[]);
    let sha = hash(&source("new"));
    let mut requests = vec![
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18",
            "capabilities":{},"clientInfo":{"name":"test","version":"0"}}}),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
    ];
    requests.extend(tool_calls(&sha));
    let (replies, stderr) = mcp(&fixture.0, &requests);
    assert_eq!(stderr, "");
    let ids: Vec<Value> = replies.iter().map(|r| r["id"].clone()).collect();
    assert_eq!(ids, (1..=8).map(|n| json!(n)).collect::<Vec<_>>());
    assert_eq!(replies[0]["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(replies[0]["result"]["capabilities"], json!({"tools":{}}));
    assert_eq!(
        tool_names(&replies[1]),
        [
            "search_aasb_pages",
            "list_aasb_documents",
            "read_aasb_paragraph",
            "verify_aasb_quote",
            "read_aasb_page",
            "get_aasb_stats",
            "list_aasb_s2_requirements",
            "check_aasb_gap_analysis"
        ]
    );
    let commands = [
        fixture.call(
            "search",
            &["cobalt", "--standard", "AASB 16", "--limit", "5"],
        ),
        fixture.call("catalogue", &["--standard", "AASB 16"]),
        fixture.call("paragraph", &[&sha, "26"]),
        fixture.call("verify-quote", &[&sha, "1", "Fabricated cobalt"]),
        fixture.call("page", &[&sha, "1"]),
        fixture.call("stats", &[]),
    ];
    for (reply, command) in replies[2..].iter().zip(commands) {
        let result = &reply["result"];
        assert_eq!(result["isError"], false, "{result}");
        assert_eq!(result["structuredContent"], command);
        let text: Value =
            serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(text, command);
    }
    assert_eq!(replies[4]["result"]["structuredContent"]["status"], "found");
    assert_eq!(replies[5]["result"]["structuredContent"]["verified"], true);
}

#[test]
fn mcp_s2_tools_refuse_another_standard_and_a_result_that_is_not_an_object() {
    let fixture = Fixture::new();
    fixture.call("build", &[]);
    let sha = hash(&source("new"));
    let requests: Vec<Value> = [
        ("list_aasb_s2_requirements", json!({"document_sha256":sha})),
        (
            "check_aasb_gap_analysis",
            json!({"document_sha256":sha,"result":{"results":[]}}),
        ),
        (
            "check_aasb_gap_analysis",
            json!({"document_sha256":sha,"result":"/etc/passwd"}),
        ),
        ("check_aasb_gap_analysis", json!({"document_sha256":sha})),
        ("get_aasb_stats", json!({"path":"/etc"})),
        ("read_aasb_page", json!({"document_sha256":sha})),
    ]
    .into_iter()
    .zip(1..)
    .map(|((name, arguments), id)| {
        json!({"jsonrpc":"2.0","id":id,"method":"tools/call",
            "params":{"name":name,"arguments":arguments}})
    })
    .collect();
    let (replies, stderr) = mcp(&fixture.0, &requests);
    assert_eq!(stderr, "");
    let errors: Vec<_> = replies
        .iter()
        .map(|reply| {
            assert_eq!(reply["result"]["isError"], true, "{reply}");
            reply["result"]["content"][0]["text"].as_str().unwrap()
        })
        .collect();
    assert_eq!(
        errors,
        [
            "the document is not AASB S2",
            "the document is not AASB S2",
            "argument result has the wrong type",
            "missing argument: result",
            "unknown argument: path",
            "missing argument: page"
        ]
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "each request is checked by its reply's index; splitting the table from its checks hides the pairing"
)]
fn mcp_refuses_paths_unknown_tools_and_malformed_requests() {
    let fixture = Fixture::new();
    fixture.call("build", &[]);
    let sha = hash(&source("new"));
    let call = |id: u32, name: &str, arguments: Value| {
        json!({"jsonrpc":"2.0","id":id,"method":"tools/call",
            "params":{"name":name,"arguments":arguments}})
    };
    let requests = [
        call(
            1,
            "verify_aasb_quote",
            json!({"document_sha256":sha,"quote":"Fabricated cobalt","snapshot":"/etc"}),
        ),
        call(
            2,
            "search_aasb_pages",
            json!({"query":"cobalt","limit":"5"}),
        ),
        call(
            3,
            "search_aasb_pages",
            json!({"query":"cobalt","limit":101}),
        ),
        call(4, "read_aasb_paragraph", json!({"label":"26"})),
        call(5, "build", json!({})),
        json!({"jsonrpc":"2.0","id":6,"method":"server/discover"}),
        json!({"id":7,"method":"ping"}),
        json!({"jsonrpc":"2.0","id":8,"method":"initialize","params":{"protocolVersion":"1999-01-01"}}),
        // 2025-03-26 requires accepting batches, which this server refuses.
        json!({"jsonrpc":"2.0","id":9,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}),
        json!([{"jsonrpc":"2.0","id":10,"method":"ping"}]),
        json!({"jsonrpc":"2.0","id":null,"method":"ping"}),
        json!({"jsonrpc":"2.0","id":{"n":1},"method":"ping"}),
        json!({"jsonrpc":"2.0","id":1.5,"method":"ping"}),
        json!({"jsonrpc":"2.0","id":"eleven","method":"ping"}),
    ];
    let mut input = mcp_lines(&requests);
    input.push_str("not json\n");
    let (replies, stderr) = mcp_input(&fixture.0, &input);
    assert_eq!(stderr, "");
    let tool_error = |n: usize| {
        assert_eq!(replies[n]["result"]["isError"], true, "{}", replies[n]);
        replies[n]["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert_eq!(tool_error(0), "unknown argument: snapshot");
    assert_eq!(tool_error(1), "argument limit has the wrong type");
    assert!(tool_error(2).starts_with("limit must be 1..100"));
    assert_eq!(tool_error(3), "missing argument: document_sha256");
    assert_eq!(replies[4]["error"]["code"], -32602);
    assert_eq!(replies[5]["error"]["code"], -32601);
    assert_eq!(replies[6]["error"]["code"], -32600);
    assert_eq!(replies[7]["result"]["protocolVersion"], "2025-11-25");
    assert_eq!(replies[8]["result"]["protocolVersion"], "2025-11-25");
    // A batch, and a null, object or fractional id, are invalid requests with no id.
    for reply in &replies[9..13] {
        assert_eq!(
            *reply,
            json!({"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"invalid request"}})
        );
    }
    assert_eq!(
        replies[13],
        json!({"jsonrpc":"2.0","id":"eleven","result":{}})
    );
    assert_eq!(
        replies[14],
        json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"parse error"}})
    );
    assert_eq!(replies.len(), 15);
}

#[test]
fn mcp_refuses_an_over_long_line_and_serves_the_next() {
    let fixture = Fixture::new();
    fixture.call("build", &[]);
    let long = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\",\"pad\":\"{}\"}}\n",
        "x".repeat(3 << 20)
    );
    let input = long + &mcp_lines(&[json!({"jsonrpc":"2.0","id":2,"method":"ping"})]);
    let (replies, stderr) = mcp_input(&fixture.0, &input);
    assert_eq!(stderr, "");
    assert_eq!(replies[0]["id"], Value::Null);
    assert_eq!(replies[0]["error"]["code"], -32600);
    assert_eq!(
        replies[0]["error"]["message"],
        "invalid request: a message line over 1 MiB is not read"
    );
    assert_eq!(replies[1], json!({"jsonrpc":"2.0","id":2,"result":{}}));
    assert_eq!(replies.len(), 2);
}

#[test]
fn mcp_needs_a_built_snapshot_before_serving() {
    let fixture = Fixture::new();
    let output = Command::new(env!("CARGO_BIN_EXE_aasb-corpus"))
        .arg("mcp")
        .arg(&fixture.0)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
}

const CONTEXT_DOCUMENTS: [(&str, &str, &str, &str); 2] = [
    (
        "sequence",
        "AASB 16",
        "Fabricated sequence",
        "24 Fabricated opening paragraph.\n25 Fabricated second requirement.\n26 Fabricated amber lease requirement.\n27 Fabricated fourth requirement.\n28 Fabricated closing paragraph.",
    ),
    (
        "repeat",
        "AASB 1",
        "Fabricated repeat",
        "5 Fabricated opening note.\n6 Fabricated violet note.\n5 Fabricated repeated note.\n6 Fabricated violet guidance.",
    ),
];

/// The stderr of a command that must fail.
fn failure(arguments: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_aasb-corpus"))
        .args(arguments)
        .output()
        .unwrap();
    assert!(!output.status.success(), "{arguments:?}");
    String::from_utf8(output.stderr).unwrap()
}

fn context_labels(candidate: &Value, side: &str) -> Vec<String> {
    candidate["context"][side]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["paragraph"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn paragraph_context_returns_neighbouring_paragraphs_from_the_build_time_table() {
    let fixture = Fixture::with(&CONTEXT_DOCUMENTS);
    fixture.call("build", &[]);
    assert_eq!(fixture.call("stats", &[])["paragraphs"], 9);
    let sha = hash(&source("sequence"));
    let plain = fixture.call("paragraph", &[&sha, "26"]);
    assert!(plain["candidates"][0].get("context").is_none());
    let one = fixture.call("paragraph", &[&sha, "26", "--context", "1"]);
    assert_eq!(one["status"], "found_unconfirmed");
    let candidate = &one["candidates"][0];
    assert_eq!(candidate["context"]["ordinal"], 2);
    assert_eq!(context_labels(candidate, "before"), ["25"]);
    assert_eq!(context_labels(candidate, "after"), ["27"]);
    let before = &candidate["context"]["before"][0];
    assert_eq!(
        before["fragments"][0]["text"],
        "25 Fabricated second requirement."
    );
    assert_eq!(before["fragments"][0]["start_line"], 2);
    assert_eq!(before["next_paragraph"], "26");
    assert!(
        one["context_boundary"]
            .as_str()
            .unwrap()
            .contains("second reader")
    );
    // The paragraph itself is unchanged by the context option.
    let mut without = one.clone();
    without["candidates"][0]
        .as_object_mut()
        .unwrap()
        .remove("context");
    without.as_object_mut().unwrap().remove("context_boundary");
    assert_eq!(without, plain);
    // Context stops at the ends of the document.
    let end = fixture.call("paragraph", &[&sha, "27", "--context", "2"]);
    assert_eq!(
        context_labels(&end["candidates"][0], "before"),
        ["25", "26"]
    );
    assert_eq!(context_labels(&end["candidates"][0], "after"), ["28"]);
    let start = fixture.call("paragraph", &[&sha, "24", "--context", "1"]);
    assert!(context_labels(&start["candidates"][0], "before").is_empty());
}

#[test]
fn a_repeated_label_keeps_one_context_per_candidate_and_counts_are_bounded() {
    let fixture = Fixture::with(&CONTEXT_DOCUMENTS);
    fixture.call("build", &[]);
    let sha = hash(&source("sequence"));
    let repeat = hash(&source("repeat"));
    let ambiguous = fixture.call("paragraph", &[&repeat, "5", "--context", "1"]);
    assert_eq!(ambiguous["status"], "ambiguous");
    let candidates = ambiguous["candidates"].as_array().unwrap();
    assert!(context_labels(&candidates[0], "before").is_empty());
    assert_eq!(context_labels(&candidates[0], "after"), ["6"]);
    // The second 5 restarts the series, so the first 6 is not its context.
    assert!(context_labels(&candidates[1], "before").is_empty());
    assert_eq!(context_labels(&candidates[1], "after"), ["6"]);
    let root = fixture.0.to_str().unwrap();
    for bad in ["0", "6", "x"] {
        failure(&["paragraph", root, &sha, "26", "--context", bad]);
    }
    failure(&["paragraph", root, &sha, "26", "--stem", "1"]);
}

#[test]
fn paragraph_search_returns_labelled_paragraphs() {
    let fixture = Fixture::with(&CONTEXT_DOCUMENTS);
    fixture.call("build", &[]);
    let sha = hash(&source("sequence"));
    let found = fixture.call("search", &["amber", "--paragraphs"]);
    let results = found["results"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["document_sha256"], sha);
    assert_eq!(results[0]["paragraph"], "26");
    assert_eq!(results[0]["ordinal"], 2);
    assert_eq!(results[0]["pdf_page"], 1);
    assert!(results[0]["snippet"].as_str().unwrap().contains("[amber]"));
    assert_eq!(found["paragraphs"], true);
    let filtered = fixture.call(
        "search",
        &["violet", "--paragraphs", "--standard", "AASB 16"],
    );
    assert!(filtered["results"].as_array().unwrap().is_empty());
    let stemmed = fixture.call("search", &["requirements", "--paragraphs", "--stem"]);
    let labels: BTreeSet<_> = stemmed["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["paragraph"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        labels,
        BTreeSet::from(["25".into(), "26".into(), "27".into()])
    );
    let root = fixture.0.to_str().unwrap();
    failure(&["search", root, "amber", "--paragraphs", "--paragraphs"]);
    failure(&["search", root, "amber", "--paragraphs", "yes"]);
}

#[test]
fn an_older_index_answers_paragraphs_from_pages_and_refuses_the_table_options() {
    let fixture = Fixture::with(&CONTEXT_DOCUMENTS);
    fixture.call("build", &[]);
    let sha = hash(&source("sequence"));
    let repeat = hash(&source("repeat"));
    // Schema 2 has no derived facts, so compare the lookup without them.
    let current = [(&sha, "26"), (&repeat, "6")].map(|(document, label)| {
        let mut result = fixture.call("paragraph", &[document, label]);
        for key in ["cited_by", "validity", "reporting_tier", "derived_boundary"] {
            result.as_object_mut().unwrap().remove(key).unwrap();
        }
        result
    });
    let connection = rusqlite::Connection::open(fixture.0.join("index.sqlite3")).unwrap();
    connection
        .execute_batch(
            "DROP TABLE paragraphs_fts; DROP TABLE paragraphs_stem; DROP TABLE paragraphs;
            DROP TABLE citations; DROP TABLE document_facts; PRAGMA user_version=2;",
        )
        .unwrap();
    drop(connection);
    assert_eq!(fixture.call("paragraph", &[&sha, "26"]), current[0]);
    assert_eq!(fixture.call("paragraph", &[&repeat, "6"]), current[1]);
    assert!(fixture.call("stats", &[]).get("paragraphs").is_none());
    let root = fixture.0.to_str().unwrap();
    for arguments in [
        vec!["paragraph", root, &sha, "26", "--context", "1"],
        vec!["search", root, "amber", "--paragraphs"],
    ] {
        assert!(
            failure(&arguments).contains("no paragraph table"),
            "{arguments:?}"
        );
    }
}

#[test]
fn mcp_paragraph_context_and_search_match_the_commands() {
    let fixture = Fixture::with(&CONTEXT_DOCUMENTS);
    fixture.call("build", &[]);
    let sha = hash(&source("sequence"));
    let requests = [
        json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"read_aasb_paragraph",
            "arguments":{"document_sha256":sha,"label":"26","context":1}}}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"search_aasb_pages",
            "arguments":{"query":"amber","paragraphs":true}}}),
    ];
    let (replies, stderr) = mcp(&fixture.0, &requests);
    assert_eq!(stderr, "");
    let commands = [
        fixture.call("paragraph", &[&sha, "26", "--context", "1"]),
        fixture.call("search", &["amber", "--paragraphs"]),
    ];
    for (reply, command) in replies.iter().zip(commands) {
        assert_eq!(reply["result"]["isError"], false, "{reply}");
        assert_eq!(reply["result"]["structuredContent"], command);
    }
}

const CITING_DOCUMENTS: [(&str, &str, &str, &str); 4] = [
    (
        "leases-old",
        "AASB 16",
        "Fabricated older",
        "26 Fabricated amber lease requirement.\n27 Next fabricated paragraph.",
    ),
    (
        "leases-new",
        "AASB 16",
        "Fabricated newer",
        "26 Fabricated amber lease requirement.\n27 Next fabricated paragraph.",
    ),
    (
        "tier",
        "AASB 1060",
        "Fabricated tier",
        "1 Fabricated note; see paragraph 26 of AASB 16.\n2 Next fabricated note.",
    ),
    (
        "amending",
        "",
        "",
        "5 Paragraphs 26 and 27 of AASB 16 are fabricated.\n6 Next fabricated amendment.",
    ),
];

/// A catalogue that states periods, a tier title and an amending standard, as the
/// captured catalogue does.
fn citing_fixture() -> Fixture {
    let fixture = Fixture::with(&CITING_DOCUMENTS);
    let url = |name: &str| format!("https://example.invalid/{name}.pdf");
    write_json(
        &fixture.0.join("catalogue.json"),
        json!({"retrieved_date":"2026-10-15",
        "listings":[{"standard":"AASB 1060","title":"Fabricated Simplified Disclosures for Tier 2 Entities"}],
        "versions":[
            {"standard":"AASB 16","version_label":"Fabricated older","download_url":url("leases-old"),
             "applies_to_periods_text":"Applies to periods beginning on or after 1 January 2019 but before 1 January 2027"},
            {"standard":"AASB 16","version_label":"Fabricated newer","download_url":url("leases-new"),
             "applies_to_periods_text":"Applies to periods beginning on or after 1 January 2027"},
            {"standard":"AASB 1060","version_label":"Fabricated tier","download_url":url("tier")}],
        "amendments":[{"title":"AASB 2099-1 Amendments to Australian Accounting Standards – Fabricated",
            "download_url":url("amending"),"parent_standards":["AASB 16"]}]}),
    );
    fixture.call("build", &[]);
    fixture
}

fn citing_titles(cited_by: &Value, kind: &str) -> Vec<(String, u64)> {
    cited_by[kind]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            (
                row["title"].as_str().unwrap().to_owned(),
                row["line"].as_u64().unwrap(),
            )
        })
        .collect()
}

#[test]
fn paragraphs_carry_their_citations_validity_and_stated_tier() {
    let fixture = citing_fixture();
    let old = hash(&source("leases-old"));
    let new = hash(&source("leases-new"));
    let found = fixture.call("paragraph", &[&old, "26"]);
    let cited_by = &found["cited_by"];
    assert_eq!(
        citing_titles(cited_by, "standards"),
        [("Fabricated tier".into(), 1)]
    );
    assert_eq!(
        citing_titles(cited_by, "amending_standards"),
        [(
            "AASB 2099-1 Amendments to Australian Accounting Standards – Fabricated".into(),
            1
        )]
    );
    // The snapshot holds no Interpretation or agenda decision, so none is linked.
    assert_eq!(cited_by["interpretations"], json!([]));
    assert_eq!(cited_by["agenda_decisions"], json!([]));
    assert_eq!(cited_by["total"], 2);
    assert_eq!(found["validity"]["status"], "current");
    assert_eq!(found["validity"]["superseded_by"], json!([new]));
    assert_eq!(found["reporting_tier"]["tier"], Value::Null);
    assert!(
        found["derived_boundary"]
            .as_str()
            .unwrap()
            .contains("does not decide")
    );
    let later = fixture.call("paragraph", &[&new, "27"]);
    assert_eq!(later["validity"]["status"], "not_yet_effective");
    assert_eq!(later["validity"]["superseded_by"], json!([]));
    assert_eq!(later["cited_by"]["total"], 1);
    let tier = fixture.call("paragraph", &[&hash(&source("tier")), "1"]);
    assert_eq!(tier["reporting_tier"]["tier"], 2);
    assert_eq!(tier["validity"]["status"], Value::Null);
    let catalogue = fixture.call("catalogue", &["--version", &old]);
    assert_eq!(catalogue["results"][0]["validity"], found["validity"]);
    assert_eq!(
        catalogue["results"][0]["reporting_tier"],
        found["reporting_tier"]
    );
}

/// The live reference test reads this file; check its shape offline.
#[test]
fn reference_questions_are_well_formed_and_cover_aasb_s2() {
    let questions = include_str!("fixtures/reference-questions.tsv");
    let rows: Vec<Vec<&str>> = questions
        .lines()
        .filter(|line| !line.starts_with('#'))
        .map(|line| line.split('\t').collect())
        .collect();
    for row in &rows {
        assert_eq!(row.len(), 3, "{row:?}");
        assert!(row.iter().all(|field| !field.trim().is_empty()), "{row:?}");
        assert!(row[1].starts_with("AASB "), "{row:?}");
    }
    let s2: BTreeSet<&str> = rows
        .iter()
        .filter(|row| row[1] == "AASB S2")
        .map(|row| row[2])
        .collect();
    assert_eq!(s2, BTreeSet::from(["3", "AusC1.1", "BC88"]));
}

const SERIES_DOCUMENTS: [(&str, &str, &str, &str); 1] = [(
    "series",
    "AASB S2",
    "Fabricated series",
    "104 Fabricated main paragraph.\n105 Fabricated last main paragraph.\nB1 Fabricated appendix paragraph.\nB2 Fabricated second appendix paragraph.\nC5 Fabricated transition paragraph.\nC6 Fabricated last transition paragraph.\nC1 Fabricated reproduced paragraph.\nC2 Fabricated reproduced second paragraph.",
)];

#[test]
fn context_stops_where_the_label_series_changes_or_restarts() {
    let fixture = Fixture::with(&SERIES_DOCUMENTS);
    fixture.call("build", &[]);
    let sha = hash(&source("series"));
    let appendix = fixture.call("paragraph", &[&sha, "B1", "--context", "3"]);
    let candidate = &appendix["candidates"][0];
    assert!(context_labels(candidate, "before").is_empty());
    assert_eq!(context_labels(candidate, "after"), ["B2"]);
    let last = fixture.call("paragraph", &[&sha, "C6", "--context", "3"]);
    assert_eq!(context_labels(&last["candidates"][0], "before"), ["C5"]);
    assert!(context_labels(&last["candidates"][0], "after").is_empty());
    let main = fixture.call("paragraph", &[&sha, "105", "--context", "2"]);
    assert_eq!(context_labels(&main["candidates"][0], "before"), ["104"]);
    assert!(context_labels(&main["candidates"][0], "after").is_empty());
}

/// Each stored row `verify` must re-derive: a change to any of them fails verification.
#[test]
fn verify_rederives_paragraph_columns_citations_and_document_facts() {
    let fixture = citing_fixture();
    let root = fixture.0.to_str().unwrap();
    assert_eq!(fixture.call("verify", &[])["verified"], true);
    for change in [
        "UPDATE paragraphs SET text='altered' WHERE id=(SELECT min(id) FROM paragraphs)",
        "UPDATE paragraphs SET needs_review=1-needs_review WHERE id=(SELECT min(id) FROM paragraphs)",
        "UPDATE paragraphs SET ordinal=ordinal+100 WHERE id=(SELECT max(id) FROM paragraphs)",
        "UPDATE citations SET cited_label='99' WHERE id=(SELECT min(id) FROM citations)",
        "DELETE FROM citations WHERE id=(SELECT min(id) FROM citations)",
        "UPDATE document_facts SET validity=replace(validity,'current','superseded')",
        "UPDATE document_facts SET reporting_tier='{\"tier\":3,\"basis\":null}'",
    ] {
        let path = fixture.0.join("index.sqlite3");
        let original = fs::read(&path).unwrap();
        let connection = rusqlite::Connection::open(&path).unwrap();
        assert!(connection.execute(change, []).unwrap() > 0, "{change}");
        drop(connection);
        let verified = Command::new(env!("CARGO_BIN_EXE_aasb-corpus"))
            .args(["verify", root])
            .output()
            .unwrap();
        assert!(!verified.status.success(), "{change}");
        fs::write(&path, original).unwrap();
    }
    assert_eq!(fixture.call("verify", &[])["verified"], true);
}
