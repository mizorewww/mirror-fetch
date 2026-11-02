//! The built-in client: HTTP ranges, one connection.

use crate::download::{Event, Phase, STALL, Transfer};
use crate::error::{Failure, Stop, is_permanent_status};
use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::Duration;

/// Attempts on one host that may fail without progress.
const IDLE_ATTEMPTS: u32 = 3;

/// aria2's control file for `part`: its partial file is sparse, so while this exists
/// the length of `part` is not a contiguous prefix.
pub(crate) fn aria2_control(part: &std::path::Path) -> PathBuf {
    let mut name = part.as_os_str().to_owned();
    name.push(".aria2");
    PathBuf::from(name)
}

pub(crate) fn run(t: &Transfer) -> Result<(), Stop> {
    let control = aria2_control(t.part);
    if control.exists() {
        let _ = fs::remove_file(t.part);
        let _ = fs::remove_file(control);
    }
    // Hosts hand large files to a CDN, so downloads follow redirects.
    let agent = ureq::Agent::config_builder()
        .proxy(t.route.proxy())
        .max_redirects(5)
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_per_read(Some(STALL))
        .http_status_as_error(false)
        .build()
        .new_agent();
    let mut idle = 0;
    let mut phase = Phase::Connect;
    loop {
        t.check()?;
        let have = fs::metadata(t.part).map(|m| m.len()).unwrap_or(0);
        if have == t.remote.size {
            return Ok(());
        }
        if have > t.remote.size {
            fs::remove_file(t.part)?;
            continue;
        }
        t.progress.event(Event::Resumed { bytes: have });
        t.phase(phase);
        let result = attempt(&agent, t, have);
        let after = fs::metadata(t.part).map(|m| m.len()).unwrap_or(0);
        match result {
            Ok(()) => idle = 0,
            Err(Stop::Failed(failure)) => {
                // A rejected range only means this partial file no longer fits.
                let range_rejected =
                    have > 0 && matches!(failure, Failure::Status { code: 416, .. });
                let permanent =
                    matches!(failure, Failure::Status { code, .. } if is_permanent_status(code));
                if permanent && !range_rejected {
                    return Err(Stop::Failed(failure));
                }
                t.check()?;
                if after > have {
                    idle = 0;
                } else {
                    idle += 1;
                }
                if idle >= IDLE_ATTEMPTS {
                    return Err(Stop::Failed(failure));
                }
                t.progress.event(Event::Interrupted {
                    host: t.host,
                    received: after,
                    failure: &failure,
                });
                phase = Phase::Retry;
                sleep(t, Duration::from_secs(2 * u64::from(idle.max(1))))?;
            }
            Err(other) => return Err(other),
        }
    }
}

/// A wait that notices cancellation.
fn sleep(t: &Transfer, wait: Duration) -> Result<(), Stop> {
    let deadline = std::time::Instant::now() + wait;
    loop {
        t.check()?;
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return Ok(());
        }
        std::thread::sleep(left.min(Duration::from_millis(200)));
    }
}

/// One request from byte `have`, appending to the partial file.
fn attempt(agent: &ureq::Agent, t: &Transfer, have: u64) -> Result<(), Stop> {
    t.progress.event(Event::Request {
        url: t.url,
        from: have,
    });
    let mut request = agent.get(t.url);
    if have > 0 {
        request = request.header("Range", &format!("bytes={have}-"));
    }
    let mut response = request
        .call()
        .map_err(|error| Failure::Request(error.to_string()))?;
    let code = response.status().as_u16();
    if code >= 400 {
        // The server's error page or JSON usually names the cause.
        let body = response.body_mut().read_to_string().unwrap_or_default();
        let n = body.chars().count();
        let tail: String = body.chars().skip(n.saturating_sub(200)).collect();
        if code == 416 && have > 0 {
            fs::remove_file(t.part)?;
        }
        return Err(Failure::Status { code, body: tail }.into());
    }
    let mut done = if code == 206 { have } else { 0 };
    let mut out = if done > 0 {
        fs::OpenOptions::new().append(true).open(t.part)?
    } else {
        fs::File::create(t.part)?
    };
    t.phase(Phase::Download);
    t.progress.event(Event::Position { bytes: done });
    let mut reader = response.into_body().into_reader();
    let mut buffer = vec![0u8; 512 * 1024];
    let mut since_check = 0u64;
    loop {
        // Every 32 MB, so a cancel need not wait out a large file.
        if since_check >= 32 << 20 {
            t.check()?;
            since_check = 0;
        }
        let n = reader.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        out.write_all(&buffer[..n])?;
        done += n as u64;
        since_check += n as u64;
        t.progress.event(Event::Position { bytes: done });
    }
    out.sync_all()?;
    if done != t.remote.size {
        return Err(Failure::Incomplete {
            expected: t.remote.size,
            received: done,
        }
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::download::Remote;
    use crate::download::test_support::{serve, sha256};
    use crate::hub::Host;
    use crate::proxy::DIRECT;

    fn transfer<'a>(
        url: &'a str,
        host: &'a Host,
        part: &'a std::path::Path,
        remote: &'a Remote<'a>,
    ) -> Transfer<'a> {
        Transfer {
            url,
            host,
            part,
            remote,
            cancel: &(),
            progress: &(),
            route: &DIRECT,
        }
    }

    #[test]
    fn interrupted_downloads_resume_with_a_range_and_verify() {
        let body = b"abcdef";
        let hash = sha256(body);
        let remote = Remote {
            repo: "test/repo",
            file: "synthetic.bin",
            size: body.len() as u64,
            sha256: &hash,
        };
        let (base, server) = serve(vec![
            "HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nabc".into(),
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 3-5/6\r\nContent-Length: 3\r\nConnection: close\r\n\r\ndef".into(),
        ]);
        let url = format!("{base}/model");
        let root = tempfile::tempdir().unwrap();
        let part = root.path().join("synthetic.part");
        let host = Host::Hub("http://127.0.0.1".into());
        run(&transfer(&url, &host, &part, &remote)).unwrap();
        let requests = server.join().unwrap();
        assert!(!requests[0].to_ascii_lowercase().contains("range:"));
        assert!(requests[1].to_ascii_lowercase().contains("range: bytes=3-"));
        assert_eq!(fs::read(&part).unwrap(), body);
        assert!(crate::download::verify(&part, &remote, &(), &()).unwrap());
    }

    #[test]
    fn a_server_ignoring_the_range_restarts_the_file() {
        let hash = sha256(b"12345");
        let remote = Remote {
            repo: "test/repo",
            file: "synthetic.bin",
            size: 5,
            sha256: &hash,
        };
        let (base, server) = serve(vec![
            "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\n12345".into(),
        ]);
        let url = format!("{base}/model");
        let root = tempfile::tempdir().unwrap();
        let part = root.path().join("synthetic.part");
        fs::write(&part, b"xx").unwrap();
        let host = Host::Hub("http://127.0.0.1".into());
        run(&transfer(&url, &host, &part, &remote)).unwrap();
        server.join().unwrap();
        assert_eq!(fs::read(&part).unwrap(), b"12345");
    }

    #[test]
    fn missing_files_are_not_retried() {
        let hash = sha256(b"12345");
        let remote = Remote {
            repo: "test/repo",
            file: "synthetic.bin",
            size: 5,
            sha256: &hash,
        };
        let (base, server) = serve(vec![
            "HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\nConnection: close\r\n\r\nnot found"
                .into(),
        ]);
        let url = format!("{base}/model");
        let root = tempfile::tempdir().unwrap();
        let part = root.path().join("synthetic.part");
        let host = Host::Hub("http://127.0.0.1".into());
        match run(&transfer(&url, &host, &part, &remote)) {
            Err(Stop::Failed(failure)) => {
                assert!(failure.is_permanent());
                assert_eq!(failure.to_string(), "HTTP 404: not found");
            }
            other => panic!("{other:?}"),
        }
        server.join().unwrap();
    }

    #[test]
    fn a_rejected_range_starts_the_file_over() {
        let hash = sha256(b"12345");
        let remote = Remote {
            repo: "test/repo",
            file: "synthetic.bin",
            size: 5,
            sha256: &hash,
        };
        let (base, server) = serve(vec![
            "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .into(),
            "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\n12345".into(),
        ]);
        let url = format!("{base}/model");
        let root = tempfile::tempdir().unwrap();
        let part = root.path().join("synthetic.part");
        fs::write(&part, b"xx").unwrap();
        let host = Host::Hub("http://127.0.0.1".into());
        run(&transfer(&url, &host, &part, &remote)).unwrap();
        server.join().unwrap();
        assert_eq!(fs::read(&part).unwrap(), b"12345");
    }
}
