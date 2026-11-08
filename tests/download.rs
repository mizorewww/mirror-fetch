//! The general core against a loopback server: resume, mirror fallback,
//! verification and cancellation.

mod common;

use common::{Body, Server, payload, sha256};
use mirror_fetch::{Downloader, Error, Event, Failure, Mirror, Progress, Target, part_path};
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

#[derive(Default)]
struct Log {
    events: Mutex<Vec<String>>,
    length: Mutex<Option<u64>>,
}

impl Progress for Log {
    fn event(&self, event: Event<'_>) {
        if !matches!(event, Event::Position { .. }) {
            self.events.lock().unwrap().push(format!("{event:?}"));
        }
    }

    fn length(&self, bytes: u64) {
        *self.length.lock().unwrap() = Some(bytes);
    }
}

impl Log {
    fn has(&self, needle: &str) -> bool {
        self.events
            .lock()
            .unwrap()
            .iter()
            .any(|e| e.contains(needle))
    }
}

fn aria2() -> Option<PathBuf> {
    std::process::Command::new("aria2c")
        .arg("--version")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|_| PathBuf::from("aria2c"))
}

#[test]
fn an_interrupted_transfer_resumes_with_a_range() {
    let data = payload(200_000);
    let server = Server::start();
    server.serve("/file.bin", Body::bytes(&data).truncated_once(70_000));
    let root = tempfile::tempdir().unwrap();
    let dest = root.path().join("out/file.bin");
    let target = Target::new([server.url("/file.bin")]).with_sha256(sha256(&data));
    let log = Log::default();
    Downloader::default()
        .download(&target, &dest, &(), &log)
        .unwrap();
    assert_eq!(fs::read(&dest).unwrap(), data);
    let requests = server.requests();
    assert_eq!(requests.len(), 2, "{requests:?}");
    assert_eq!(requests[0].range, None);
    assert_eq!(requests[1].range, Some(70_000));
    assert!(log.has("Interrupted"));
    assert_eq!(*log.length.lock().unwrap(), Some(200_000));
    assert!(!part_path(&dest).exists());
}

#[test]
fn a_failing_mirror_falls_through_to_the_next() {
    let data = payload(10_000);
    let server = Server::start();
    server.serve("/gone", Body::status(404));
    server.serve("/ok", Body::bytes(&data));
    let root = tempfile::tempdir().unwrap();
    let dest = root.path().join("file.bin");
    let target = Target::new([server.url("/gone"), server.url("/ok")]).with_size(10_000);
    let log = Log::default();
    Downloader::default()
        .download(&target, &dest, &(), &log)
        .unwrap();
    assert_eq!(fs::read(&dest).unwrap(), data);
    assert!(log.has("HostFailed"));
    assert!(
        log.events
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .contains("Completed")
    );
}

/// The first mirror serves the right length with the wrong bytes: they are
/// discarded, not resumed from, and the second mirror's copy is kept.
#[test]
fn a_mirror_with_the_wrong_bytes_falls_through_to_the_next() {
    let data = payload(10_000);
    let mut wrong = data.clone();
    wrong[5_000] ^= 1;
    let server = Server::start();
    server.serve("/bad", Body::bytes(&wrong));
    server.serve("/good", Body::bytes(&data));
    let root = tempfile::tempdir().unwrap();
    let dest = root.path().join("file.bin");
    let target = Target::new([server.url("/bad"), server.url("/good")]).with_sha256(sha256(&data));
    let log = Log::default();
    Downloader::default()
        .download(&target, &dest, &(), &log)
        .unwrap();
    assert_eq!(fs::read(&dest).unwrap(), data);
    assert!(log.has("ChecksumMismatch"));
    let good: Vec<_> = server
        .requests()
        .into_iter()
        .filter(|r| r.path == "/good")
        .collect();
    assert_eq!(good[0].range, None, "nothing of the bad copy is kept");
}

#[test]
fn a_sha256_mismatch_everywhere_fails_and_leaves_nothing() {
    let server = Server::start();
    server.serve("/a", Body::bytes(b"hello"));
    let root = tempfile::tempdir().unwrap();
    let dest = root.path().join("file.bin");
    let target = Target::new([server.url("/a")]).with_sha256(sha256(b"other"));
    let error = Downloader::default()
        .download(&target, &dest, &(), &())
        .unwrap_err();
    match &error {
        Error::AllHostsFailed(failures) => {
            assert_eq!(failures.len(), 1);
            assert_eq!(failures[0].host, "127.0.0.1");
            assert!(matches!(failures[0].failure, Failure::ChecksumMismatch));
        }
        other => panic!("{other:?}"),
    }
    assert!(!dest.exists());
    assert!(!part_path(&dest).exists());
}

/// Cancelling keeps what arrived in `.part`; the next run asks only for the rest.
#[test]
fn a_cancelled_download_keeps_its_part_and_resumes() {
    struct CancelAfter<'a>(&'a AtomicBool, u64);
    impl Progress for CancelAfter<'_> {
        fn event(&self, event: Event<'_>) {
            if let Event::Position { bytes } = event
                && bytes >= self.1
            {
                self.0.store(true, Ordering::Relaxed);
            }
        }
    }
    let data = payload(256 * 1024);
    let server = Server::start();
    server.serve(
        "/slow",
        Body::bytes(&data).slow(4 * 1024, Duration::from_millis(20)),
    );
    let root = tempfile::tempdir().unwrap();
    let dest = root.path().join("file.bin");
    let target = Target::new([server.url("/slow")]).with_sha256(sha256(&data));
    let cancel = AtomicBool::new(false);
    let error = Downloader::default()
        .download(&target, &dest, &cancel, &CancelAfter(&cancel, 8 * 1024))
        .unwrap_err();
    assert!(matches!(error, Error::Cancelled), "{error:?}");
    assert!(!dest.exists());
    let kept = fs::metadata(part_path(&dest)).unwrap().len();
    assert!(kept >= 8 * 1024 && kept < data.len() as u64, "{kept}");

    server.serve("/slow", Body::bytes(&data));
    Downloader::default()
        .download(&target, &dest, &(), &())
        .unwrap();
    assert_eq!(fs::read(&dest).unwrap(), data);
    assert_eq!(server.requests().last().unwrap().range, Some(kept));
}

/// Neither size nor hash, nor a length from the server: the connection's end is
/// the file's end.
#[test]
fn a_file_of_unknown_length_downloads_whole() {
    let data = payload(30_000);
    let server = Server::start();
    server.serve("/stream", Body::bytes(&data).without_length());
    let root = tempfile::tempdir().unwrap();
    let dest = root.path().join("stream.bin");
    let log = Log::default();
    Downloader::default()
        .download(&Target::new([server.url("/stream")]), &dest, &(), &log)
        .unwrap();
    assert_eq!(fs::read(&dest).unwrap(), data);
    assert_eq!(*log.length.lock().unwrap(), None);
    assert!(!log.has("Verify"));
}

#[test]
fn a_bearer_mirror_sends_its_token_and_others_do_not() {
    let server = Server::start();
    server.serve("/private", Body::status(401));
    server.serve("/public", Body::bytes(b"hello"));
    let root = tempfile::tempdir().unwrap();
    let target = Target {
        mirrors: vec![
            Mirror::new(server.url("/private")).with_bearer("tok_123"),
            Mirror::new(server.url("/public")),
        ],
        size: Some(5),
        ..Target::default()
    };
    Downloader::default()
        .download(&target, &root.path().join("f"), &(), &())
        .unwrap();
    let requests = server.requests();
    assert_eq!(requests[0].authorization.as_deref(), Some("Bearer tok_123"));
    assert_eq!(requests[1].authorization, None);
}

/// Only where aria2c is installed: aria2 resumes the built-in client's partial
/// file and falls through mirrors like it.
#[test]
fn aria2_downloads_through_the_same_mirrors() {
    let Some(program) = aria2() else {
        eprintln!("aria2c not on PATH; skipped");
        return;
    };
    let data = payload(300_000);
    let server = Server::start();
    server.serve("/gone", Body::status(404));
    server.serve("/ok", Body::bytes(&data));
    let root = tempfile::tempdir().unwrap();
    let dest = root.path().join("file.bin");
    fs::write(part_path(&dest), &data[..100_000]).unwrap();
    let target = Target::new([server.url("/gone"), server.url("/ok")])
        .with_size(data.len() as u64)
        .with_sha256(sha256(&data));
    let log = Log::default();
    Downloader::default()
        .with_aria2(Some(program))
        .with_connections(4)
        .download(&target, &dest, &(), &log)
        .unwrap_or_else(|error| panic!("{error:?} {:?}", server.requests()));
    assert_eq!(fs::read(&dest).unwrap(), data);
    assert!(!log.has("Aria2Unavailable"), "{:?}", log.events.lock());
    assert!(log.has("HostFailed"));
    assert_eq!(*log.length.lock().unwrap(), Some(300_000));
    // aria2 probes the length first, then continues from a piece boundary of
    // what the built-in client left.
    assert!(
        server
            .requests()
            .iter()
            .any(|r| r.path == "/ok" && r.range.is_some_and(|from| from > 0)),
        "{:?}",
        server.requests()
    );
}

/// The HEAD that looks for a validator before aria2 runs stops with the
/// download: a server that never answers it cannot hold a cancel up. (The
/// program need not exist: the HEAD comes first.)
#[test]
fn cancelling_during_the_validator_head_returns_promptly() {
    let server = Server::start();
    server.serve("/f", Body::bytes(b"data").with_silent_head());
    let root = tempfile::tempdir().unwrap();
    let cancel = AtomicBool::new(false);
    let started = std::time::Instant::now();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(300));
            cancel.store(true, Ordering::Relaxed);
        });
        let error = Downloader::default()
            .with_aria2(Some(root.path().join("no-such-aria2c")))
            .download(
                &Target::new([server.url("/f")]),
                &root.path().join("f"),
                &cancel,
                &(),
            )
            .unwrap_err();
        assert!(matches!(error, Error::Cancelled), "{error:?}");
    });
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
}
