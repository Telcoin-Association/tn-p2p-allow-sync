//! A loopback JSON-RPC server for the integration tests: a `TcpListener` on `127.0.0.1:0` that a test thread serves from a queue of canned responses, one per connection.
//!
//! The tool closes every connection after one call, so each call takes the next [`Response`] off the queue.
//! Every request body is recorded, in order, for the test to inspect.
//! The server answers HTTP 503 once the queue is empty, so a call the test did not plan for fails loudly rather than hanging.
//! The listener stays bound for the server's whole life, so no other socket of the test process can take its port while the server is down.

use std::collections::VecDeque;
use std::fs;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// How long the server waits on a client before it gives up on the connection.
const CLIENT_TIMEOUT: Duration = Duration::from_secs(15);

/// The most header bytes the server reads from one request.
const MAX_HEADER_BYTES: usize = 64 * 1024;

/// One canned answer to one call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    /// HTTP 200 with this body, normally a whole JSON-RPC response.
    Json(String),
    /// This HTTP status with an empty body.
    Status(u16),
    /// Reads the request and never answers; the connection is held until the client gives up.
    Hang,
    /// Removes the file at the path, as something outside the tool would while the node is being asked, then answers with the inner response.
    ///
    /// A missing file is no error, so a test can name a file the tool may already have removed.
    Removing(PathBuf, Box<Response>),
    /// The node is down: every connection is closed before its request is read, with no response byte, so the call breaks as `rpc-unreachable`.
    ///
    /// The server stays down until the next [`FakeRpc::set`]; entries queued after a `Down` are never served, and calls made while it is down are not recorded.
    Down,
}

/// A JSON-RPC response whose `result` is the JSON text `result`.
pub fn result(result: &str) -> Response {
    Response::Json(format!(r#"{{"jsonrpc":"2.0","id":1,"result":{result}}}"#))
}

/// A JSON-RPC error response with `code`.
pub fn error(code: i64) -> Response {
    Response::Json(format!(
        r#"{{"jsonrpc":"2.0","id":1,"error":{{"code":{code},"message":"canned error"}}}}"#
    ))
}

#[derive(Debug, Default)]
struct Shared {
    queue: VecDeque<Response>,
    requests: Vec<String>,
    /// Set on reaching a `Down` at the head of the queue, cleared by the next [`FakeRpc::set`].
    down: bool,
    stop: bool,
}

fn lock(shared: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    // a test thread that panicked while holding the lock fails its own test; the data is still usable
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The server: its port, which stays the same across [`FakeRpc::set`], and the thread serving it.
#[derive(Debug)]
pub struct FakeRpc {
    port: u16,
    shared: Arc<Mutex<Shared>>,
    thread: Option<JoinHandle<()>>,
}

impl FakeRpc {
    /// Binds `127.0.0.1:0` and serves `responses` in order; a leading [`Response::Down`] starts it down.
    pub fn start(responses: Vec<Response>) -> io::Result<Self> {
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))?;
        let port = listener.local_addr()?.port();
        let shared = Arc::new(Mutex::new(Shared {
            queue: responses.into(),
            ..Shared::default()
        }));
        let serving = Arc::clone(&shared);
        let thread = Some(thread::spawn(move || serve(&listener, &serving)));
        Ok(Self {
            port,
            shared,
            thread,
        })
    }

    /// The port the config's `RPC_PORT` names.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Replaces the queue with `responses`, bringing the server up again when it was down.
    ///
    /// A leading [`Response::Down`] takes the server down, so the next call breaks with no response byte.
    pub fn set(&mut self, responses: Vec<Response>) -> io::Result<()> {
        // swapped under the lock the serving thread takes for every connection, so each call
        // sees either the old queue or the new one
        let mut shared = lock(&self.shared);
        shared.queue = responses.into();
        shared.down = false;
        Ok(())
    }

    /// Every request body received so far, in order.
    pub fn requests(&self) -> Vec<String> {
        lock(&self.shared).requests.clone()
    }

    /// The JSON-RPC method of every request received so far, in order.
    pub fn methods(&self) -> Vec<String> {
        self.requests()
            .iter()
            .map(|body| {
                serde_json::from_str::<serde_json::Value>(body)
                    .ok()
                    .and_then(|call| call.get("method")?.as_str().map(str::to_owned))
                    .unwrap_or_default()
            })
            .collect()
    }
}

impl Drop for FakeRpc {
    /// Stops the serving thread and waits until its listener is closed.
    fn drop(&mut self) {
        let Some(thread) = self.thread.take() else {
            return;
        };
        lock(&self.shared).stop = true;
        // the thread sits in accept; a connection of our own wakes it to see the flag
        let _ = TcpStream::connect_timeout(
            &SocketAddr::from((Ipv4Addr::LOCALHOST, self.port)),
            Duration::from_secs(2),
        );
        let _ = thread.join();
    }
}

/// The serving loop: one response per connection, and a close with no response byte while down, until the server is stopped.
fn serve(listener: &TcpListener, shared: &Mutex<Shared>) {
    loop {
        let Ok((mut stream, _)) = listener.accept() else {
            continue;
        };
        {
            let mut shared = lock(shared);
            if shared.stop {
                return;
            }
            if matches!(shared.queue.front(), Some(Response::Down)) {
                shared.queue.clear();
                shared.down = true;
            }
            if shared.down {
                // closed unread: the client's call breaks before any response byte
                continue;
            }
        }
        let body = read_request(&mut stream).unwrap_or_default();
        let response = {
            let mut shared = lock(shared);
            shared.requests.push(body);
            shared.queue.pop_front()
        };
        respond(&mut stream, response);
    }
}

/// Answers one call with `response`, or with HTTP 503 when the queue was empty.
fn respond(stream: &mut TcpStream, response: Option<Response>) {
    match response {
        Some(Response::Json(body)) => answer(stream, "200 OK", &body),
        Some(Response::Status(code)) => answer(stream, &format!("{code} Canned"), ""),
        Some(Response::Hang) => hold(stream),
        Some(Response::Removing(path, then)) => {
            // a file already gone is what the test wanted
            let _ = fs::remove_file(&path);
            respond(stream, Some(*then));
        }
        // the head is checked before the request is read, so only a `set` that races a call
        // pops one, and that call breaks as it would on a node that is down
        Some(Response::Down) => {}
        None => answer(stream, "503 Service Unavailable", ""),
    }
}

/// Reads one request and returns its body as text.
fn read_request(stream: &mut TcpStream) -> io::Result<String> {
    stream.set_read_timeout(Some(CLIENT_TIMEOUT))?;
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() >= MAX_HEADER_BYTES || stream.read(&mut byte)? == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "no request head",
            ));
        }
        head.extend_from_slice(&byte);
    }
    let head = String::from_utf8_lossy(&head);
    let length = head
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse::<u64>().ok())
        .unwrap_or(0);
    let mut body = Vec::new();
    stream.take(length).read_to_end(&mut body)?;
    Ok(String::from_utf8_lossy(&body).into_owned())
}

fn answer(stream: &mut TcpStream, status: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    // a client that gave up early fails its own test
    let _ = stream
        .write_all(response.as_bytes())
        .and_then(|()| stream.flush());
}

/// Holds the connection open without answering until the client closes it.
fn hold(stream: &mut TcpStream) {
    let mut sink = [0_u8; 64];
    while let Ok(n) = stream.read(&mut sink) {
        if n == 0 {
            break;
        }
    }
}
