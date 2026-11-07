//! The `model-fetch` program end to end, against a loopback server.

#![cfg(feature = "cli")]

mod common;

use common::{Body, Server, payload, sha256};
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

fn program() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_model-fetch"));
    for name in [
        "ALL_PROXY",
        "HTTPS_PROXY",
        "HTTP_PROXY",
        "all_proxy",
        "https_proxy",
        "http_proxy",
        "NO_PROXY",
        "no_proxy",
        "HF_ENDPOINT",
        "HF_TOKEN",
    ] {
        command.env_remove(name);
    }
    command.stdin(Stdio::null());
    command
}

fn run(command: &mut Command) -> (i32, String, String) {
    let Output {
        status,
        stdout,
        stderr,
    } = command.output().unwrap();
    (
        status.code().unwrap_or(-1),
        String::from_utf8_lossy(&stdout).into_owned(),
        String::from_utf8_lossy(&stderr).into_owned(),
    )
}

fn arg(path: &Path) -> &str {
    path.to_str().unwrap()
}

#[test]
fn get_downloads_verifies_and_then_skips_a_complete_file() {
    let data = payload(100_000);
    let hash = sha256(&data);
    let server = Server::start();
    server.serve("/files/model.bin", Body::bytes(&data));
    let root = tempfile::tempdir().unwrap();
    let (code, stdout, stderr) = run(program().current_dir(root.path()).args([
        "get",
        &server.url("/files/model.bin"),
        "--sha256",
        &hash,
    ]));
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(stdout, "");
    assert!(
        stderr.contains("saved model.bin (97.7 KiB, sha256 verified)"),
        "{stderr}"
    );
    assert!(!stderr.contains('\r'), "no redrawn line without a terminal");
    assert_eq!(std::fs::read(root.path().join("model.bin")).unwrap(), data);

    let (code, _, stderr) = run(program().current_dir(root.path()).args([
        "get",
        &server.url("/files/model.bin"),
        "--sha256",
        &hash,
    ]));
    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.contains("already complete"), "{stderr}");
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn get_resumes_and_falls_back_to_a_mirror() {
    let data = payload(150_000);
    let server = Server::start();
    server.serve("/gone", Body::status(404));
    server.serve("/cut", Body::bytes(&data).truncated_once(60_000));
    let root = tempfile::tempdir().unwrap();
    let dest = root.path().join("sub/out.bin");
    let (code, _, stderr) = run(program().args([
        "get",
        &server.url("/gone"),
        "--mirror",
        &server.url("/cut"),
        "-o",
        arg(&dest),
        "--size",
        "150000",
    ]));
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stderr.contains("127.0.0.1 failed (HTTP 404: status 404); trying the next mirror"),
        "{stderr}"
    );
    assert!(stderr.contains("interrupted at 58.6 KiB"), "{stderr}");
    assert_eq!(std::fs::read(&dest).unwrap(), data);
    let cut: Vec<_> = server
        .requests()
        .into_iter()
        .filter(|r| r.path == "/cut")
        .map(|r| r.range)
        .collect();
    assert_eq!(cut, [None, Some(60_000)]);
}

#[test]
fn get_skips_a_mirror_with_the_wrong_bytes_and_fails_when_none_match() {
    let data = payload(5_000);
    let server = Server::start();
    server.serve("/wrong", Body::bytes(&payload(4_999)));
    server.serve("/right", Body::bytes(&data));
    let root = tempfile::tempdir().unwrap();
    let dest = root.path().join("f.bin");
    let (code, _, stderr) = run(program().args([
        "get",
        &server.url("/wrong"),
        "--mirror",
        &server.url("/right"),
        "-o",
        arg(&dest),
        "--sha256",
        &sha256(&data),
    ]));
    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.contains("sha256 mismatch"), "{stderr}");
    assert_eq!(std::fs::read(&dest).unwrap(), data);

    let other = root.path().join("g.bin");
    let (code, _, stderr) = run(program().args([
        "get",
        &server.url("/wrong"),
        "-o",
        arg(&other),
        "--sha256",
        &sha256(&data),
    ]));
    assert_eq!(code, 1, "{stderr}");
    assert!(
        stderr.contains(
            "model-fetch: error: could not download g.bin\n  127.0.0.1: checksum mismatch"
        ),
        "{stderr}"
    );
    assert!(!other.exists());
    assert!(!root.path().join("g.bin.part").exists());
}

#[test]
fn usage_errors_exit_2_and_help_exits_0() {
    let (code, _, stderr) = run(program().args(["get", "https://e.example/f", "--sha256", "xyz"]));
    assert_eq!(code, 2, "{stderr}");
    let (code, _, stderr) = run(program().args(["get", "https://e.example/"]));
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("pass -o FILE"), "{stderr}");
    let (code, _, _) = run(program().args(["hf", "org/name", "../escape"]));
    assert_eq!(code, 2);
    let (code, _, _) = run(program().args(["frobnicate"]));
    assert_eq!(code, 2);
    let (code, stdout, _) = run(program().args(["--help"]));
    assert_eq!(code, 0);
    assert!(stdout.contains("Exit status"));
}

/// "trying the next mirror" only when there is one; a stale partial file that
/// cannot be trusted is not announced as resumed.
#[test]
fn messages_say_only_what_happens() {
    let server = Server::start();
    server.serve("/a", Body::status(404));
    server.serve("/b", Body::status(404));
    let root = tempfile::tempdir().unwrap();
    let (code, _, stderr) = run(program().current_dir(root.path()).args([
        "get",
        &server.url("/a"),
        "--mirror",
        &server.url("/b"),
        "-o",
        "f",
    ]));
    assert_eq!(code, 1, "{stderr}");
    assert_eq!(
        stderr.matches("trying the next mirror").count(),
        1,
        "{stderr}"
    );
    assert!(stderr.contains("  127.0.0.1: HTTP 404"), "{stderr}");

    server.serve("/c", Body::bytes(b"fresh"));
    std::fs::write(root.path().join("c.part"), b"stale").unwrap();
    let (code, _, stderr) =
        run(program()
            .current_dir(root.path())
            .args(["get", &server.url("/c"), "-o", "c"]));
    assert_eq!(code, 0, "{stderr}");
    assert!(!stderr.contains("resuming"), "{stderr}");
    assert!(stderr.contains("saved c (5 B, unverified)"), "{stderr}");
    assert_eq!(std::fs::read(root.path().join("c")).unwrap(), b"fresh");
}

/// With stdout and stderr closed at the far end, writes fail with EPIPE; the
/// program carries on and exits by its own result instead of panicking.
#[test]
fn a_closed_stdout_or_stderr_is_not_a_panic() {
    let server = Server::start();
    server.serve("/f", Body::bytes(b"data"));
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("f");
    for args in [
        vec![
            "get".to_owned(),
            server.url("/f"),
            "-o".into(),
            arg(&file).into(),
        ],
        vec![
            "verify".to_owned(),
            arg(&file).into(),
            "--sha256".into(),
            sha256(b"data"),
        ],
        vec!["--help".to_owned()],
    ] {
        let (reader, writer) = std::io::pipe().unwrap();
        drop(reader);
        let status = program()
            .args(&args)
            .stdout(writer.try_clone().unwrap())
            .stderr(writer)
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(0), "{args:?}");
    }
}

/// Every proxy variable names a dead proxy; loopback goes direct anyway.
#[test]
fn loopback_ignores_the_environment_proxy() {
    let server = Server::start();
    server.serve("/f.txt", Body::bytes(b"direct"));
    let root = tempfile::tempdir().unwrap();
    let (code, _, stderr) = run(program()
        .current_dir(root.path())
        .env("ALL_PROXY", "http://127.0.0.1:1")
        .env("HTTP_PROXY", "http://127.0.0.1:1")
        .env("HTTPS_PROXY", "http://127.0.0.1:1")
        .args(["get", &server.url("/f.txt")]));
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(std::fs::read(root.path().join("f.txt")).unwrap(), b"direct");
    assert!(server.requests().iter().all(|r| !r.via_proxy));
}

/// Two loopback hubs: the first lacks the revision. HF_TOKEN is set but neither
/// host is Hugging Face, so neither sees it, and it is never printed.
#[test]
fn hf_downloads_files_across_endpoints_keeping_their_paths() {
    let first = Server::start();
    let second = Server::start();
    second.serve("/org/model/resolve/v1/config.json", Body::bytes(b"{}"));
    second.serve(
        "/org/model/resolve/v1/onnx/model.onnx",
        Body::bytes(b"onnx"),
    );
    let root = tempfile::tempdir().unwrap();
    let (code, _, stderr) = run(program().env("HF_TOKEN", "hf_secret_value").args([
        "hf",
        "org/model",
        "config.json",
        "onnx/model.onnx",
        "--revision",
        "v1",
        "--endpoint",
        &first.base(),
        "--endpoint",
        &format!("{}/", second.base()),
        "-o",
        arg(root.path()),
    ]));
    assert_eq!(code, 0, "{stderr}");
    assert!(!stderr.contains("hf_secret_value"));
    assert_eq!(
        std::fs::read(root.path().join("config.json")).unwrap(),
        b"{}"
    );
    assert_eq!(
        std::fs::read(root.path().join("onnx/model.onnx")).unwrap(),
        b"onnx"
    );
    for request in first.requests().iter().chain(&second.requests()) {
        assert_eq!(request.authorization, None, "{request:?}");
    }
    // A HEAD for the metadata and a GET, for each file.
    assert_eq!(first.requests().len(), 4);
}

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

/// The Hub's size and hash are checked; the files come from the commit the
/// revision named; a second run finds them complete and downloads nothing.
#[test]
fn hf_verifies_against_the_hub_and_skips_complete_files() {
    let weights = payload(80_000);
    let config = b"{}\n".to_vec();
    let cdn = Server::start();
    cdn.serve("/blob", Body::bytes(&weights));
    let hub = Server::start();
    for revision in ["main", COMMIT] {
        hub.serve(
            &format!("/org/model/resolve/{revision}/w.bin"),
            Body::redirect(cdn.url("/blob"))
                .with_header("X-Linked-Size", &weights.len().to_string())
                .with_header("X-Linked-Etag", &format!("\"{}\"", sha256(&weights)))
                .with_header("X-Repo-Commit", COMMIT),
        );
        hub.serve(
            &format!("/org/model/resolve/{revision}/config.json"),
            Body::bytes(&config)
                .with_etag(&format!("\"{}\"", common::git_sha1(&config)))
                .with_header("X-Repo-Commit", COMMIT),
        );
    }
    let root = tempfile::tempdir().unwrap();
    let args = [
        "hf",
        "org/model",
        "w.bin",
        "config.json",
        "--endpoint",
        &hub.base(),
        "-o",
        arg(root.path()),
    ];
    let (code, _, stderr) = run(program().args(args));
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stderr.contains("w.bin (78.1 KiB, sha256 verified)"),
        "{stderr}"
    );
    assert!(
        stderr.contains("config.json (3 B, git hash verified)"),
        "{stderr}"
    );
    let gets: Vec<String> = hub
        .requests()
        .into_iter()
        .filter(|r| r.method == "GET")
        .map(|r| r.path)
        .collect();
    assert!(gets.iter().all(|path| path.contains(COMMIT)), "{gets:?}");

    let gets = |server: &Server| {
        server
            .requests()
            .iter()
            .filter(|r| r.method == "GET")
            .count()
    };
    let before = (gets(&hub), gets(&cdn));
    let (code, _, stderr) = run(program().args(args));
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(stderr.matches("already complete").count(), 2, "{stderr}");
    assert_eq!(
        (gets(&hub), gets(&cdn)),
        before,
        "only HEADs the second time"
    );
}

/// The bytes do not match the Hub's hash: nothing is saved.
#[test]
fn hf_rejects_bytes_that_differ_from_the_hub() {
    let hub = Server::start();
    hub.serve(
        "/org/model/resolve/main/w.bin",
        Body::bytes(b"tampered")
            .with_header("X-Linked-Etag", &format!("\"{}\"", sha256(b"original"))),
    );
    let root = tempfile::tempdir().unwrap();
    let (code, _, stderr) = run(program().args([
        "hf",
        "org/model",
        "w.bin",
        "--endpoint",
        &hub.base(),
        "-o",
        arg(root.path()),
    ]));
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("checksum mismatch"), "{stderr}");
    assert!(!root.path().join("w.bin").exists());
}

/// A hub that gives no metadata still serves the file, said to be unverified.
#[test]
fn hf_without_metadata_says_unverified() {
    let hub = Server::start();
    hub.serve(
        "/org/model/resolve/main/a.txt",
        Body::bytes(b"a").without_head(),
    );
    let root = tempfile::tempdir().unwrap();
    let (code, _, stderr) = run(program().args([
        "hf",
        "org/model",
        "a.txt",
        "--endpoint",
        &hub.base(),
        "-o",
        arg(root.path()),
    ]));
    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.contains("the Hub gave no size or hash"), "{stderr}");
    assert!(stderr.contains("a.txt (1 B, unverified)"), "{stderr}");
}

/// HF_ENDPOINT comes first; it answers, so the public hosts after it are never asked.
#[test]
fn hf_tries_hf_endpoint_first() {
    let hub = Server::start();
    hub.serve("/org/model/resolve/main/a.txt", Body::bytes(b"a"));
    let root = tempfile::tempdir().unwrap();
    let (code, _, stderr) = run(program().env("HF_ENDPOINT", hub.base()).args([
        "hf",
        "org/model",
        "a.txt",
        "-o",
        arg(root.path()),
    ]));
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(std::fs::read(root.path().join("a.txt")).unwrap(), b"a");
}

#[test]
fn hf_reports_every_failed_file() {
    let hub = Server::start();
    let root = tempfile::tempdir().unwrap();
    let (code, _, stderr) = run(program().args([
        "hf",
        "org/model",
        "a.bin",
        "b.bin",
        "--endpoint",
        &hub.base(),
        "-o",
        arg(root.path()),
    ]));
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("2 of 2 files failed"), "{stderr}");
}

#[test]
fn verify_checks_a_file() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("f");
    std::fs::write(&file, b"hello").unwrap();
    let (code, stdout, _) =
        run(program().args(["verify", arg(&file), "--sha256", &sha256(b"hello")]));
    assert_eq!(code, 0);
    assert!(stdout.ends_with(": OK\n"), "{stdout}");
    let (code, _, stderr) =
        run(program().args(["verify", arg(&file), "--sha256", &sha256(b"other")]));
    assert_eq!(code, 1);
    assert!(stderr.contains("sha256 mismatch"), "{stderr}");
}

/// Ctrl-C stops the download with status 130 and keeps `.part`; the same
/// command then fetches only the rest. `stall` holds the connection open after
/// the first bytes, so the program must leave without the read returning.
#[cfg(unix)]
fn interrupt_then_resume(stall: bool) {
    let data = payload(256 * 1024);
    let server = Server::start();
    let body = Body::bytes(&data);
    server.serve(
        "/big.bin",
        if stall {
            body.stalling_after(20_000)
        } else {
            body.slow(4 * 1024, Duration::from_millis(50))
        },
    );
    let root = tempfile::tempdir().unwrap();
    let part = root.path().join("big.bin.part");
    let child = program()
        .current_dir(root.path())
        .args(["get", &server.url("/big.bin"), "--sha256", &sha256(&data)])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::fs::metadata(&part).map_or(0, |m| m.len()) < 16_000 {
        assert!(Instant::now() < deadline, "no bytes arrived");
        std::thread::sleep(Duration::from_millis(20));
    }
    let started = Instant::now();
    let killed = Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(130), "{stderr}");
    assert!(started.elapsed() < Duration::from_secs(8));
    assert!(stderr.contains("cancelled;"), "{stderr}");
    assert!(
        stderr.contains("run the same command to resume"),
        "{stderr}"
    );
    assert!(!stderr.contains("panicked"), "{stderr}");
    let kept = std::fs::metadata(&part).unwrap().len();
    assert!(kept >= 16_000 && kept < data.len() as u64, "{kept}");

    server.serve("/big.bin", Body::bytes(&data));
    let (code, _, stderr) = run(program().current_dir(root.path()).args([
        "get",
        &server.url("/big.bin"),
        "--sha256",
        &sha256(&data),
    ]));
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(stderr.matches("resuming at").count(), 1, "{stderr}");
    assert_eq!(std::fs::read(root.path().join("big.bin")).unwrap(), data);
    assert_eq!(server.requests().last().unwrap().range, Some(kept));
}

#[cfg(unix)]
#[test]
fn ctrl_c_during_a_transfer_keeps_the_part_file() {
    interrupt_then_resume(false);
}

#[cfg(unix)]
#[test]
fn ctrl_c_during_a_silent_server_still_exits() {
    interrupt_then_resume(true);
}

/// Only where aria2c is installed.
#[test]
fn get_with_aria2_auto() {
    if Command::new("aria2c").arg("--version").output().is_err() {
        eprintln!("aria2c not on PATH; skipped");
        return;
    }
    let data = payload(500_000);
    let server = Server::start();
    server.serve("/a.bin", Body::bytes(&data));
    let root = tempfile::tempdir().unwrap();
    let (code, _, stderr) = run(program().current_dir(root.path()).args([
        "get",
        &server.url("/a.bin"),
        "--aria2",
        "auto",
        "--connections",
        "4",
        "--sha256",
        &sha256(&data),
    ]));
    assert_eq!(code, 0, "{stderr}");
    assert!(!stderr.contains("aria2c unavailable"), "{stderr}");
    assert_eq!(std::fs::read(root.path().join("a.bin")).unwrap(), data);
}
