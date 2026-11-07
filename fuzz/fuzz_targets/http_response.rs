//! `rpc::read_response` over a cursor of any bytes: the HTTP/1.1 reader between the node's RPC socket and the JSON parser.
//!
//! Each input is read twice: as a whole response, and as the body of a response whose head says `Transfer-Encoding: chunked`,
//! so the chunked decoder is reached without a corpus of valid heads.
//! Inputs above 64 KiB are skipped to keep one run fast; the 8 MiB body cap is asserted, not reached.
//!
//! Oracles, for an accepted response:
//! - the body is at most `MAX_BODY_BYTES` and the head, up to and with its empty line, at most `MAX_HEADER_BYTES`;
//! - the status line is `HTTP/1.1 200` or `HTTP/1.0 200`, alone or followed by a space;
//! - a chunked body equals the de-chunking of the bytes after the head, written here from RFC 9112 section 7.1;
//! - any other body is a prefix of the bytes after the head, and all of them when the head has no `Content-Length`.

#![no_main]

use std::io::Cursor;

use libfuzzer_sys::fuzz_target;
use tn_p2p_allow_sync::rpc::{MAX_BODY_BYTES, MAX_HEADER_BYTES, read_response};

/// The largest input read; larger ones are skipped.
const MAX_INPUT: usize = 64 * 1024;

/// The head the second reading puts in front of the input.
const CHUNKED_HEAD: &[u8] = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n";

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT {
        return;
    }
    check(data);
    let mut framed = CHUNKED_HEAD.to_vec();
    framed.extend_from_slice(data);
    check(&framed);
});

fn check(input: &[u8]) {
    let Ok(body) = read_response(&mut Cursor::new(input)) else {
        return;
    };
    assert!(
        body.len() <= MAX_BODY_BYTES,
        "a body of {} bytes",
        body.len()
    );

    // every line of an accepted head ends in CRLF and holds no LF, so the first CRLFCRLF ends the head
    let head_len = input
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|at| at + 4)
        .expect("an accepted response has a head that ends in an empty line");
    assert!(head_len <= MAX_HEADER_BYTES, "a head of {head_len} bytes");
    let (head, rest) = input.split_at(head_len);
    let mut lines = head[..head_len - 4]
        .split(|&b| b == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line));
    let status = lines.next().unwrap_or_default();
    assert!(
        [&b"HTTP/1.1 200"[..], b"HTTP/1.0 200"].iter().any(|ok| {
            status
                .strip_prefix(*ok)
                .is_some_and(|reason| reason.is_empty() || reason.starts_with(b" "))
        }),
        "accepted with status line {:?}",
        String::from_utf8_lossy(status)
    );
    let names: Vec<&[u8]> = lines
        .filter_map(|line| line.split(|&b| b == b':').next())
        .collect();
    let has = |field: &[u8]| names.iter().any(|name| name.eq_ignore_ascii_case(field));

    // a Transfer-Encoding other than one `chunked`, or one next to a Content-Length, is refused
    if has(b"transfer-encoding") {
        assert_eq!(
            dechunk(rest).as_deref(),
            Some(body.as_slice()),
            "the chunked body differs from its de-chunking"
        );
    } else {
        assert!(
            rest.starts_with(&body),
            "the body is not the bytes after the head"
        );
        if !has(b"content-length") {
            assert_eq!(
                body.len(),
                rest.len(),
                "a body read to the end stopped early"
            );
        }
    }
}

/// The data of a chunked body: size lines of hex digits with optional spaces or tabs around them and `;` extensions after,
/// each chunk's data followed by CRLF, up to the size-0 chunk; the trailer section after it is not read.
///
/// `None` when the bytes break that grammar. No cap applies here: the reader's caps can only refuse more.
fn dechunk(mut rest: &[u8]) -> Option<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let end = rest.windows(2).position(|window| window == b"\r\n")?;
        let line = &rest[..end];
        rest = &rest[end + 2..];
        if line.contains(&b'\n') {
            return None;
        }
        let mut size = line.split(|&b| b == b';').next()?;
        while let [b' ' | b'\t', inner @ ..] | [inner @ .., b' ' | b'\t'] = size {
            size = inner;
        }
        let size = std::str::from_utf8(size).ok()?;
        // `from_str_radix` takes a sign, which a chunk size may not have
        if size.is_empty() || !size.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let size = usize::from_str_radix(size, 16).ok()?;
        if size == 0 {
            return Some(body);
        }
        body.extend_from_slice(rest.get(..size)?);
        rest = rest[size..].strip_prefix(b"\r\n")?;
    }
}
