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
    // Hosts hand large files to a CDN, so downloads follow redirects; a token is
    // never sent on after one.
    let agent = ureq::Agent::config_builder()
        .proxy(t.route.proxy())
        .max_redirects(5)
        .redirect_auth_headers(ureq::config::RedirectAuthHeaders::Never)
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
        if let Some(size) = t.size {
            if have == size {
                return Ok(());
            }
            if have > size {
                fs::remove_file(t.part)?;
                continue;
            }
        }
        t.progress.event(Event::Resumed { bytes: have });
        t.phase(phase);
        let result = attempt(&agent, t, have);
        let after = fs::metadata(t.part).map(|m| m.len()).unwrap_or(0);
        match result {
            Ok(()) => return Ok(()),
            Err(Stop::Failed(failure)) => {
                let failure = redact(failure, t.bearer);
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

/// Takes the token out of any text a failure carries, in case a server echoed it.
fn redact(failure: Failure, bearer: Option<&str>) -> Failure {
    let Some(token) = bearer.filter(|token| !token.is_empty()) else {
        return failure;
    };
    let clean = |text: String| text.replace(token, "<redacted>");
    match failure {
        Failure::Request(message) => Failure::Request(clean(message)),
        Failure::Status { code, body } => Failure::Status {
            code,
            body: clean(body),
        },
        other => other,
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

/// `Content-Range: bytes 3-5/6` → `(Some(3), Some(6))`; `bytes */6` → `(None, Some(6))`.
fn content_range(response: &ureq::http::Response<ureq::Body>) -> (Option<u64>, Option<u64>) {
    let Some(value) = response
        .headers()
        .get("content-range")
        .and_then(|value| value.to_str().ok())
    else {
        return (None, None);
    };
    let Some((range, total)) = value
        .trim()
        .strip_prefix("bytes ")
        .and_then(|v| v.split_once('/'))
    else {
        return (None, None);
    };
    let start = range
        .split_once('-')
        .and_then(|(start, _)| start.trim().parse().ok());
    (start, total.trim().parse().ok())
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
    if let Some(token) = t.bearer {
        request = request.header("Authorization", &format!("Bearer {token}"));
    }
    let mut response = request
        .call()
        .map_err(|error| Failure::Request(error.to_string()))?;
    let code = response.status().as_u16();
    if code >= 400 {
        // Without an expected size, a range past the end may mean the file is whole.
        if code == 416 && have > 0 && t.size.is_none() && content_range(&response).1 == Some(have) {
            return Ok(());
        }
        // The server's error page or JSON usually names the cause.
        let body = response.body_mut().read_to_string().unwrap_or_default();
        let n = body.chars().count();
        let tail: String = body.chars().skip(n.saturating_sub(200)).collect();
        if code == 416 && have > 0 {
            fs::remove_file(t.part)?;
        }
        return Err(Failure::Status { code, body: tail }.into());
    }
    let resumed = code == 206;
    let (_, range_total) = content_range(&response);
    let announced = if resumed {
        range_total
    } else {
        response.body().content_length()
    };
    let total = t.size.or(announced);
    let mut done = if resumed { have } else { 0 };
    let mut out = if done > 0 {
        fs::OpenOptions::new().append(true).open(t.part)?
    } else {
        fs::File::create(t.part)?
    };
    t.phase(Phase::Download);
    if let Some(total) = total {
        t.progress.length(total);
    }
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
    match total {
        Some(expected) if done != expected => Err(Failure::Incomplete {
            expected,
            received: done,
        }
        .into()),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::download::Expect;
    use crate::download::test_support::{serve, sha256};
    use crate::hub::Host;
    use crate::proxy::DIRECT;

    fn transfer<'a>(
        url: &'a str,
        host: &'a Host,
        part: &'a std::path::Path,
        size: Option<u64>,
    ) -> Transfer<'a> {
        Transfer {
            url,
            host,
            part,
            size,
            bearer: None,
            connections: 1,
            cancel: &(),
            progress: &(),
            route: &DIRECT,
        }
    }

    #[test]
    fn interrupted_downloads_resume_with_a_range_and_verify() {
        let body = b"abcdef";
        let hash = sha256(body);
        let size = Some(body.len() as u64);
        let (base, server) = serve(vec![
            "HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nabc".into(),
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 3-5/6\r\nContent-Length: 3\r\nConnection: close\r\n\r\ndef".into(),
        ]);
        let url = format!("{base}/model");
        let root = tempfile::tempdir().unwrap();
        let part = root.path().join("synthetic.part");
        let host = Host::Hub("http://127.0.0.1".into());
        run(&transfer(&url, &host, &part, size)).unwrap();
        let requests = server.join().unwrap();
        assert!(!requests[0].to_ascii_lowercase().contains("range:"));
        assert!(requests[1].to_ascii_lowercase().contains("range: bytes=3-"));
        assert_eq!(fs::read(&part).unwrap(), body);
        let expect = Expect {
            size,
            sha256: Some(&hash),
        };
        assert!(crate::download::verify(&part, expect, &(), &()).unwrap());
    }

    #[test]
    fn a_server_ignoring_the_range_restarts_the_file() {
        let size = Some(5);
        let (base, server) = serve(vec![
            "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\n12345".into(),
        ]);
        let url = format!("{base}/model");
        let root = tempfile::tempdir().unwrap();
        let part = root.path().join("synthetic.part");
        fs::write(&part, b"xx").unwrap();
        let host = Host::Hub("http://127.0.0.1".into());
        run(&transfer(&url, &host, &part, size)).unwrap();
        server.join().unwrap();
        assert_eq!(fs::read(&part).unwrap(), b"12345");
    }

    #[test]
    fn missing_files_are_not_retried() {
        let size = Some(5);
        let (base, server) = serve(vec![
            "HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\nConnection: close\r\n\r\nnot found"
                .into(),
        ]);
        let url = format!("{base}/model");
        let root = tempfile::tempdir().unwrap();
        let part = root.path().join("synthetic.part");
        let host = Host::Hub("http://127.0.0.1".into());
        match run(&transfer(&url, &host, &part, size)) {
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
        let size = Some(5);
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
        run(&transfer(&url, &host, &part, size)).unwrap();
        server.join().unwrap();
        assert_eq!(fs::read(&part).unwrap(), b"12345");
    }
    #[test]
    fn without_an_expected_size_the_server_length_decides() {
        let (base, server) = serve(vec![
            "HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nabc".into(),
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 3-5/6\r\nContent-Length: 3\r\nConnection: close\r\n\r\ndef".into(),
        ]);
        let url = format!("{base}/model");
        let root = tempfile::tempdir().unwrap();
        let part = root.path().join("synthetic.part");
        let host = Host::Url(url.clone());
        run(&transfer(&url, &host, &part, None)).unwrap();
        let requests = server.join().unwrap();
        assert!(requests[1].to_ascii_lowercase().contains("range: bytes=3-"));
        assert_eq!(fs::read(&part).unwrap(), b"abcdef");
    }

    /// A partial file that is already whole answers its range with 416 and the
    /// full length: nothing is left to fetch.
    #[test]
    fn a_whole_partial_file_is_recognised_without_a_size() {
        let (base, server) = serve(vec![
            "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */5\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
        ]);
        let url = format!("{base}/model");
        let root = tempfile::tempdir().unwrap();
        let part = root.path().join("synthetic.part");
        fs::write(&part, b"12345").unwrap();
        let host = Host::Url(url.clone());
        run(&transfer(&url, &host, &part, None)).unwrap();
        server.join().unwrap();
        assert_eq!(fs::read(&part).unwrap(), b"12345");
    }

    /// The token goes to the mirror, not on to where it redirects, and never
    /// into a failure's text.
    #[test]
    fn a_bearer_token_stays_with_its_host() {
        let (cdn, cdn_server) = serve(vec![
            "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\n12345".into(),
        ]);
        let (base, server) = serve(vec![format!(
            "HTTP/1.1 302 Found\r\nLocation: {cdn}/signed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )]);
        let url = format!("{base}/model");
        let root = tempfile::tempdir().unwrap();
        let part = root.path().join("synthetic.part");
        let host = Host::Url(url.clone());
        let mut t = transfer(&url, &host, &part, Some(5));
        t.bearer = Some("hf_secret");
        run(&t).unwrap();
        let first = server.join().unwrap();
        let second = cdn_server.join().unwrap();
        assert!(first[0].contains("Bearer hf_secret"), "{}", first[0]);
        assert!(!second[0].to_ascii_lowercase().contains("authorization"));
        assert_eq!(fs::read(&part).unwrap(), b"12345");

        let echoed = redact(
            Failure::Status {
                code: 401,
                body: "bad token hf_secret".into(),
            },
            Some("hf_secret"),
        );
        assert_eq!(echoed.to_string(), "HTTP 401: bad token <redacted>");
    }
}
