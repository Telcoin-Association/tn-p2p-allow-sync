//! Loopback JSON-RPC over HTTP/1.1.
//!
//! [`RpcClient::call`] sends one `POST /` to `127.0.0.1` on the node's RPC port and returns the `result` of the JSON-RPC response.
//! The address is a literal [`SocketAddr`], so no name is ever resolved.
//! The request says `Connection: close`, which leaves three ways for the response body to end: `Content-Length`, `chunked`, or the end of the stream.
//!
//! Every way a call can fail is one [`RpcError`], and [`RpcError::token`] gives its `STALE_REASON` token.
//! Connecting is bounded by a connect timeout, each read by a read timeout, and the whole call by an overall deadline that is checked before every read, so a server that drips one byte at a time cannot hold a run past the deadline.
//! A timeout of any of the three is [`RpcError::Unreachable`], the same as a refused connection.
//! The response head is capped at [`MAX_HEADER_BYTES`] and the decoded body at [`MAX_BODY_BYTES`]; a response that passes either cap is refused without buffering any more of it.

use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use serde_json::{Map, Value};

/// Largest decoded response body accepted, in bytes: 8 MiB.
pub const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

/// Largest response head accepted, in bytes: the status line, the header lines and the blank line that ends them, 16 KiB in all.
pub const MAX_HEADER_BYTES: usize = 16 * 1024;

/// Longest chunk-size line accepted in a chunked body, CRLF included.
///
/// Size lines are framing, not body, so they are capped one at a time rather than counted against either cap.
const MAX_CHUNK_LINE_BYTES: usize = 1024;

/// Default time allowed to connect.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// Default time allowed for each read.
const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// Default time allowed for the whole call, connect included.
const DEADLINE: Duration = Duration::from_secs(10);

/// The JSON-RPC request id; each call is the only request on its connection.
const REQUEST_ID: u64 = 1;

/// A JSON-RPC client for the node's RPC port on `127.0.0.1`.
///
/// Each [`call`](Self::call) opens its own connection and closes it once the response has been read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RpcClient {
    port: u16,
    connect_timeout: Duration,
    read_timeout: Duration,
    deadline: Duration,
}

impl RpcClient {
    /// A client for `127.0.0.1:port` with the default timeouts: 2 s to connect, 5 s for each read and 10 s for the whole call.
    pub const fn new(port: u16) -> Self {
        Self {
            port,
            connect_timeout: CONNECT_TIMEOUT,
            read_timeout: READ_TIMEOUT,
            deadline: DEADLINE,
        }
    }

    /// A client with shorter timeouts, so a test that stalls fails in well under the default deadline.
    #[cfg(test)]
    pub(crate) const fn with_timeouts(
        port: u16,
        connect_timeout: Duration,
        read_timeout: Duration,
        deadline: Duration,
    ) -> Self {
        Self {
            port,
            connect_timeout,
            read_timeout,
            deadline,
        }
    }

    /// Calls `method` with `params` and returns the `result` of the response, which is always a JSON object.
    ///
    /// # Errors
    ///
    /// - [`RpcError::Unreachable`] when the connection is refused, a write or read fails, the server closes before any response byte, or a timeout or the deadline passes.
    /// - [`RpcError::Http`] when the status is not 200, and [`RpcError::BadHttp`] when the response breaks HTTP/1.1 framing.
    /// - [`RpcError::BodyTooLarge`] when the head passes [`MAX_HEADER_BYTES`] or the body passes [`MAX_BODY_BYTES`].
    /// - [`RpcError::MethodMissing`] or [`RpcError::JsonRpc`] when the response is a JSON-RPC error.
    /// - [`RpcError::Malformed`] when the body is not JSON, has no `result`, or its `result` is not an object.
    pub fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let started = Instant::now();
        let addr = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), self.port);
        // the deadline bounds the connect as well, so the whole call ends within it
        let mut stream = TcpStream::connect_timeout(&addr, self.connect_timeout.min(self.deadline))
            .map_err(io_error)?;
        stream
            .write_all(&request(self.port, method, params))
            .map_err(io_error)?;
        let mut reader = BufReader::new(TimedStream {
            stream,
            read_timeout: self.read_timeout,
            deadline: self.deadline,
            started,
        });
        let body = read_response(&mut reader)?;
        drop(reader);
        let response = serde_json::from_slice::<Value>(&body);
        // the raw body is not kept alive next to the value parsed from it
        drop(body);
        into_result(response.map_err(|_| RpcError::Malformed)?)
    }
}

/// Why a call returned no result.
///
/// [`RpcError::token`] names each one as it appears in `STALE_REASON`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RpcError {
    /// The connection was refused, broke or closed before any response byte, or a timeout or the overall deadline passed: `rpc-unreachable`.
    Unreachable,
    /// The response status was not 200: `rpc-http-<code>`.
    Http(u16),
    /// The response broke HTTP/1.1 framing: `rpc-bad-http`.
    BadHttp,
    /// The response head passed [`MAX_HEADER_BYTES`] or the decoded body passed [`MAX_BODY_BYTES`]: `rpc-body-too-large`.
    BodyTooLarge,
    /// The body was not JSON, had no `result`, or its `result` was not an object: `snapshot-malformed`.
    Malformed,
    /// The node answered JSON-RPC error -32601, method not found, so this node release lacks the method: `rpc-method-missing`.
    MethodMissing,
    /// The node answered any other JSON-RPC error, with this code: `rpc-error-<code>`.
    JsonRpc(i64),
}

impl RpcError {
    /// The `STALE_REASON` token for this error, such as `rpc-unreachable` or `rpc-http-503`.
    pub fn token(&self) -> String {
        self.to_string()
    }
}

impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreachable => f.write_str("rpc-unreachable"),
            Self::Http(code) => write!(f, "rpc-http-{code}"),
            Self::BadHttp => f.write_str("rpc-bad-http"),
            Self::BodyTooLarge => f.write_str("rpc-body-too-large"),
            Self::Malformed => f.write_str("snapshot-malformed"),
            Self::MethodMissing => f.write_str("rpc-method-missing"),
            Self::JsonRpc(code) => write!(f, "rpc-error-{code}"),
        }
    }
}

impl std::error::Error for RpcError {}

/// The connection, read under the per-read timeout and the overall deadline.
struct TimedStream {
    stream: TcpStream,
    read_timeout: Duration,
    deadline: Duration,
    started: Instant,
}

impl Read for TimedStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // checked before every read, so a server that answers each read just inside the read
        // timeout still cannot hold the call past the deadline
        let left = self.deadline.saturating_sub(self.started.elapsed());
        if left.is_zero() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        self.stream
            .set_read_timeout(Some(self.read_timeout.min(left)))?;
        self.stream.read(buf)
    }
}

/// How the response body ends.
enum Framing {
    /// After exactly this many bytes.
    Length(usize),
    /// At the last chunk of a chunked body.
    Chunked,
    /// When the server closes the connection.
    Eof,
}

/// A failed connect, write or read means the node is not answering, whatever the error kind.
fn io_error(_: io::Error) -> RpcError {
    RpcError::Unreachable
}

/// The request bytes: head and JSON body.
fn request(port: u16, method: &str, params: Value) -> Vec<u8> {
    let mut call = Map::new();
    call.insert("jsonrpc".to_owned(), Value::from("2.0"));
    call.insert("id".to_owned(), Value::from(REQUEST_ID));
    call.insert("method".to_owned(), Value::from(method));
    call.insert("params".to_owned(), params);
    let body = Value::Object(call).to_string();
    let mut request = format!(
        "POST / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    request.extend_from_slice(body.as_bytes());
    request
}

/// Reads one response and returns its decoded body.
///
/// The head is capped at [`MAX_HEADER_BYTES`] and the decoded body at [`MAX_BODY_BYTES`], and every failure is the [`RpcError`] that [`RpcClient::call`] returns for it.
/// Public for the fuzz targets in `fuzz/`; the program reads a response only through [`RpcClient::call`].
pub fn read_response<R: BufRead>(reader: &mut R) -> Result<Vec<u8>, RpcError> {
    // a server that closes before its first byte never answered: the connection broke, not the framing
    if reader.fill_buf().map_err(io_error)?.is_empty() {
        return Err(RpcError::Unreachable);
    }
    let mut budget = MAX_HEADER_BYTES;
    let status = read_line(reader, &mut budget, RpcError::BodyTooLarge)?;
    match status_code(&status)? {
        200 => {}
        code => return Err(RpcError::Http(code)),
    }
    match read_headers(reader, &mut budget)? {
        Framing::Length(length) => read_length(reader, length),
        Framing::Chunked => read_chunked(reader),
        Framing::Eof => read_to_eof(reader),
    }
}

/// Reads one line ending in CRLF and returns it without the CRLF, charging every byte read to `budget`.
///
/// A line still open when the budget runs out is `over`; a bare LF, or the end of the stream inside a line, is bad framing.
fn read_line<R: BufRead>(
    reader: &mut R,
    budget: &mut usize,
    over: RpcError,
) -> Result<Vec<u8>, RpcError> {
    let mut line = Vec::new();
    reader
        .by_ref()
        .take(to_u64(*budget))
        .read_until(b'\n', &mut line)
        .map_err(io_error)?;
    *budget = budget.saturating_sub(line.len());
    if line.ends_with(b"\r\n") {
        line.truncate(line.len().saturating_sub(2));
        Ok(line)
    } else if line.ends_with(b"\n") || *budget > 0 {
        Err(RpcError::BadHttp)
    } else {
        Err(over)
    }
}

/// The status code of an `HTTP/1.1` or `HTTP/1.0` status line.
fn status_code(line: &[u8]) -> Result<u16, RpcError> {
    let rest = line
        .strip_prefix(b"HTTP/1.1 ")
        .or_else(|| line.strip_prefix(b"HTTP/1.0 "))
        .ok_or(RpcError::BadHttp)?;
    let (code, reason) = rest.split_at_checked(3).ok_or(RpcError::BadHttp)?;
    if !(reason.is_empty() || reason.starts_with(b" ")) {
        return Err(RpcError::BadHttp);
    }
    u16::try_from(parse_digits(code, 10)?).map_err(|_| RpcError::BadHttp)
}

/// Reads the header lines up to the blank line and returns how the body is framed.
///
/// Only `Content-Length` and `Transfer-Encoding` matter; every other field is checked for shape and ignored.
fn read_headers<R: BufRead>(reader: &mut R, budget: &mut usize) -> Result<Framing, RpcError> {
    let mut length = None;
    let mut chunked = false;
    loop {
        let line = read_line(reader, budget, RpcError::BodyTooLarge)?;
        if line.is_empty() {
            break;
        }
        let (name, value) = split_field(&line)?;
        if name.eq_ignore_ascii_case(b"content-length") {
            let n = parse_digits(value, 10)?;
            if n > MAX_BODY_BYTES {
                return Err(RpcError::BodyTooLarge);
            }
            // repeated lengths are framing only when they agree
            if length.is_some_and(|m| m != n) {
                return Err(RpcError::BadHttp);
            }
            length = Some(n);
        } else if name.eq_ignore_ascii_case(b"transfer-encoding") {
            // a single `chunked` is the only coding there is a decoder for; a second header
            // would apply a coding on top of it
            if chunked || !value.eq_ignore_ascii_case(b"chunked") {
                return Err(RpcError::BadHttp);
            }
            chunked = true;
        }
    }
    match (chunked, length) {
        // both at once is the response-splitting shape, refused rather than resolved
        (true, Some(_)) => Err(RpcError::BadHttp),
        (true, None) => Ok(Framing::Chunked),
        (false, Some(n)) => Ok(Framing::Length(n)),
        (false, None) => Ok(Framing::Eof),
    }
}

/// Splits a header line into its name and its value, the value without surrounding whitespace.
fn split_field(line: &[u8]) -> Result<(&[u8], &[u8]), RpcError> {
    let mut parts = line.splitn(2, |&b| b == b':');
    let name = parts.next().unwrap_or_default();
    let value = parts.next().ok_or(RpcError::BadHttp)?;
    // a name must be a bare token: no whitespace before the colon and no folded continuation lines
    if name.is_empty() || !name.iter().all(|&b| is_token_byte(b)) {
        return Err(RpcError::BadHttp);
    }
    Ok((name, trim_ows(value)))
}

/// Whether `b` may appear in a field name (RFC 9110 `tchar`).
fn is_token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

/// `s` without leading or trailing spaces and tabs.
fn trim_ows(mut s: &[u8]) -> &[u8] {
    while let [b' ' | b'\t', rest @ ..] = s {
        s = rest;
    }
    while let [rest @ .., b' ' | b'\t'] = s {
        s = rest;
    }
    s
}

/// Parses a non-empty run of digits in `radix`.
///
/// Any other byte is bad framing; a value past `usize` is past every cap.
fn parse_digits(digits: &[u8], radix: u8) -> Result<usize, RpcError> {
    if digits.is_empty() {
        return Err(RpcError::BadHttp);
    }
    digits.iter().try_fold(0_usize, |value, &byte| {
        let digit = char::from(byte)
            .to_digit(u32::from(radix))
            .and_then(|digit| usize::try_from(digit).ok())
            .ok_or(RpcError::BadHttp)?;
        value
            .checked_mul(usize::from(radix))
            .and_then(|value| value.checked_add(digit))
            .ok_or(RpcError::BodyTooLarge)
    })
}

/// Reads a body of exactly `length` bytes, already checked against [`MAX_BODY_BYTES`].
fn read_length<R: Read>(reader: &mut R, length: usize) -> Result<Vec<u8>, RpcError> {
    let mut body = Vec::with_capacity(length);
    reader
        .by_ref()
        .take(to_u64(length))
        .read_to_end(&mut body)
        .map_err(io_error)?;
    // the stream ended before the promised length
    if body.len() != length {
        return Err(RpcError::BadHttp);
    }
    Ok(body)
}

/// Reads and decodes a chunked body.
///
/// The body is complete at the last chunk, so the trailer section is never read: the connection closes behind it.
fn read_chunked<R: BufRead>(reader: &mut R) -> Result<Vec<u8>, RpcError> {
    // the whole cap is reserved up front so the buffer never reallocates past it; pages that
    // are never written cost nothing
    let mut body = Vec::with_capacity(MAX_BODY_BYTES);
    loop {
        let mut budget = MAX_CHUNK_LINE_BYTES;
        let line = read_line(reader, &mut budget, RpcError::BadHttp)?;
        // chunk extensions after `;` are ignored
        let size_field = line.split(|&b| b == b';').next().unwrap_or_default();
        let size = parse_digits(trim_ows(size_field), 16)?;
        if size == 0 {
            return Ok(body);
        }
        if size > MAX_BODY_BYTES.saturating_sub(body.len()) {
            return Err(RpcError::BodyTooLarge);
        }
        let before = body.len();
        reader
            .by_ref()
            .take(to_u64(size))
            .read_to_end(&mut body)
            .map_err(io_error)?;
        if body.len().saturating_sub(before) != size {
            return Err(RpcError::BadHttp);
        }
        // the chunk data ends with CRLF and nothing else
        let mut budget = 2;
        if !read_line(reader, &mut budget, RpcError::BadHttp)?.is_empty() {
            return Err(RpcError::BadHttp);
        }
    }
}

/// Reads a body that ends when the server closes the connection.
fn read_to_eof<R: Read>(reader: &mut R) -> Result<Vec<u8>, RpcError> {
    // reserved up front for the same reason as in `read_chunked`
    let mut body = Vec::with_capacity(MAX_BODY_BYTES);
    reader
        .by_ref()
        .take(to_u64(MAX_BODY_BYTES))
        .read_to_end(&mut body)
        .map_err(io_error)?;
    if body.len() < MAX_BODY_BYTES {
        return Ok(body);
    }
    // one byte past the cap tells a body at the cap from one over it, and lands in its own
    // buffer so `body` never grows
    let mut past = Vec::new();
    reader
        .by_ref()
        .take(1)
        .read_to_end(&mut past)
        .map_err(io_error)?;
    if past.is_empty() {
        Ok(body)
    } else {
        Err(RpcError::BodyTooLarge)
    }
}

/// The `result` of a JSON-RPC response, or the error it carries.
fn into_result(response: Value) -> Result<Value, RpcError> {
    let Value::Object(mut response) = response else {
        return Err(RpcError::Malformed);
    };
    match response.remove("error") {
        None | Some(Value::Null) => {}
        Some(error) => {
            return Err(match error.get("code").and_then(Value::as_i64) {
                Some(-32601) => RpcError::MethodMissing,
                Some(code) => RpcError::JsonRpc(code),
                None => RpcError::Malformed,
            });
        }
    }
    match response.remove("result") {
        Some(result @ Value::Object(_)) => Ok(result),
        _ => Err(RpcError::Malformed),
    }
}

/// `n` as a `u64` read limit; `usize` is never wider than 64 bits on a supported target.
fn to_u64(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::error::Error;
    use std::net::TcpListener;
    use std::thread::{self, JoinHandle};

    type TestResult = Result<(), Box<dyn Error>>;

    /// The server side of one connection: returns the request it read.
    type Server = JoinHandle<io::Result<Vec<u8>>>;

    const METHOD: &str = "tn_admissionSnapshot";

    const RESULT_JSON: &str = r#"{"version":1,"swarms":{"primary":{"available":true}}}"#;

    /// Timeouts short enough that a stalled test fails in about 2 s.
    fn client(port: u16) -> RpcClient {
        RpcClient::with_timeouts(
            port,
            Duration::from_millis(1000),
            Duration::from_millis(1000),
            Duration::from_millis(2000),
        )
    }

    fn call(client: &RpcClient) -> Result<Value, RpcError> {
        client.call(METHOD, Value::Array(Vec::new()))
    }

    fn expected_result() -> Result<Value, Box<dyn Error>> {
        Ok(serde_json::from_str(RESULT_JSON)?)
    }

    fn success_json() -> String {
        format!(r#"{{"jsonrpc":"2.0","id":1,"result":{RESULT_JSON}}}"#)
    }

    /// A 200 response with a `Content-Length` body.
    fn with_length(json: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{json}",
            json.len()
        )
        .into_bytes()
    }

    /// A JSON-RPC success response with an empty `pad` string, the shape [`padded_json`] fills.
    const PAD_SHELL: &str = r#"{"jsonrpc":"2.0","id":1,"result":{"pad":""}}"#;

    /// A JSON-RPC success response whose body is exactly `len` bytes long, and the `result` it carries.
    fn padded_json(len: usize) -> (String, Value) {
        let pad = "a".repeat(len.saturating_sub(PAD_SHELL.len()));
        let json = format!(r#"{{"jsonrpc":"2.0","id":1,"result":{{"pad":"{pad}"}}}}"#);
        let mut result = Map::new();
        result.insert("pad".to_owned(), Value::String(pad));
        (json, Value::Object(result))
    }

    /// Reads one request off `stream`: the head, then as many body bytes as its `Content-Length` says.
    fn read_request(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
        stream.set_read_timeout(Some(Duration::from_secs(3)))?;
        let mut request = Vec::new();
        let mut byte = [0_u8; 1];
        while !request.ends_with(b"\r\n\r\n") {
            if stream.read(&mut byte)? == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            request.extend_from_slice(&byte);
        }
        let length = String::from_utf8_lossy(&request)
            .lines()
            .find_map(|line| line.strip_prefix("Content-Length: "))
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0);
        let mut body = vec![0_u8; length];
        stream.read_exact(&mut body)?;
        request.extend_from_slice(&body);
        Ok(request)
    }

    /// Accepts one connection on a fresh loopback port, reads the request and hands the stream to `respond`.
    fn serve<F>(respond: F) -> io::Result<(u16, Server)>
    where
        F: FnOnce(&mut TcpStream) -> io::Result<()> + Send + 'static,
    {
        let listener = TcpListener::bind(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0))?;
        let port = listener.local_addr()?.port();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept()?;
            let request = read_request(&mut stream)?;
            // the client hangs up early on every refused response, so a failed write is expected
            let _ = respond(&mut stream);
            Ok(request)
        });
        Ok((port, server))
    }

    fn join(server: Server) -> Result<Vec<u8>, Box<dyn Error>> {
        Ok(server.join().map_err(|_| "server thread panicked")??)
    }

    /// Calls a server that answers with `response` and closes.
    fn call_against(response: Vec<u8>) -> Result<Result<Value, RpcError>, Box<dyn Error>> {
        let (port, server) = serve(move |stream| stream.write_all(&response))?;
        let result = call(&client(port));
        join(server)?;
        Ok(result)
    }

    #[test]
    fn request_is_one_post_with_the_four_headers() -> TestResult {
        let (port, server) = serve(|stream| stream.write_all(&with_length(&success_json())))?;
        let result = client(port).call(METHOD, Value::Array(Vec::new()))?;
        assert_eq!(result, expected_result()?);
        let request = join(server)?;
        let request = String::from_utf8(request)?;
        let (head, body) = request
            .split_once("\r\n\r\n")
            .ok_or("request has no blank line")?;
        assert_eq!(
            head,
            format!(
                "POST / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close",
                body.len()
            )
        );
        let body: Value = serde_json::from_str(body)?;
        let expected: Value = serde_json::from_str(
            r#"{"jsonrpc":"2.0","id":1,"method":"tn_admissionSnapshot","params":[]}"#,
        )?;
        assert_eq!(body, expected);
        Ok(())
    }

    #[test]
    fn content_length_body() -> TestResult {
        assert_eq!(
            call_against(with_length(&success_json()))?,
            Ok(expected_result()?)
        );
        Ok(())
    }

    #[test]
    fn chunked_body_is_decoded_and_trailers_ignored() -> TestResult {
        let json = success_json();
        let (first, second) = json.split_at_checked(10).ok_or("short json")?;
        let response = format!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: Chunked\r\n\r\n{:x}\r\n{first}\r\n{:X} ;ext=1\r\n{second}\r\n0\r\nX-Trailer: ignored\r\n\r\n",
            first.len(),
            second.len()
        );
        assert_eq!(call_against(response.into_bytes())?, Ok(expected_result()?));
        Ok(())
    }

    #[test]
    fn eof_delimited_body() -> TestResult {
        let response = format!(
            "HTTP/1.0 200 OK\r\nContent-Type: application/json\r\n\r\n{}",
            success_json()
        );
        assert_eq!(call_against(response.into_bytes())?, Ok(expected_result()?));
        Ok(())
    }

    #[test]
    fn non_200_status_is_an_http_error() -> TestResult {
        let response = b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n".to_vec();
        let result = call_against(response)?;
        assert_eq!(result, Err(RpcError::Http(503)));
        assert_eq!(
            result.err().map(|e| e.token()).as_deref(),
            Some("rpc-http-503")
        );
        Ok(())
    }

    #[test]
    fn a_close_before_any_byte_is_unreachable() -> TestResult {
        assert_eq!(call_against(Vec::new())?, Err(RpcError::Unreachable));
        Ok(())
    }

    #[test]
    fn garbage_status_line_is_bad_http() -> TestResult {
        for response in [
            &b"SSH-2.0-OpenSSH_9.6\r\n"[..],
            b"HTTP/2 200\r\n\r\n",
            b"HTTP/1.1 2x0 OK\r\n\r\n",
            b"HTTP/1.1 +20 OK\r\n\r\n",
            b"HTTP/1.1 20\r\n\r\n",
            b"HTTP/1.1 200OK\r\n\r\n",
            b"HTTP/1.1 200 OK\n\n",
            b"HTTP/1.1 200 OK",
        ] {
            assert_eq!(
                call_against(response.to_vec())?,
                Err(RpcError::BadHttp),
                "{}",
                String::from_utf8_lossy(response)
            );
        }
        Ok(())
    }

    #[test]
    fn broken_framing_is_bad_http() -> TestResult {
        let json = success_json();
        let cases = [
            // the stream ends before Content-Length bytes
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{json}",
                json.len().saturating_add(5)
            ),
            // both framings at once
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
                json.len()
            ),
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip, chunked\r\n\r\n0\r\n\r\n".to_owned(),
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n".to_owned(),
            format!("HTTP/1.1 200 OK\r\nContent-Length: 12a\r\n\r\n{json}"),
            format!("HTTP/1.1 200 OK\r\nContent-Length: -1\r\n\r\n{json}"),
            format!("HTTP/1.1 200 OK\r\nContent-Length: \r\n\r\n{json}"),
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Length: 1\r\n\r\n{json}",
                json.len()
            ),
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length : {}\r\n\r\n{json}",
                json.len()
            ),
            format!(
                "HTTP/1.1 200 OK\r\nX-Folded: a\r\n b\r\nContent-Length: {}\r\n\r\n{json}",
                json.len()
            ),
            format!(
                "HTTP/1.1 200 OK\r\nNo-Colon\r\nContent-Length: {}\r\n\r\n{json}",
                json.len()
            ),
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\n\r\n{json}",
                json.len()
            ),
            // the stream ends inside the head
            "HTTP/1.1 200 OK\r\nContent-Length: 5\r\n".to_owned(),
            // chunk size that is not hex
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n".to_owned(),
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n\r\n".to_owned(),
            // chunk data not followed by CRLF
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n{}xx0\r\n\r\n".to_owned(),
            // the stream ends inside a chunk, and before the last chunk
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n10\r\n{}".to_owned(),
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n{}\r\n".to_owned(),
            // a chunk-size line longer than any real one
            format!(
                "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2;{}\r\n{{}}\r\n0\r\n\r\n",
                "x".repeat(MAX_CHUNK_LINE_BYTES)
            ),
        ];
        for response in cases {
            assert_eq!(
                call_against(response.clone().into_bytes())?,
                Err(RpcError::BadHttp),
                "{response:?}"
            );
        }
        Ok(())
    }

    /// A 200 response with a `Content-Length` body whose head is exactly `head_len` bytes, padded by one header.
    fn padded_head(head_len: usize, json: &str) -> Result<Vec<u8>, Box<dyn Error>> {
        let shell = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nX-Pad: \r\n\r\n",
            json.len()
        );
        let pad = "a".repeat(head_len.saturating_sub(shell.len()));
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nX-Pad: {pad}\r\n\r\n",
            json.len()
        );
        if head.len() != head_len {
            return Err("pad arithmetic is off".into());
        }
        Ok(format!("{head}{json}").into_bytes())
    }

    #[test]
    fn head_at_the_cap_is_read() -> TestResult {
        let response = padded_head(MAX_HEADER_BYTES, &success_json())?;
        assert_eq!(call_against(response)?, Ok(expected_result()?));
        Ok(())
    }

    #[test]
    fn head_over_the_cap_is_too_large() -> TestResult {
        let response = padded_head(MAX_HEADER_BYTES.saturating_add(1), &success_json())?;
        assert_eq!(call_against(response)?, Err(RpcError::BodyTooLarge));
        // a status line alone past the cap
        let response = format!("HTTP/1.1 200 {}\r\n\r\n", "a".repeat(MAX_HEADER_BYTES));
        assert_eq!(
            call_against(response.into_bytes())?,
            Err(RpcError::BodyTooLarge)
        );
        Ok(())
    }

    #[test]
    fn body_at_the_cap_is_read() -> TestResult {
        let (json, expected) = padded_json(MAX_BODY_BYTES);
        assert_eq!(json.len(), MAX_BODY_BYTES);
        assert_eq!(call_against(with_length(&json))?, Ok(expected.clone()));
        let mut eof = b"HTTP/1.1 200 OK\r\n\r\n".to_vec();
        eof.extend_from_slice(json.as_bytes());
        assert_eq!(call_against(eof)?, Ok(expected));
        Ok(())
    }

    #[test]
    fn body_over_the_cap_is_too_large() -> TestResult {
        let over = MAX_BODY_BYTES.saturating_add(1);
        let (json, _) = padded_json(over);
        assert_eq!(json.len(), over);

        // refused on the header alone; the body is never sent
        let response = format!("HTTP/1.1 200 OK\r\nContent-Length: {over}\r\n\r\n");
        assert_eq!(
            call_against(response.into_bytes())?,
            Err(RpcError::BodyTooLarge)
        );
        let response =
            "HTTP/1.1 200 OK\r\nContent-Length: 99999999999999999999999\r\n\r\n".to_owned();
        assert_eq!(
            call_against(response.into_bytes())?,
            Err(RpcError::BodyTooLarge)
        );

        let mut eof = b"HTTP/1.1 200 OK\r\n\r\n".to_vec();
        eof.extend_from_slice(json.as_bytes());
        assert_eq!(call_against(eof)?, Err(RpcError::BodyTooLarge));

        let response = format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{over:x}\r\n");
        assert_eq!(
            call_against(response.into_bytes())?,
            Err(RpcError::BodyTooLarge)
        );

        // two chunks that pass the cap only together
        let (first, second) = json
            .as_bytes()
            .split_at_checked(MAX_BODY_BYTES / 2)
            .ok_or("short json")?;
        let mut chunked = format!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n",
            first.len()
        )
        .into_bytes();
        chunked.extend_from_slice(first);
        chunked.extend_from_slice(format!("\r\n{:x}\r\n", second.len()).as_bytes());
        chunked.extend_from_slice(second);
        chunked.extend_from_slice(b"\r\n0\r\n\r\n");
        assert_eq!(call_against(chunked)?, Err(RpcError::BodyTooLarge));
        Ok(())
    }

    #[test]
    fn slow_drip_stops_at_the_deadline() -> TestResult {
        let response = with_length(&success_json());
        let (port, server) = serve(move |stream| {
            // each byte lands well inside the read timeout; only the deadline can stop the call
            for byte in response.chunks(1) {
                stream.write_all(byte)?;
                thread::sleep(Duration::from_millis(100));
            }
            Ok(())
        })?;
        let deadline = Duration::from_millis(1000);
        let client = RpcClient::with_timeouts(
            port,
            Duration::from_millis(1000),
            Duration::from_millis(500),
            deadline,
        );
        let started = Instant::now();
        let result = call(&client);
        let elapsed = started.elapsed();
        assert_eq!(result, Err(RpcError::Unreachable));
        assert!(elapsed >= deadline, "stopped after {elapsed:?}");
        assert!(
            elapsed < Duration::from_millis(2500),
            "stopped after {elapsed:?}"
        );
        join(server)?;
        Ok(())
    }

    #[test]
    fn silent_server_stops_at_the_read_timeout() -> TestResult {
        // holds the connection open without a byte until the client hangs up
        let (port, server) = serve(|stream| stream.read(&mut [0_u8; 1]).map(drop))?;
        let deadline = Duration::from_millis(2500);
        let client = RpcClient::with_timeouts(
            port,
            Duration::from_millis(1000),
            Duration::from_millis(200),
            deadline,
        );
        let started = Instant::now();
        let result = call(&client);
        let elapsed = started.elapsed();
        assert_eq!(result, Err(RpcError::Unreachable));
        assert!(
            elapsed < Duration::from_millis(2000),
            "stopped after {elapsed:?}"
        );
        join(server)?;
        Ok(())
    }

    #[test]
    fn method_not_found_is_method_missing() -> TestResult {
        let json =
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"Method not found"}}"#;
        let result = call_against(with_length(json))?;
        assert_eq!(result, Err(RpcError::MethodMissing));
        assert_eq!(
            result.err().map(|e| e.token()).as_deref(),
            Some("rpc-method-missing")
        );
        Ok(())
    }

    #[test]
    fn other_json_rpc_error_keeps_its_code() -> TestResult {
        for (json, error) in [
            (
                r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32001,"message":"not found"}}"#,
                RpcError::JsonRpc(-32001),
            ),
            (
                r#"{"jsonrpc":"2.0","id":1,"error":{"code":3,"message":"execution reverted"},"result":{}}"#,
                RpcError::JsonRpc(3),
            ),
        ] {
            assert_eq!(call_against(with_length(json))?, Err(error), "{json}");
        }
        Ok(())
    }

    #[test]
    fn anything_but_an_object_result_is_malformed() -> TestResult {
        for json in [
            r#"{"jsonrpc":"2.0","id":1,"result":[]}"#,
            r#"{"jsonrpc":"2.0","id":1,"result":null}"#,
            r#"{"jsonrpc":"2.0","id":1,"result":"ok"}"#,
            r#"{"jsonrpc":"2.0","id":1,"result":1}"#,
            r#"{"jsonrpc":"2.0","id":1}"#,
            r#"{"jsonrpc":"2.0","id":1,"error":{"message":"no code"}}"#,
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":"-32601"}}"#,
            r#"{"jsonrpc":"2.0","id":1,"error":"text"}"#,
            r#"[{"jsonrpc":"2.0","id":1,"result":{}}]"#,
            r#"{"jsonrpc":"2.0","id":1,"result":{}} trailing"#,
            "not json",
            "",
        ] {
            assert_eq!(
                call_against(with_length(json))?,
                Err(RpcError::Malformed),
                "{json}"
            );
        }
        Ok(())
    }

    #[test]
    fn null_error_is_no_error() -> TestResult {
        let json = format!(r#"{{"jsonrpc":"2.0","id":1,"error":null,"result":{RESULT_JSON}}}"#);
        assert_eq!(call_against(with_length(&json))?, Ok(expected_result()?));
        Ok(())
    }

    #[test]
    fn refused_connection_is_unreachable() -> TestResult {
        let listener = TcpListener::bind(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0))?;
        let port = listener.local_addr()?.port();
        drop(listener);
        let result = call(&client(port));
        assert_eq!(result, Err(RpcError::Unreachable));
        assert_eq!(
            result.err().map(|e| e.token()).as_deref(),
            Some("rpc-unreachable")
        );
        Ok(())
    }

    #[test]
    fn tokens_follow_the_contract() {
        let tokens = [
            RpcError::Unreachable,
            RpcError::Http(503),
            RpcError::Http(404),
            RpcError::BadHttp,
            RpcError::BodyTooLarge,
            RpcError::Malformed,
            RpcError::MethodMissing,
            RpcError::JsonRpc(-32001),
            RpcError::JsonRpc(3),
        ]
        .map(|error| error.token());
        assert_eq!(
            tokens,
            [
                "rpc-unreachable",
                "rpc-http-503",
                "rpc-http-404",
                "rpc-bad-http",
                "rpc-body-too-large",
                "snapshot-malformed",
                "rpc-method-missing",
                "rpc-error--32001",
                "rpc-error-3",
            ]
        );
    }

    #[test]
    fn new_uses_the_default_timeouts() {
        assert_eq!(
            RpcClient::new(8545),
            RpcClient::with_timeouts(
                8545,
                Duration::from_secs(2),
                Duration::from_secs(5),
                Duration::from_secs(10),
            )
        );
    }
}
