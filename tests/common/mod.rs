//! A loopback HTTP/1.1 server for tests: each path serves bytes with ranges, a
//! status or a redirect, and can misbehave on purpose. Nothing leaves the machine.

#![allow(dead_code)]

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// What one path answers.
#[derive(Clone, Default)]
pub struct Body {
    data: Vec<u8>,
    status: Option<u16>,
    location: Option<String>,
    /// The first GET announces the whole length but sends only this many bytes.
    truncate_first: Option<usize>,
    /// Every GET sends this many bytes, then holds the connection open.
    stall_after: Option<usize>,
    /// Bytes per write and the pause between writes.
    slow: Option<(usize, Duration)>,
    /// No `Content-Length`: the body ends when the connection closes.
    no_length: bool,
}

impl Body {
    pub fn bytes(data: &[u8]) -> Self {
        Self {
            data: data.to_vec(),
            ..Self::default()
        }
    }

    pub fn status(code: u16) -> Self {
        Self {
            status: Some(code),
            ..Self::default()
        }
    }

    pub fn redirect(location: impl Into<String>) -> Self {
        Self {
            status: Some(302),
            location: Some(location.into()),
            ..Self::default()
        }
    }

    pub fn truncated_once(mut self, bytes: usize) -> Self {
        self.truncate_first = Some(bytes);
        self
    }

    pub fn stalling_after(mut self, bytes: usize) -> Self {
        self.stall_after = Some(bytes);
        self
    }

    pub fn slow(mut self, chunk: usize, pause: Duration) -> Self {
        self.slow = Some((chunk, pause));
        self
    }

    pub fn without_length(mut self) -> Self {
        self.no_length = true;
        self
    }
}

/// One request as the server saw it.
#[derive(Clone, Debug)]
pub struct Request {
    pub method: String,
    pub path: String,
    /// The `N` of `Range: bytes=N-` or `bytes=N-M`.
    pub range: Option<u64>,
    pub authorization: Option<String>,
    pub via_proxy: bool,
}

#[derive(Default)]
struct State {
    routes: HashMap<String, Body>,
    gets: HashMap<String, usize>,
    requests: Vec<Request>,
}

pub struct Server {
    base: String,
    state: Arc<Mutex<State>>,
}

impl Server {
    /// Listens on `127.0.0.1` at a free port until the test process ends.
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(State::default()));
        let shared = state.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let state = shared.clone();
                std::thread::spawn(move || handle(stream, &state));
            }
        });
        Self { base, state }
    }

    /// `http://127.0.0.1:<port>`.
    pub fn base(&self) -> String {
        self.base.clone()
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    pub fn serve(&self, path: &str, body: Body) {
        self.state
            .lock()
            .unwrap()
            .routes
            .insert(path.to_owned(), body);
    }

    pub fn requests(&self) -> Vec<Request> {
        self.state.lock().unwrap().requests.clone()
    }
}

fn read_head(stream: &mut TcpStream) -> Option<String> {
    let mut data = Vec::new();
    let mut byte = [0u8; 1];
    while !data.ends_with(b"\r\n\r\n") {
        match stream.read(&mut byte) {
            Ok(1) => data.push(byte[0]),
            _ => return None,
        }
    }
    Some(String::from_utf8_lossy(&data).into_owned())
}

fn handle(mut stream: TcpStream, state: &Mutex<State>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let Some(head) = read_head(&mut stream) else {
        return;
    };
    let mut lines = head.lines();
    let mut first = lines.next().unwrap_or_default().split_whitespace();
    let method = first.next().unwrap_or_default().to_owned();
    let target = first.next().unwrap_or_default();
    // A proxy is sent the absolute URL.
    let via_proxy = target.starts_with("http://");
    let path = if via_proxy {
        let rest = target.trim_start_matches("http://");
        rest.find('/').map_or("/", |i| &rest[i..]).to_owned()
    } else {
        target.to_owned()
    };
    let mut range = None;
    let mut range_end: Option<u64> = None;
    let mut authorization = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        match name.trim().to_ascii_lowercase().as_str() {
            "range" => {
                if let Some((from, to)) = value
                    .trim()
                    .strip_prefix("bytes=")
                    .and_then(|v| v.split_once('-'))
                {
                    range = from.trim().parse().ok();
                    range_end = to.trim().parse().ok();
                }
            }
            "authorization" => authorization = Some(value.trim().to_owned()),
            _ => {}
        }
    }
    let (body, nth) = {
        let mut state = state.lock().unwrap();
        state.requests.push(Request {
            method: method.clone(),
            path: path.clone(),
            range,
            authorization,
            via_proxy,
        });
        let body = state.routes.get(&path).cloned();
        let nth = if method == "GET" {
            let count = state.gets.entry(path.clone()).or_default();
            *count += 1;
            *count - 1
        } else {
            0
        };
        (body, nth)
    };
    let Some(body) = body else {
        let _ = stream.write_all(
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\nConnection: close\r\n\r\nnot found",
        );
        return;
    };
    if let Some(code) = body.status {
        let location = body
            .location
            .map(|to| format!("Location: {to}\r\n"))
            .unwrap_or_default();
        let text = format!("status {code}");
        let _ = stream.write_all(
            format!(
                "HTTP/1.1 {code} Test\r\n{location}Content-Length: {}\r\nConnection: close\r\n\r\n{text}",
                text.len()
            )
            .as_bytes(),
        );
        return;
    }
    let len = body.data.len() as u64;
    let start = range.unwrap_or(0);
    if range.is_some() && start >= len {
        let _ = stream.write_all(
            format!(
                "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{len}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .as_bytes(),
        );
        return;
    }
    let end = range_end.map_or(len - 1, |end| end.min(len - 1));
    let slice = &body.data[start as usize..=end as usize];
    let mut head = if range.is_some() {
        format!("HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{len}\r\n")
    } else {
        "HTTP/1.1 200 OK\r\nAccept-Ranges: bytes\r\n".to_owned()
    };
    if !body.no_length {
        head.push_str(&format!("Content-Length: {}\r\n", slice.len()));
    }
    head.push_str("Connection: close\r\n\r\n");
    if stream.write_all(head.as_bytes()).is_err() || method == "HEAD" {
        return;
    }
    let mut send = slice;
    if let Some(n) = body.truncate_first.filter(|_| nth == 0) {
        send = &slice[..n.min(slice.len())];
    }
    if let Some(n) = body.stall_after {
        let _ = stream.write_all(&slice[..n.min(slice.len())]);
        let _ = stream.flush();
        std::thread::sleep(Duration::from_secs(30));
        return;
    }
    match body.slow {
        Some((chunk, pause)) => {
            for piece in send.chunks(chunk) {
                if stream.write_all(piece).is_err() {
                    return;
                }
                let _ = stream.flush();
                std::thread::sleep(pause);
            }
        }
        None => {
            let _ = stream.write_all(send);
        }
    }
}

pub fn sha256(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Deterministic bytes, so a resumed file can be compared with the original.
pub fn payload(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 31 % 251) as u8).collect()
}
