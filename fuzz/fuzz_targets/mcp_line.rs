//! Any bytes as the MCP server's stdin: reading never panics, a line over the limit is
//! refused and dropped before anything can parse it, and every other line comes back
//! exactly as split at its newline.
#![no_main]

use aasb_corpus::{MAX_LINE, Next, next_line};
use libfuzzer_sys::fuzz_target;
use std::io::BufReader;

/// Reads `stream` with `limit` through a `capacity`-byte buffer and checks each result
/// against splitting the stream at every newline.
fn check(stream: &[u8], limit: usize, capacity: usize) {
    let mut expected = stream.split(|b| *b == b'\n').collect::<Vec<_>>();
    // A final newline ends the last line; it does not start an empty one.
    if expected.last().is_some_and(|last| last.is_empty()) {
        expected.pop();
    }
    let mut reader = BufReader::with_capacity(capacity, stream);
    let mut line = Vec::new();
    for want in expected {
        match next_line(&mut reader, &mut line, limit).expect("a slice reads without error") {
            Next::Line => {
                assert!(want.len() <= limit, "a line over the limit was returned");
                assert_eq!(line, want);
                // The server parses only a line returned here.
                let _ = serde_json::from_slice::<serde_json::Value>(&line);
            }
            Next::TooLong => {
                assert!(want.len() > limit, "a line within the limit was refused");
                assert!(line.is_empty(), "a refused line was kept for parsing");
            }
            Next::End => panic!("the stream ended before its last line"),
        }
    }
    let end = next_line(&mut reader, &mut line, limit).expect("a slice reads without error");
    assert!(matches!(end, Next::End), "a line was read past the end");
}

fuzz_target!(|data: &[u8]| {
    // As the server reads stdin.
    check(data, MAX_LINE, 8192);
    // A limit and buffer size the input chooses, so short lines cross both.
    if let [limit, capacity, rest @ ..] = data {
        check(rest, usize::from(*limit), usize::from(*capacity) + 1);
    }
    // A first line one byte over the real limit, whatever follows it.
    if data.first() == Some(&0) {
        let stream = [vec![b'{'; MAX_LINE + 1], data.to_vec()].concat();
        check(&stream, MAX_LINE, 8192);
    }
});
