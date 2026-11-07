//! One download per destination: a second one at the same time fails at once,
//! on the general path and on `fetch` alike.

mod common;

use common::{Body, Server, payload, sha256};
use model_fetch::{Downloader, Error, Host, Remote, Target, part_path};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn lock_path(part: &Path) -> PathBuf {
    let mut name = part.as_os_str().to_owned();
    name.push(".lock");
    PathBuf::from(name)
}

fn wait_for(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn assert_busy(result: Result<(), Error>) {
    match result {
        Err(Error::Io(error)) => {
            assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
            assert!(error.to_string().contains("another download is writing"));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_second_download_to_the_same_file_fails_fast() {
    let data = payload(200_000);
    let server = Server::start();
    server.serve(
        "/slow",
        Body::bytes(&data).slow(16 * 1024, Duration::from_millis(20)),
    );
    let root = tempfile::tempdir().unwrap();
    let dest = root.path().join("f.bin");
    let target = Target::new([server.url("/slow")]).with_sha256(sha256(&data));
    std::thread::scope(|scope| {
        let first = scope.spawn(|| Downloader::default().download(&target, &dest, &(), &()));
        wait_for(&part_path(&dest));
        let started = Instant::now();
        assert_busy(Downloader::default().download(&target, &dest, &(), &()));
        assert!(started.elapsed() < Duration::from_secs(1));
        first.join().unwrap().unwrap();
    });
    assert_eq!(std::fs::read(&dest).unwrap(), data);
    assert!(!lock_path(&part_path(&dest)).exists());
    // Released: the next run is free to go.
    Downloader::default()
        .download(&target, &dest, &(), &())
        .unwrap();
}

#[test]
fn a_second_fetch_to_the_same_file_fails_fast() {
    let data = payload(200_000);
    let server = Server::start();
    server.serve(
        "/org/model/resolve/main/m.bin",
        Body::bytes(&data).slow(16 * 1024, Duration::from_millis(20)),
    );
    let hash = sha256(&data);
    let remote = Remote {
        repo: "org/model",
        file: "m.bin",
        size: data.len() as u64,
        sha256: &hash,
    };
    let root = tempfile::tempdir().unwrap();
    let dest = root.path().join("m.bin");
    let downloader = Downloader::new(vec![Host::Hub(server.base())], None);
    std::thread::scope(|scope| {
        let first = scope.spawn(|| downloader.fetch(&remote, &dest, &(), &()));
        wait_for(&dest.with_extension("part"));
        assert_busy(downloader.fetch(&remote, &dest, &(), &()));
        first.join().unwrap().unwrap();
    });
    assert_eq!(std::fs::read(&dest).unwrap(), data);
    assert!(!lock_path(&dest.with_extension("part")).exists());
}
