//! The API as course2md uses it, pinned: exhaustive matches, struct literals and
//! constructors that must keep compiling as the crate grows.

use model_fetch::{
    Cancel, Downloader, Endpoints, Error, Event, Failure, Host, MIRROR, ORIGIN, Phase, Progress,
    Reason, Remote, local_country, normalize_endpoint, system_time_zone,
};
use std::sync::Mutex;

mod common;

struct Words(Mutex<Vec<String>>);

impl Progress for Words {
    fn event(&self, event: Event<'_>) {
        let word = match event {
            Event::Phase { phase, host } => {
                let name = host.map(Host::name).unwrap_or_default();
                match phase {
                    Phase::Connect => format!("connect {name}"),
                    Phase::Retry => format!("retry {name}"),
                    Phase::Verify => "verify".to_owned(),
                    Phase::Download => format!("download {name}"),
                }
            }
            Event::Resumed { bytes } => format!("resumed {bytes}"),
            Event::Position { bytes } => format!("at {bytes}"),
            Event::Request { url, from } => format!("request {url} {from}"),
            Event::Interrupted {
                host,
                received,
                failure,
            } => format!("interrupted {} {received} {}", host.name(), text(failure)),
            Event::HostFailed { host, failure } => {
                format!("failed {} {}", host.name(), text(failure))
            }
            Event::ChecksumMismatch { expected, actual } => format!("mismatch {expected} {actual}"),
            Event::Aria2Unavailable { reason } => format!("no aria2 {reason}"),
            Event::ManifestFailed { error } => format!("manifest {error}"),
            Event::Completed { host } => format!("done {}", host.name()),
        };
        self.0.lock().unwrap().push(word);
    }
}

fn text(failure: &Failure) -> String {
    match failure {
        Failure::Request(message) => message.clone(),
        Failure::Status { code, body } => format!("HTTP {code}: {body}"),
        Failure::Incomplete { expected, received } => format!("{expected} {received}"),
        Failure::Stalled => "stalled".into(),
        Failure::ChecksumMismatch => "checksum".into(),
        Failure::Aria2 { code, message } => format!("{code} {message}"),
        Failure::Aria2Exited { status, stderr } => format!("{status} {stderr}"),
        Failure::Aria2Rpc(message) => message.clone(),
        Failure::Io(error) => error.to_string(),
    }
}

struct Never;

impl Cancel for Never {
    fn is_cancelled(&self) -> bool {
        false
    }
}

#[test]
fn fetch_with_a_published_hash_writes_the_file_and_its_manifest() {
    let body = b"published bytes";
    let server = common::Server::start();
    server.serve(
        "/test/repo/resolve/main/synthetic.bin",
        common::Body::bytes(body),
    );
    let hash = common::sha256(body);
    let remote = Remote {
        repo: "test/repo",
        file: "synthetic.bin",
        size: body.len() as u64,
        sha256: &hash,
    };
    let root = tempfile::tempdir().unwrap();
    let dest = root.path().join("synthetic.bin");
    let words = Words(Mutex::new(Vec::new()));
    let downloader = Downloader::new(vec![Host::Hub(server.base())], None);
    match downloader.fetch(&remote, &dest, &Never, &words) {
        Ok(()) => {}
        Err(Error::AllHostsFailed(failures)) => panic!(
            "{:?}",
            failures
                .iter()
                .map(|failed| format!("{}: {}", failed.host, text(&failed.failure)))
                .collect::<Vec<_>>()
        ),
        Err(Error::Io(error)) => panic!("{error}"),
        Err(Error::Cancelled) => panic!("cancelled"),
    }
    assert_eq!(std::fs::read(&dest).unwrap(), body);
    assert!(dest.with_extension("manifest.json").exists());
    assert_eq!(
        words.0.lock().unwrap().last().unwrap(),
        "done 127.0.0.1",
        "events end in completion"
    );
}

#[test]
fn host_selection_is_unchanged() {
    let chosen = Endpoints::choose(None, local_country(Some("zh-CN"), None).as_deref());
    assert_eq!(chosen.reason, Reason::MainlandChina);
    assert_eq!(chosen.hosts[1], Host::Hub(MIRROR.into()));
    assert_eq!(
        Endpoints::choose(None, None).hosts[0],
        Host::Hub(ORIGIN.into())
    );
    assert_eq!(
        normalize_endpoint(Some("https://e.example/".into())).as_deref(),
        Some("https://e.example")
    );
    let _ = system_time_zone();
}
