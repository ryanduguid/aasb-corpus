use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

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
    for command in [
        vec!["build", root],
        vec!["stats", root],
        vec!["catalogue", root],
        vec!["search", root, "lease"],
        vec!["paragraph", root, &sha, "26"],
        vec!["page", root, &sha, "1"],
        vec!["verify", root],
        vec!["verify-quote", root, &sha, "1", "Fabricated cobalt"],
    ] {
        let output = Command::new("strace")
            .args(["-f", "-qq", "-e", "trace=%network,execve", "-o", log])
            .arg(env!("CARGO_BIN_EXE_aasb-corpus"))
            .args(&command)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
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
