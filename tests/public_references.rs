//! Read-only qualification of an explicitly supplied, extracted public snapshot.
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::PathBuf;
use std::process::Command;

#[test]
#[ignore = "requires hash-pinned, built snapshot with both actual readers in AASB_PUBLIC_SNAPSHOT"]
fn official_paragraphs_have_independent_page_and_reader_confirmation() {
    let reference: Value =
        serde_json::from_str(include_str!("fixtures/public-references.json")).unwrap();
    let root =
        PathBuf::from(std::env::var_os("AASB_PUBLIC_SNAPSHOT").expect("set AASB_PUBLIC_SNAPSHOT"));
    let sha = reference["sha256"].as_str().unwrap();
    let sources: Value =
        serde_json::from_slice(&fs::read(root.join("source/manifest.json")).unwrap()).unwrap();
    let source = sources["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["sha256"] == sha)
        .expect("pinned source");
    assert_eq!(source["source_url"], reference["source_url"]);
    assert_eq!(source["page_count"], 23);
    let filename = source["filename"].as_str().unwrap();
    assert_eq!(
        std::path::Path::new(filename).file_name().unwrap(),
        filename,
        "source filename only"
    );
    assert!(!filename.contains(['/', '\\', ':']));
    let digest: String = Sha256::digest(fs::read(root.join("source").join(filename)).unwrap())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(digest, sha);
    let call = |args: &[&str]| -> Value {
        let out = Command::new(env!("CARGO_BIN_EXE_aasb-corpus"))
            .arg(args[0])
            .arg(&root)
            .args(&args[1..])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    };
    call(&["verify"]);
    let catalogue = call(&["catalogue", "--standard", "AASB 116", "--version", sha]);
    assert_eq!(catalogue["results"].as_array().unwrap().len(), 1);
    for expected in reference["paragraphs"].as_array().unwrap() {
        let found = call(&["paragraph", sha, expected["number"].as_str().unwrap()]);
        assert_eq!(
            found["status"], "found",
            "paragraph {}: {found}",
            expected["number"]
        );
        assert_eq!(found["second_reader"]["status"], "confirmed");
        assert_eq!(found["candidate_count"], 1);
        let candidate = &found["candidates"][0];
        assert_eq!(candidate["next_paragraph"], expected["next"]);
        let fragments = candidate["fragments"].as_array().unwrap();
        assert!(!fragments.is_empty());
        assert!(fragments.iter().all(|f| f["pdf_page"] == expected["page"]));
        assert!(
            fragments
                .iter()
                .all(|f| f["start_line"].as_u64().unwrap() <= f["end_line"].as_u64().unwrap())
        );
        if expected["conditions"] == true {
            let text = fragments
                .iter()
                .map(|f| f["text"].as_str().unwrap())
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                text.contains("(a)") && text.contains("(b)"),
                "child conditions retained"
            );
        }
    }
    let missing = call(&[
        "search",
        "nonexistentquartzterm",
        "--standard",
        "AASB 116",
        "--version",
        sha,
    ]);
    assert!(missing["results"].as_array().unwrap().is_empty());
}
