//! The 2 parsers that read untrusted bytes: [`Page`], whose derived `Deserialize` reads a
//! `pages.jsonl` line, and the MCP server's bounded request-line reader. The `aasb-corpus`
//! binary uses them, and the fuzz targets in `fuzz/` exercise them.
use serde::{Deserialize, Serialize};
use std::io::{BufRead, Read};

/// One physical page of an extraction, as a line of `pages.jsonl` holds it.
#[derive(Deserialize, Serialize)]
pub struct Page {
    pub page: u32,
    pub text: String,
    pub provenance: String,
    pub needs_review: bool,
}

/// The longest request line read, in bytes. A longer line is refused unread past this.
pub const MAX_LINE: usize = 1 << 20;

/// One request line, or the sign that it was longer than the limit or stdin closed.
pub enum Next {
    Line,
    TooLong,
    End,
}

/// Reads the next line into `line` (cleared first), holding at most `limit` + 1 of its
/// bytes. The rest of an over-long line is read and dropped without being stored.
///
/// # Errors
///
/// When reading `input` fails.
pub fn next_line(
    input: &mut impl BufRead,
    line: &mut Vec<u8>,
    limit: usize,
) -> std::io::Result<Next> {
    line.clear();
    let cap = u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1);
    if (&mut *input).take(cap).read_until(b'\n', line)? == 0 {
        return Ok(Next::End);
    }
    if line.last() == Some(&b'\n') {
        line.pop();
        return Ok(Next::Line);
    }
    if line.len() <= limit {
        return Ok(Next::Line);
    }
    line.clear();
    loop {
        let buffer = input.fill_buf()?;
        if buffer.is_empty() {
            return Ok(Next::TooLong);
        }
        let (used, done) = buffer
            .iter()
            .position(|b| *b == b'\n')
            .map_or((buffer.len(), false), |at| (at + 1, true));
        input.consume(used);
        if done {
            return Ok(Next::TooLong);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn next(reader: &mut impl BufRead, line: &mut Vec<u8>) -> Next {
        next_line(reader, line, 1024).unwrap()
    }

    #[test]
    fn an_over_long_line_is_refused_without_being_held() {
        let long = vec![b'x'; 1 << 20];
        let input = [&long[..], b"\n{\"short\":1}\nlast"].concat();
        let mut reader = std::io::BufReader::with_capacity(64, &input[..]);
        let mut line = Vec::new();
        assert!(matches!(next(&mut reader, &mut line), Next::TooLong));
        // At most the limit and one byte, with a vector's growth, is ever stored.
        assert!(line.capacity() <= 2 * 1025, "{}", line.capacity());
        assert!(matches!(next(&mut reader, &mut line), Next::Line));
        assert_eq!(line, b"{\"short\":1}");
        assert!(matches!(next(&mut reader, &mut line), Next::Line));
        assert_eq!(line, b"last");
        assert!(matches!(next(&mut reader, &mut line), Next::End));
    }

    #[test]
    fn a_line_at_the_limit_is_read_and_one_byte_more_is_refused() {
        let input = [vec![b'y'; 1024], b"\n".to_vec(), vec![b'z'; 1025]].concat();
        let mut reader = &input[..];
        let mut line = Vec::new();
        assert!(matches!(next(&mut reader, &mut line), Next::Line));
        assert_eq!(line.len(), 1024);
        assert!(matches!(next(&mut reader, &mut line), Next::TooLong));
        assert!(matches!(next(&mut reader, &mut line), Next::End));
    }

    #[test]
    fn a_parsed_page_serialises_back_to_the_same_fields() {
        let line = r#"{"page":1,"text":"A fabricated lease liability","provenance":"native","needs_review":false}"#;
        let page: Page = serde_json::from_str(line).unwrap();
        assert_eq!(serde_json::to_string(&page).unwrap(), line);
        assert!(serde_json::from_str::<Page>(r#"{"page":1}"#).is_err());
    }
}
