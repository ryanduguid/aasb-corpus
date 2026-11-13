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
        for (name, standard, label, text) in DOCUMENTS {
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
            assert_eq!(replies.len(), 4);
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
        .write_all(mcp_lines(requests).as_bytes())
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

#[test]
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
    let ids: Vec<&Value> = replies.iter().map(|r| &r["id"]).collect();
    assert_eq!(
        ids,
        [1, 2, 3, 4, 5, 6]
            .map(|n| json!(n))
            .iter()
            .collect::<Vec<_>>()
    );
    assert_eq!(replies[0]["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(replies[0]["result"]["capabilities"], json!({"tools":{}}));
    let names: Vec<&str> = replies[1]["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "search_aasb_pages",
            "list_aasb_documents",
            "read_aasb_paragraph",
            "verify_aasb_quote"
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
    let mut child = Command::new(env!("CARGO_BIN_EXE_aasb-corpus"))
        .arg("mcp")
        .arg(&fixture.0)
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
    assert!(output.stderr.is_empty());
    let replies: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
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
