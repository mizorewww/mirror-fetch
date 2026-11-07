//! When a partial file may be continued, against a loopback server: never across
//! mirrors without a hash, across runs only with a validator, and a spliced file
//! that fails its hash is fetched again from byte 0.

mod common;

use common::{Body, Server, payload, sha256};
use model_fetch::{Downloader, Event, Progress, Target, part_path};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const N: usize = 200_000;

fn other(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 7 + 3) as u8).collect()
}

fn ranges(server: &Server, path: &str) -> Vec<Option<u64>> {
    server
        .requests()
        .into_iter()
        .filter(|r| r.path == path && r.method == "GET")
        .map(|r| r.range)
        .collect()
}

/// Stops the download once this many bytes are on disk.
struct CancelAt<'a>(&'a AtomicBool, u64);

impl Progress for CancelAt<'_> {
    fn event(&self, event: Event<'_>) {
        if let Event::Position { bytes } = event
            && bytes >= self.1
        {
            self.0.store(true, Ordering::Relaxed);
        }
    }
}

/// Mirror one cuts its file in half and then refuses ranges; mirror two holds
/// other bytes of the same length. With no hash, mirror two starts from byte 0.
#[test]
fn without_a_hash_two_mirrors_are_never_spliced() {
    let a = payload(N);
    let b = other(N);
    let server = Server::start();
    server.serve(
        "/a",
        Body::bytes(&a).truncated_once(N / 2).refusing_ranges(404),
    );
    server.serve("/b", Body::bytes(&b));
    let root = tempfile::tempdir().unwrap();
    let dest = root.path().join("f.bin");
    let target = Target::new([server.url("/a"), server.url("/b")]).with_size(N as u64);
    Downloader::default()
        .download(&target, &dest, &(), &())
        .unwrap();
    assert_eq!(fs::read(&dest).unwrap(), b);
    assert_eq!(ranges(&server, "/b"), [None]);
}

/// A partial file left by an earlier run with no validator beside it is not
/// trusted without a hash.
#[test]
fn an_earlier_partial_without_a_validator_is_not_resumed() {
    let a = payload(N);
    let server = Server::start();
    server.serve("/a", Body::bytes(&a));
    let root = tempfile::tempdir().unwrap();
    let dest = root.path().join("f.bin");
    fs::write(part_path(&dest), other(1_000)).unwrap();
    Downloader::default()
        .download(&Target::new([server.url("/a")]), &dest, &(), &())
        .unwrap();
    assert_eq!(fs::read(&dest).unwrap(), a);
    assert_eq!(ranges(&server, "/a"), [None]);
}

/// With the ETag kept beside the partial file, the next run continues with
/// If-Range; if the file changed meanwhile, the server sends it whole.
#[test]
fn a_validator_resumes_across_runs_and_a_changed_file_restarts() {
    let a = payload(N);
    let b = other(N);
    for changed in [false, true] {
        let server = Server::start();
        server.serve(
            "/v",
            Body::bytes(&a)
                .with_etag("\"a\"")
                .slow(8 * 1024, Duration::from_millis(20)),
        );
        let root = tempfile::tempdir().unwrap();
        let dest = root.path().join("f.bin");
        let target = Target::new([server.url("/v")]);
        let cancel = AtomicBool::new(false);
        Downloader::default()
            .download(&target, &dest, &cancel, &CancelAt(&cancel, 16 * 1024))
            .unwrap_err();
        let kept = fs::metadata(part_path(&dest)).unwrap().len();
        assert!(kept > 0 && kept < N as u64);

        let (now, etag) = if changed {
            (&b, "\"b\"")
        } else {
            (&a, "\"a\"")
        };
        server.serve("/v", Body::bytes(now).with_etag(etag));
        Downloader::default()
            .download(&target, &dest, &(), &())
            .unwrap();
        assert_eq!(&fs::read(&dest).unwrap(), now, "changed: {changed}");
        let last = server.requests().pop().unwrap();
        assert_eq!(last.range, Some(kept));
        assert_eq!(last.if_range.as_deref(), Some("\"a\""));
        let left: Vec<PathBuf> = fs::read_dir(root.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(
            left,
            std::slice::from_ref(&dest),
            "nothing is left beside the file"
        );
    }
}

/// A server that ignores If-Range sends a range of its new version: its ETag
/// differs from the one kept, so the partial file is started over, not joined.
#[test]
fn a_range_of_another_version_is_not_joined() {
    let a = payload(N);
    let b = other(N);
    let server = Server::start();
    server.serve(
        "/v",
        Body::bytes(&a)
            .with_etag("\"a\"")
            .slow(8 * 1024, Duration::from_millis(20)),
    );
    let root = tempfile::tempdir().unwrap();
    let dest = root.path().join("f.bin");
    let target = Target::new([server.url("/v")]);
    let cancel = AtomicBool::new(false);
    Downloader::default()
        .download(&target, &dest, &cancel, &CancelAt(&cancel, 16 * 1024))
        .unwrap_err();
    let kept = fs::metadata(part_path(&dest)).unwrap().len();

    server.serve("/v", Body::bytes(&b).with_etag("\"b\"").ignoring_if_range());
    Downloader::default()
        .download(&target, &dest, &(), &())
        .unwrap();
    assert_eq!(fs::read(&dest).unwrap(), b);
    let tail: Vec<_> = server.requests().into_iter().rev().take(2).collect();
    assert_eq!(tail[1].range, Some(kept), "first the range was asked for");
    assert_eq!(tail[0].range, None, "then the file from the start");
}

/// With a hash, an earlier partial file is continued; when the joined file fails
/// the hash, the same mirror is asked once more from byte 0.
#[test]
fn with_a_hash_a_spliced_mismatch_starts_the_mirror_over() {
    let a = payload(N);
    let server = Server::start();
    server.serve("/a", Body::bytes(&a));
    let root = tempfile::tempdir().unwrap();
    let dest = root.path().join("f.bin");
    fs::write(part_path(&dest), other(1_000)).unwrap();
    let target = Target::new([server.url("/a")]).with_sha256(sha256(&a));
    Downloader::default()
        .download(&target, &dest, &(), &())
        .unwrap();
    assert_eq!(fs::read(&dest).unwrap(), a);
    assert_eq!(ranges(&server, "/a"), [Some(1_000), None]);
}

/// Only where aria2c is installed: aria2 follows the same rule.
#[test]
fn aria2_does_not_continue_an_unproven_partial_without_a_hash() {
    if std::process::Command::new("aria2c")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("aria2c not on PATH; skipped");
        return;
    }
    let a = payload(N);
    let server = Server::start();
    server.serve("/gone", Body::status(404));
    server.serve("/a", Body::bytes(&a));
    let root = tempfile::tempdir().unwrap();
    let dest = root.path().join("f.bin");
    fs::write(part_path(&dest), other(50_000)).unwrap();
    Downloader::default()
        .with_aria2(Some("aria2c".into()))
        .download(
            &Target::new([server.url("/gone"), server.url("/a")]),
            &dest,
            &(),
            &(),
        )
        .unwrap();
    assert_eq!(fs::read(&dest).unwrap(), a);
}
