//! aria2c for one file, followed over JSON-RPC, which reports exact byte counts
//! (its console output rounds to MiB).

use crate::download::{Event, Phase, STALL, Transfer};
use crate::error::{Failure, Stop};
use crate::http::{self, Follow, Method};
use crate::native::aria2_control;
use crate::resume::{self, Validator};
use std::io::{BufRead, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How many lines of aria2's stderr to keep for a failure report.
const STDERR_LINES: usize = 100;

/// Stops aria2 on every path out; its control file keeps the progress.
struct Kill(Child);

impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn random_hex(bytes: usize) -> Result<String, Stop> {
    let mut buffer = vec![0u8; bytes];
    getrandom::fill(&mut buffer)
        .map_err(|error| Stop::Aria2Unavailable(format!("no random bytes: {error}")))?;
    Ok(buffer.iter().map(|b| format!("{b:02x}")).collect())
}

/// Keeps the end of a child's stderr, and keeps its pipe from filling.
fn drain(stderr: std::process::ChildStderr) -> Arc<Mutex<Vec<String>>> {
    let lines = Arc::new(Mutex::new(Vec::new()));
    let shared = lines.clone();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stderr).split(b'\n') {
            let Ok(line) = line else { return };
            let line = String::from_utf8_lossy(&line).trim_end().to_owned();
            if line.is_empty() {
                continue;
            }
            if let Ok(mut lines) = shared.lock() {
                lines.push(line);
                let overflow = lines.len().saturating_sub(STDERR_LINES);
                lines.drain(..overflow);
            }
        }
    });
    lines
}

/// The built-in client's rule, for a client that cannot send `If-Range`: a
/// partial file is continued only if a `HEAD` shows the same validator it was
/// started with, or if its hash will be checked. The current validator is kept
/// for the next run.
fn keep_or_discard(t: &Transfer) {
    let agent = http::agent(t.route, Duration::from_secs(30));
    let current = http::send(&agent, Method::Head, t.url, &[], None, Follow::All)
        .ok()
        .filter(|response| response.status().is_success())
        .and_then(|response| Validator::from_headers(t.url, response.headers()));
    let have = std::fs::metadata(t.part).map(|m| m.len()).unwrap_or(0);
    if have > 0 || aria2_control(t.part).exists() {
        let stored = Validator::load(t.part).filter(|stored| stored.url == t.url);
        let keep = match (&stored, &current) {
            (Some(stored), Some(current)) => stored.same_resource(current),
            _ => t.verified,
        };
        if !keep {
            resume::discard(t.part);
        } else if have > 0 {
            t.spliced.set(true);
        }
    }
    let kept = std::fs::metadata(t.part).map(|m| m.len()).unwrap_or(0);
    t.progress.event(Event::Resumed { bytes: kept });
    resume::record(t.part, current.as_ref());
}

pub(crate) fn run(program: &Path, t: &Transfer) -> Result<(), Stop> {
    let unavailable = |error: &dyn std::fmt::Display| Stop::Aria2Unavailable(error.to_string());
    let dir = t
        .part
        .parent()
        .ok_or_else(|| unavailable(&"no directory"))?;
    let out = t
        .part
        .file_name()
        .ok_or_else(|| unavailable(&"no file name"))?;
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .map_err(Failure::Io)?
        .port();
    let secret = random_hex(16)?;
    let gid = random_hex(8)?;
    // The secret stays out of the process list; the file also replaces any
    // user aria2.conf, whose options could change where bytes land.
    // So do the proxy and its credentials, routed as the built-in client routes them;
    // aria2's own reading of the environment is cleared below.
    let proxy = t
        .route
        .aria2_options(t.url)
        .map_err(|reason| unavailable(&reason))?;
    keep_or_discard(t);
    let mut conf = tempfile::NamedTempFile::new()?;
    writeln!(conf, "rpc-secret={secret}")?;
    for line in proxy {
        writeln!(conf, "{line}")?;
    }
    conf.flush()?;
    let mut command = Command::new(program);
    for name in crate::proxy::ARIA2_ENVIRONMENT {
        command.env_remove(name);
    }
    command
        .arg(format!("--conf-path={}", conf.path().display()))
        .arg("--quiet=true")
        .arg("--enable-rpc=true")
        .arg("--rpc-listen-all=false")
        .arg(format!("--rpc-listen-port={port}"))
        .arg(format!("--stop-with-process={}", std::process::id()))
        .arg(format!("--gid={gid}"))
        .arg(format!("--max-connection-per-server={}", t.connections))
        .arg(format!("--split={}", t.connections))
        .arg("--dir")
        .arg(dir)
        .arg("--out")
        .arg(out)
        .args([
            "--continue=true",
            "--allow-overwrite=true",
            "--auto-file-renaming=false",
            "--file-allocation=none",
            "--min-split-size=8M",
            "--connect-timeout=15",
            "--timeout=30",
            "--max-tries=5",
            "--retry-wait=3",
            "--max-file-not-found=2",
        ])
        .arg(t.url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = Kill(command.spawn().map_err(|error| unavailable(&error))?);
    let stderr = child.0.stderr.take().map(drain);
    let tail = || {
        stderr
            .as_ref()
            .and_then(|lines| lines.lock().ok().map(|lines| lines.join("\n")))
            .unwrap_or_default()
    };
    // aria2's RPC listens on loopback, which never goes through a proxy.
    let rpc = ureq::Agent::config_builder()
        .proxy(None)
        .max_redirects(0)
        .timeout_global(Some(Duration::from_secs(5)))
        .build()
        .new_agent();
    let endpoint = format!("http://127.0.0.1:{port}/jsonrpc");
    let call = |method: &str, params: serde_json::Value| -> Result<serde_json::Value, String> {
        let mut all = vec![serde_json::json!(format!("token:{secret}"))];
        if let serde_json::Value::Array(rest) = params {
            all.extend(rest);
        }
        let request =
            serde_json::json!({"jsonrpc":"2.0","id":"model-fetch","method":method,"params":all});
        let body = rpc
            .post(&endpoint)
            .content_type("application/json")
            .send(request.to_string())
            .map_err(|error| error.to_string())?
            .body_mut()
            .read_to_string()
            .map_err(|error| error.to_string())?;
        let response: serde_json::Value =
            serde_json::from_str(&body).map_err(|error| error.to_string())?;
        response
            .get("result")
            .cloned()
            .ok_or_else(|| response.to_string())
    };
    t.phase(Phase::Connect);
    let started = Instant::now();
    let mut answered = false;
    let mut downloading = false;
    let mut last = (0u64, Instant::now());
    let mut length = 0u64;
    loop {
        std::thread::sleep(Duration::from_millis(500));
        t.check()?;
        if let Ok(Some(status)) = child.0.try_wait() {
            return Err(if answered {
                Failure::Aria2Exited {
                    status: status.to_string(),
                    stderr: tail(),
                }
                .into()
            } else {
                unavailable(&format!("aria2c {status}: {}", tail()))
            });
        }
        let status = match call(
            "aria2.tellStatus",
            serde_json::json!([
                gid,
                [
                    "status",
                    "completedLength",
                    "totalLength",
                    "errorCode",
                    "errorMessage"
                ]
            ]),
        ) {
            Ok(status) => status,
            Err(_) if !answered && started.elapsed() < Duration::from_secs(10) => continue,
            Err(error) if !answered => return Err(unavailable(&error)),
            Err(error) => return Err(Failure::Aria2Rpc(error).into()),
        };
        answered = true;
        let completed: u64 = status["completedLength"]
            .as_str()
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        let total: u64 = status["totalLength"]
            .as_str()
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        if total > 0 && total != length {
            length = total;
            t.progress.length(total);
        }
        if completed != last.0 {
            last = (completed, Instant::now());
            if !downloading && completed > 0 {
                downloading = true;
                // aria2 counts what it resumed from the partial file in its first
                // report; the download phase starts from there, so those bytes are
                // not read as speed.
                t.progress.event(Event::Resumed { bytes: completed });
                t.phase(Phase::Download);
            }
        }
        t.progress.event(Event::Position { bytes: completed });
        match status["status"].as_str().unwrap_or_default() {
            "complete" => {
                let _ = call("aria2.shutdown", serde_json::json!([]));
                let deadline = Instant::now() + Duration::from_secs(5);
                while Instant::now() < deadline && !matches!(child.0.try_wait(), Ok(Some(_))) {
                    std::thread::sleep(Duration::from_millis(50));
                }
                return Ok(());
            }
            "error" | "removed" => {
                return Err(Failure::Aria2 {
                    code: status["errorCode"].as_str().unwrap_or("?").to_owned(),
                    message: status["errorMessage"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                }
                .into());
            }
            _ => {}
        }
        if last.1.elapsed() >= STALL {
            return Err(Failure::Stalled.into());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::Host;
    use crate::proxy::{DIRECT, Route};

    #[test]
    fn a_missing_program_is_unavailable_not_a_failed_transfer() {
        let root = tempfile::tempdir().unwrap();
        let part = root.path().join("file.part");
        let host = Host::Hub("http://127.0.0.1:1".into());
        let transfer = Transfer {
            url: "http://127.0.0.1:1/f",
            host: &host,
            part: &part,
            size: Some(5),
            bearer: None,
            verified: true,
            spliced: crate::download::test_support::spliced(),
            connections: 8,
            cancel: &(),
            progress: &(),
            route: &DIRECT,
        };
        assert!(matches!(
            run(&root.path().join("no-such-aria2c"), &transfer),
            Err(Stop::Aria2Unavailable(_))
        ));
    }

    #[test]
    fn the_control_file_sits_beside_the_part_file() {
        assert_eq!(
            aria2_control(Path::new("a/b.part")),
            Path::new("a/b.part.aria2")
        );
    }

    /// Only where aria2c is installed.
    #[test]
    fn a_real_aria2_downloads_over_rpc_and_reports_bytes() {
        use crate::download::Progress;
        use crate::download::test_support::serve;
        let program = Path::new("aria2c");
        if Command::new(program).arg("--version").output().is_err() {
            return;
        }
        #[derive(Default)]
        struct Last(Mutex<u64>);
        impl Progress for Last {
            fn event(&self, event: Event<'_>) {
                if let Event::Position { bytes } = event {
                    *self.0.lock().unwrap() = bytes;
                }
            }
        }
        // The HEAD that looks for a validator, then aria2's GET.
        let (base, _server) = serve(vec![
            "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\n".into(),
            "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\n12345".into(),
        ]);
        let root = tempfile::tempdir().unwrap();
        let part = root.path().join("file.part");
        let host = Host::Hub(base.clone());
        let last = Last::default();
        let url = format!("{base}/f");
        let transfer = Transfer {
            url: &url,
            host: &host,
            part: &part,
            size: Some(5),
            bearer: None,
            verified: true,
            spliced: crate::download::test_support::spliced(),
            connections: 8,
            cancel: &(),
            progress: &last,
            route: &DIRECT,
        };
        run(program, &transfer).unwrap();
        assert_eq!(std::fs::read(&part).unwrap(), b"12345");
        assert_eq!(*last.0.lock().unwrap(), 5);
    }

    /// Only where aria2c is installed: aria2 goes through the same proxy, as a plain
    /// HTTP proxy for an `http://` file.
    #[test]
    fn a_real_aria2_uses_the_proxy() {
        use crate::download::test_support::serve;
        let program = Path::new("aria2c");
        if Command::new(program).arg("--version").output().is_err() {
            return;
        }
        let (proxy, server) = serve(vec![
            "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\n".into(),
            "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\n12345".into(),
        ]);
        let route = Route::new(Some(ureq::Proxy::new(&proxy).unwrap()), None);
        let root = tempfile::tempdir().unwrap();
        let part = root.path().join("file.part");
        let host = Host::Hub("http://models.invalid".into());
        let transfer = Transfer {
            url: "http://models.invalid/r/resolve/main/f",
            host: &host,
            part: &part,
            size: Some(5),
            bearer: None,
            verified: true,
            spliced: crate::download::test_support::spliced(),
            connections: 8,
            cancel: &(),
            progress: &(),
            route: &route,
        };
        run(program, &transfer).unwrap();
        let heads = server.join().unwrap();
        // heads[0] is the built-in client's HEAD, tunnelled with CONNECT.
        assert!(heads[0].contains("models.invalid"), "{}", heads[0]);
        assert!(
            heads[1].starts_with("GET http://models.invalid/r/resolve/main/f "),
            "{}",
            heads[1]
        );
        assert_eq!(std::fs::read(&part).unwrap(), b"12345");
    }
}
