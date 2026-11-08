//! The API as course2md uses it, pinned: exhaustive matches, struct literals and
//! constructors that must keep compiling as the crate grows.

use mirror_fetch::{
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

/// `Downloader::fetch` as course2md calls it.
type Fetch = for<'a> fn(
    &Downloader,
    &Remote<'a>,
    &std::path::Path,
    &dyn Cancel,
    &dyn Progress,
) -> Result<(), Error>;

/// The signatures course2md calls, as function pointers: a changed parameter
/// type fails here, not in course2md.
#[test]
fn signatures_are_pinned() {
    use std::path::PathBuf;
    let _: fn(Vec<Host>, Option<PathBuf>) -> Downloader = Downloader::new;
    let _: fn(Option<String>, Option<&'static str>) -> Endpoints = Endpoints::choose;
    let _: fn(&Endpoints) -> Option<&'static str> = Endpoints::hub_override;
    let _: fn(Option<&str>, Option<&str>) -> Option<String> = local_country;
    let _: fn() -> Option<String> = system_time_zone;
    let _: fn(Option<String>) -> Option<String> = normalize_endpoint;
    let _: fn(&Host) -> String = Host::name;
    let _: Fetch = Downloader::fetch;
    let configured: Option<String> = None;
    let aria2: Option<PathBuf> = None;
    let chosen: Endpoints = Endpoints::choose(configured, None);
    let _: Vec<Host> = chosen.hosts.clone();
    let _: Reason = chosen.reason;
    let _ = Downloader::new(chosen.hosts, aria2);
    for reason in [Reason::Configured, Reason::MainlandChina, Reason::Default] {
        match reason {
            Reason::Configured | Reason::MainlandChina | Reason::Default => {}
        }
    }
}

/// course2md keeps one Downloader in a OnceLock shared across threads.
#[test]
fn a_downloader_can_be_shared_across_threads() {
    fn shared<T: Send + Sync>() {}
    shared::<Downloader>();
    shared::<Error>();
}

#[cfg(feature = "ip-lookup")]
#[test]
fn lookup_country_is_pinned() {
    let _: fn() -> Option<String> = mirror_fetch::lookup_country;
}
