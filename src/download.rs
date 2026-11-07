//! One file, across mirrors: transfer, verify, then replace.

use crate::error::{Error, Failure, HostFailure, Stop};
use crate::hub::Host;
use crate::proxy::Route;
use std::fmt;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// A host that sends nothing for this long is treated as stalled.
pub(crate) const STALL: Duration = Duration::from_secs(60);

/// How many connections aria2 opens to one server unless told otherwise.
pub const DEFAULT_CONNECTIONS: u32 = 8;

/// The most connections aria2 accepts per server.
pub const MAX_CONNECTIONS: u32 = 16;

/// A file published in a model repository, with its size and SHA-256.
///
/// [`Downloader::fetch`] takes it with the downloader's hosts, at revision `main`.
/// [`HubFile`](crate::HubFile) names other revisions and leaves size and hash
/// optional.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Remote<'a> {
    pub repo: &'a str,
    pub file: &'a str,
    pub size: u64,
    pub sha256: &'a str,
}

/// One URL that serves a file, and the [`Host`] that events name for it.
///
/// A mirror may carry a bearer token. It is sent only to this mirror's origin
/// (scheme, host and port): it follows a redirect that stays there and is dropped
/// for good at one that leaves. It is never handed to aria2, so a mirror with a
/// token always uses the built-in client. `Debug` does not show it.
#[derive(Clone, PartialEq, Eq)]
pub struct Mirror {
    host: Host,
    url: String,
    bearer: Option<String>,
}

impl Mirror {
    /// Any HTTP(S) URL; events name it [`Host::Url`].
    pub fn new(url: impl Into<String>) -> Self {
        let url = url.into();
        Self {
            host: Host::Url(url.clone()),
            url,
            bearer: None,
        }
    }

    /// `url`, reported as `host`: how a [`HubFile`](crate::HubFile) names its hosts.
    pub fn on(host: Host, url: impl Into<String>) -> Self {
        Self {
            host,
            url: url.into(),
            bearer: None,
        }
    }

    /// Sends `Authorization: Bearer <token>` to this mirror. Blank means none.
    pub fn with_bearer(mut self, token: impl Into<String>) -> Self {
        let token = token.into();
        self.bearer = (!token.trim().is_empty()).then(|| token.trim().to_owned());
        self
    }

    pub fn host(&self) -> &Host {
        &self.host
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// Whether requests to this mirror carry a token.
    pub fn has_bearer(&self) -> bool {
        self.bearer.is_some()
    }
}

impl fmt::Debug for Mirror {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Mirror")
            .field("host", &self.host)
            .field("url", &self.url)
            .field("bearer", &self.bearer.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// One file to download: the mirrors that serve it, tried in order, and what it
/// must be. Without `size` the server's length is trusted; without `sha256` the
/// bytes are not checked.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Target {
    pub mirrors: Vec<Mirror>,
    pub size: Option<u64>,
    /// Lower-case hex; [`Target::with_sha256`] lower-cases it.
    pub sha256: Option<String>,
}

impl Target {
    /// The same file at each of `urls`, in order.
    pub fn new<I, U>(urls: I) -> Self
    where
        I: IntoIterator<Item = U>,
        U: Into<String>,
    {
        Self {
            mirrors: urls.into_iter().map(Mirror::new).collect(),
            ..Self::default()
        }
    }

    pub fn with_size(mut self, size: u64) -> Self {
        self.size = Some(size);
        self
    }

    pub fn with_sha256(mut self, hex: impl AsRef<str>) -> Self {
        self.sha256 = Some(hex.as_ref().trim().to_ascii_lowercase());
        self
    }
}

/// Polled at safe points; true stops the download with [`Error::Cancelled`]. What was
/// received stays in the `.part` file for a later run.
pub trait Cancel {
    fn is_cancelled(&self) -> bool;
}

impl Cancel for AtomicBool {
    fn is_cancelled(&self) -> bool {
        self.load(Ordering::Relaxed)
    }
}

/// Never cancels.
impl Cancel for () {
    fn is_cancelled(&self) -> bool {
        false
    }
}

/// Receives each [`Event`] on the thread that called
/// [`Downloader::download`] or [`Downloader::fetch`].
pub trait Progress {
    fn event(&self, event: Event<'_>);

    /// The total that [`Event::Position`] counts toward in the current phase, once
    /// it is known: the expected size, or the server's, while downloading; the
    /// file's length while verifying. Without an expected size or a length from
    /// the server, it never comes.
    fn length(&self, _bytes: u64) {}
}

/// Ignores everything.
impl Progress for () {
    fn event(&self, _: Event<'_>) {}
}

/// What a transfer is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Connect,
    Retry,
    Download,
    Verify,
}

/// A step of a download, for progress and for logs.
#[derive(Debug)]
pub enum Event<'a> {
    /// A new phase. `Phase::Verify` has no host: the bytes are already here.
    Phase {
        phase: Phase,
        host: Option<&'a Host>,
    },
    /// This many bytes were already on disk: a starting point, not throughput.
    Resumed { bytes: u64 },
    /// This many bytes are done in the current phase.
    Position { bytes: u64 },
    /// One HTTP request begins from byte `from`.
    Request { url: &'a str, from: u64 },
    /// A transfer stopped partway and will resume on the same host.
    Interrupted {
        host: &'a Host,
        received: u64,
        failure: &'a Failure,
    },
    /// This host is given up on; the next is tried.
    HostFailed {
        host: &'a Host,
        failure: &'a Failure,
    },
    /// The received bytes are not the published ones.
    ChecksumMismatch { expected: &'a str, actual: &'a str },
    /// aria2 cannot run, so this and later files use the built-in client.
    Aria2Unavailable { reason: &'a str },
    /// The file is in place, but its manifest could not be written.
    ManifestFailed { error: &'a std::io::Error },
    /// The file is in place, verified.
    Completed { host: &'a Host },
}

/// Downloads a set of files. Once aria2 proves unusable for one file, the others
/// skip it, including those running at the same time.
#[derive(Debug)]
pub struct Downloader {
    hosts: Vec<Host>,
    aria2: Option<PathBuf>,
    connections: u32,
    aria2_broken: AtomicBool,
    route: Route,
}

/// What one transfer attempt needs.
pub(crate) struct Transfer<'a> {
    pub url: &'a str,
    pub host: &'a Host,
    pub part: &'a Path,
    /// The expected length, if known.
    pub size: Option<u64>,
    pub bearer: Option<&'a str>,
    pub connections: u32,
    pub cancel: &'a dyn Cancel,
    pub progress: &'a dyn Progress,
    pub route: &'a Route,
}

impl Transfer<'_> {
    pub fn check(&self) -> Result<(), Stop> {
        if self.cancel.is_cancelled() {
            Err(Stop::Cancelled)
        } else {
            Ok(())
        }
    }

    pub fn phase(&self, phase: Phase) {
        self.progress.event(Event::Phase {
            phase,
            host: Some(self.host),
        });
    }
}

/// No hosts and no aria2: enough for [`Downloader::download`], whose
/// [`Target`] names its own mirrors.
impl Default for Downloader {
    fn default() -> Self {
        Self::new(Vec::new(), None)
    }
}

impl Downloader {
    /// `hosts` are tried in order by [`Downloader::fetch`]; [`Downloader::download`]
    /// ignores them. `aria2` is the program's path, if it should be used.
    ///
    /// Requests go through the proxy that `ALL_PROXY`, `HTTPS_PROXY` or `HTTP_PROXY`
    /// names, as they stand now, except to the hosts `NO_PROXY` lists and to this
    /// machine's own (`localhost`, `127.0.0.1`, `::1`).
    pub fn new(hosts: Vec<Host>, aria2: Option<PathBuf>) -> Self {
        Self {
            hosts,
            aria2,
            connections: DEFAULT_CONNECTIONS,
            aria2_broken: AtomicBool::new(false),
            route: Route::from_environment(),
        }
    }

    /// Uses aria2 at `program`, or not at all with `None`.
    pub fn with_aria2(mut self, program: Option<PathBuf>) -> Self {
        self.aria2 = program;
        self.aria2_broken = AtomicBool::new(false);
        self
    }

    /// How many connections aria2 opens to one server, from 1 to
    /// [`MAX_CONNECTIONS`]. The built-in client always uses one.
    pub fn with_connections(mut self, connections: u32) -> Self {
        self.connections = connections.clamp(1, MAX_CONNECTIONS);
        self
    }

    /// The hosts [`Downloader::fetch`] tries.
    pub fn hosts(&self) -> &[Host] {
        &self.hosts
    }

    #[cfg(test)]
    pub(crate) fn routed(mut self, route: Route) -> Self {
        self.route = route;
        self
    }

    /// Downloads `target` to `dest`, resuming from `dest` with `.part` appended
    /// (`model.gguf.part`). Each mirror is tried in turn; one whose bytes fail
    /// the size or SHA-256 is given up on and its bytes discarded. `dest` is
    /// replaced only by a complete, verified file. Its directory is created.
    pub fn download(
        &self,
        target: &Target,
        dest: &Path,
        cancel: &dyn Cancel,
        progress: &dyn Progress,
    ) -> Result<(), Error> {
        let expect = Expect {
            size: target.size,
            sha256: target.sha256.as_deref(),
        };
        self.run(
            &target.mirrors,
            expect,
            dest,
            &part_path(dest),
            cancel,
            progress,
            &|| {},
        )
    }

    /// Downloads `remote` from this downloader's hosts to `dest`, resuming from
    /// `dest` with the extension `.part`. Writes `dest` with the extension
    /// `manifest.json` afterwards, recording the verified size and hash.
    pub fn fetch(
        &self,
        remote: &Remote<'_>,
        dest: &Path,
        cancel: &dyn Cancel,
        progress: &dyn Progress,
    ) -> Result<(), Error> {
        let mirrors: Vec<Mirror> = self
            .hosts
            .iter()
            .map(|host| Mirror::on(host.clone(), host.file_url(remote.repo, remote.file)))
            .collect();
        let expect = Expect {
            size: Some(remote.size),
            sha256: Some(remote.sha256),
        };
        self.run(
            &mirrors,
            expect,
            dest,
            &dest.with_extension("part"),
            cancel,
            progress,
            &|| write_manifest(dest, remote, progress),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn run(
        &self,
        mirrors: &[Mirror],
        expect: Expect<'_>,
        dest: &Path,
        part: &Path,
        cancel: &dyn Cancel,
        progress: &dyn Progress,
        placed: &dyn Fn(),
    ) -> Result<(), Error> {
        if let Some(parent) = dest.parent().filter(|dir| !dir.as_os_str().is_empty()) {
            fs::create_dir_all(parent).map_err(Error::Io)?;
        }
        let mut failures = Vec::new();
        for mirror in mirrors {
            if cancel.is_cancelled() {
                return Err(Error::Cancelled);
            }
            let host = &mirror.host;
            let transfer = Transfer {
                url: &mirror.url,
                host,
                part,
                size: expect.size,
                bearer: mirror.bearer.as_deref(),
                connections: self.connections,
                cancel,
                progress,
                route: &self.route,
            };
            let failure = match self.transfer(&transfer) {
                Ok(()) => match check(part, expect, cancel, progress) {
                    Ok(None) => {
                        fs::rename(part, dest).map_err(Error::Io)?;
                        placed();
                        progress.event(Event::Completed { host });
                        return Ok(());
                    }
                    Ok(Some(failure)) => {
                        let _ = fs::remove_file(part);
                        failure
                    }
                    Err(Stop::Failed(Failure::Io(error))) => return Err(Error::Io(error)),
                    Err(_) => return Err(Error::Cancelled),
                },
                Err(Stop::Failed(failure)) => failure,
                Err(_) => return Err(Error::Cancelled),
            };
            if cancel.is_cancelled() {
                return Err(Error::Cancelled);
            }
            progress.event(Event::HostFailed {
                host,
                failure: &failure,
            });
            failures.push(HostFailure {
                host: host.name(),
                failure,
            });
        }
        Err(Error::AllHostsFailed(failures))
    }

    fn transfer(&self, transfer: &Transfer) -> Result<(), Stop> {
        // A token never leaves the built-in client, which keeps it on its origin.
        let program = self
            .aria2
            .as_deref()
            .filter(|_| transfer.bearer.is_none())
            .filter(|_| !self.aria2_broken.load(Ordering::Relaxed));
        if let Some(program) = program {
            match crate::aria2::run(program, transfer) {
                Err(Stop::Aria2Unavailable(reason)) => {
                    transfer
                        .progress
                        .event(Event::Aria2Unavailable { reason: &reason });
                    self.aria2_broken.store(true, Ordering::Relaxed);
                }
                result => return result,
            }
        }
        crate::native::run(transfer)
    }
}

/// What the bytes must be.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Expect<'a> {
    pub size: Option<u64>,
    pub sha256: Option<&'a str>,
}

/// `dest` with `.part` appended, so `a.tar.gz` and `a.zip` do not share one.
pub fn part_path(dest: &Path) -> PathBuf {
    let mut name = dest.as_os_str().to_owned();
    name.push(".part");
    PathBuf::from(name)
}

/// The lower-case hex SHA-256 of the file at `path`, reporting
/// [`Phase::Verify`] progress.
pub fn file_sha256(
    path: &Path,
    cancel: &dyn Cancel,
    progress: &dyn Progress,
) -> Result<String, Error> {
    match hash_file(path, cancel, progress) {
        Ok(digest) => Ok(digest),
        Err(Stop::Failed(Failure::Io(error))) => Err(Error::Io(error)),
        Err(_) => Err(Error::Cancelled),
    }
}

fn hash_file(path: &Path, cancel: &dyn Cancel, progress: &dyn Progress) -> Result<String, Stop> {
    use sha2::{Digest, Sha256};
    let mut file = fs::File::open(path)?;
    progress.event(Event::Phase {
        phase: Phase::Verify,
        host: None,
    });
    if let Ok(metadata) = file.metadata() {
        progress.length(metadata.len());
    }
    progress.event(Event::Position { bytes: 0 });
    let mut hash = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    let mut done = 0u64;
    loop {
        if done.is_multiple_of(64 << 20) && cancel.is_cancelled() {
            return Err(Stop::Cancelled);
        }
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
        done += n as u64;
        progress.event(Event::Position { bytes: done });
    }
    Ok(hash.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Whether `part` has the expected size and SHA-256, where they are known.
#[cfg(test)]
pub(crate) fn verify(
    part: &Path,
    expect: Expect<'_>,
    cancel: &dyn Cancel,
    progress: &dyn Progress,
) -> Result<bool, Stop> {
    check(part, expect, cancel, progress).map(|failure| failure.is_none())
}

/// What is wrong with `part`, if anything: its size, then its SHA-256, where they
/// are known. Reports hashing progress.
pub(crate) fn check(
    part: &Path,
    expect: Expect<'_>,
    cancel: &dyn Cancel,
    progress: &dyn Progress,
) -> Result<Option<Failure>, Stop> {
    if let Some(size) = expect.size {
        let actual = fs::metadata(part).map(|m| m.len()).unwrap_or(0);
        if actual != size {
            return Ok(Some(Failure::Incomplete {
                expected: size,
                received: actual,
            }));
        }
    }
    let Some(expected) = expect.sha256 else {
        return Ok(None);
    };
    let digest = hash_file(part, cancel, progress)?;
    if digest.eq_ignore_ascii_case(expected.trim()) {
        return Ok(None);
    }
    progress.event(Event::ChecksumMismatch {
        expected,
        actual: &digest,
    });
    Ok(Some(Failure::ChecksumMismatch))
}

/// Records the verified size so a later start can trust the file without hashing it.
fn write_manifest(dest: &Path, remote: &Remote<'_>, progress: &dyn Progress) {
    let manifest = serde_json::json!({"size": remote.size, "sha256": remote.sha256});
    let write = || -> std::io::Result<()> {
        let dir = dest.parent().filter(|dir| !dir.as_os_str().is_empty());
        let mut file = tempfile::NamedTempFile::new_in(dir.unwrap_or(Path::new(".")))?;
        file.write_all(manifest.to_string().as_bytes())?;
        file.as_file().sync_all()?;
        file.persist(dest.with_extension("manifest.json"))?;
        Ok(())
    };
    if let Err(error) = write() {
        progress.event(Event::ManifestFailed { error: &error });
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    //! A loopback HTTP/1.1 server answering each connection with the next canned
    //! response, as raw bytes so tests can send truncated bodies.

    use std::io::{Read, Write};
    use std::time::Duration;

    /// Returns the base URL and a handle yielding each request's head.
    pub fn serve(responses: Vec<String>) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let mut heads = Vec::new();
            for response in responses {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut data = Vec::new();
                let mut buffer = [0u8; 4096];
                while !data.windows(4).any(|w| w == b"\r\n\r\n") {
                    let n = stream.read(&mut buffer).unwrap();
                    if n == 0 {
                        break;
                    }
                    data.extend_from_slice(&buffer[..n]);
                }
                heads.push(String::from_utf8_lossy(&data).into_owned());
                stream.write_all(response.as_bytes()).unwrap();
            }
            heads
        });
        (format!("http://{address}"), server)
    }

    /// A CONNECT proxy on loopback: each connection asks for a tunnel, then sends one
    /// request inside it, answered with the next canned response. Returns the proxy's
    /// URL and a handle yielding each CONNECT head and inner request head.
    pub fn serve_proxy(
        responses: Vec<String>,
    ) -> (String, std::thread::JoinHandle<Vec<(String, String)>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let mut heads = Vec::new();
            for response in responses {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let connect = read_head(&mut stream);
                stream
                    .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                    .unwrap();
                let request = read_head(&mut stream);
                stream.write_all(response.as_bytes()).unwrap();
                heads.push((connect, request));
            }
            heads
        });
        (format!("http://{address}"), server)
    }

    fn read_head(stream: &mut std::net::TcpStream) -> String {
        let mut data = Vec::new();
        let mut byte = [0u8; 1];
        while !data.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap() == 1 {
            data.push(byte[0]);
        }
        String::from_utf8_lossy(&data).into_owned()
    }

    pub fn sha256(body: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        Sha256::digest(body)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{serve, sha256};
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Log(Mutex<Vec<String>>);

    impl Progress for Log {
        fn event(&self, event: Event<'_>) {
            self.0.lock().unwrap().push(format!("{event:?}"));
        }
    }

    #[test]
    fn corrupted_bytes_fail_verification() {
        let hash = sha256(b"12345");
        let remote = Remote {
            repo: "test/repo",
            file: "synthetic.bin",
            size: 5,
            sha256: &hash,
        };
        let root = tempfile::tempdir().unwrap();
        let part = root.path().join("synthetic.part");
        let expect = Expect {
            size: Some(remote.size),
            sha256: Some(remote.sha256),
        };
        fs::write(&part, b"12346").unwrap();
        assert!(!verify(&part, expect, &(), &()).unwrap());
        fs::write(&part, b"1234").unwrap();
        assert!(!verify(&part, expect, &(), &()).unwrap());
        fs::write(&part, b"12345").unwrap();
        assert!(verify(&part, expect, &(), &()).unwrap());
        let upper = hash.to_ascii_uppercase();
        let only_hash = Expect {
            size: None,
            sha256: Some(&upper),
        };
        assert!(verify(&part, only_hash, &(), &()).unwrap());
        let nothing = Expect {
            size: None,
            sha256: None,
        };
        fs::write(&part, b"anything").unwrap();
        assert!(verify(&part, nothing, &(), &()).unwrap());
    }

    #[test]
    fn a_fetch_falls_through_hosts_and_writes_the_manifest() {
        let hash = sha256(b"12345");
        let remote = Remote {
            repo: "test/repo",
            file: "synthetic.bin",
            size: 5,
            sha256: &hash,
        };
        let (missing, first) = serve(vec![
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
        ]);
        let (good, second) = serve(vec![
            "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\n12345".into(),
        ]);
        let root = tempfile::tempdir().unwrap();
        let dest = root.path().join("models/synthetic.bin");
        let log = Log::default();
        let downloader = Downloader::new(vec![Host::Hub(missing), Host::Hub(good)], None);
        downloader.fetch(&remote, &dest, &(), &log).unwrap();
        first.join().unwrap();
        second.join().unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"12345");
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(dest.with_extension("manifest.json")).unwrap())
                .unwrap();
        assert_eq!(manifest["size"], 5);
        assert_eq!(manifest["sha256"], hash.as_str());
        assert!(!dest.with_extension("part").exists());
        let log = log.0.lock().unwrap();
        assert!(log.iter().any(|event| event.contains("HostFailed")));
        assert!(log.last().unwrap().contains("Completed"));
    }

    #[test]
    fn a_wrong_checksum_reports_every_host() {
        let remote = Remote {
            repo: "test/repo",
            file: "synthetic.bin",
            size: 5,
            sha256: "0000",
        };
        let (base, server) = serve(vec![
            "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\n12345".into(),
        ]);
        let root = tempfile::tempdir().unwrap();
        let dest = root.path().join("synthetic.bin");
        let error = Downloader::new(vec![Host::Hub(base)], None)
            .fetch(&remote, &dest, &(), &())
            .unwrap_err();
        server.join().unwrap();
        match error {
            Error::AllHostsFailed(failures) => {
                assert!(matches!(failures[0].failure, Failure::ChecksumMismatch));
            }
            other => panic!("{other:?}"),
        }
        assert!(!dest.exists());
    }

    /// A remote host is reached through the environment's proxy; a loopback host
    /// beside it is not, even with that proxy unreachable.
    #[test]
    fn remote_hosts_go_through_the_proxy_and_loopback_does_not() {
        use super::test_support::serve_proxy;
        let hash = sha256(b"12345");
        let remote = Remote {
            repo: "test/repo",
            file: "synthetic.bin",
            size: 5,
            sha256: &hash,
        };
        let (proxy, server) = serve_proxy(vec![
            "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\n12345".into(),
        ]);
        let route = Route::new(Some(ureq::Proxy::new(&proxy).unwrap()), None);
        let root = tempfile::tempdir().unwrap();
        let dest = root.path().join("proxied.bin");
        Downloader::new(vec![Host::Hub("http://models.invalid".into())], None)
            .routed(route)
            .fetch(&remote, &dest, &(), &())
            .unwrap();
        let heads = server.join().unwrap();
        assert!(heads[0].0.starts_with("CONNECT models.invalid:80 "));
        assert!(heads[0].1.contains("/test/repo/resolve/main/synthetic.bin"));
        assert_eq!(fs::read(&dest).unwrap(), b"12345");

        let (base, server) = serve(vec![
            "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\n12345".into(),
        ]);
        let unreachable = Route::new(Some(ureq::Proxy::new("http://127.0.0.1:1").unwrap()), None);
        let dest = root.path().join("direct.bin");
        Downloader::new(vec![Host::Hub(base)], None)
            .routed(unreachable)
            .fetch(&remote, &dest, &(), &())
            .unwrap();
        server.join().unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"12345");
    }

    /// The general path: plain URLs, no manifest, `.part` appended to the name.
    #[test]
    fn a_download_falls_through_mirrors_without_a_manifest() {
        let (missing, first) = serve(vec![
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
        ]);
        let (good, second) = serve(vec![
            "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\n12345".into(),
        ]);
        let root = tempfile::tempdir().unwrap();
        let dest = root.path().join("file.tar.gz");
        let target = Target::new([format!("{missing}/a"), format!("{good}/b")])
            .with_sha256(sha256(b"12345").to_ascii_uppercase());
        Downloader::default()
            .download(&target, &dest, &(), &())
            .unwrap();
        first.join().unwrap();
        assert!(second.join().unwrap()[0].starts_with("GET /b "));
        assert_eq!(fs::read(&dest).unwrap(), b"12345");
        assert!(!part_path(&dest).exists());
        assert_eq!(part_path(&dest), root.path().join("file.tar.gz.part"));
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
    }

    /// A transfer that ends at the wrong length is reported as such, not as a
    /// checksum mismatch.
    #[test]
    fn a_wrong_size_is_reported_as_incomplete() {
        let root = tempfile::tempdir().unwrap();
        let part = root.path().join("f.part");
        fs::write(&part, b"1234").unwrap();
        let expect = Expect {
            size: Some(5),
            sha256: Some("00"),
        };
        match check(&part, expect, &(), &()).unwrap() {
            Some(Failure::Incomplete { expected, received }) => {
                assert_eq!((expected, received), (5, 4));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_cancelled_fetch_touches_nothing() {
        let remote = Remote {
            repo: "r",
            file: "f",
            size: 1,
            sha256: "",
        };
        let root = tempfile::tempdir().unwrap();
        let error = Downloader::new(vec![Host::ModelScope], None)
            .fetch(&remote, &root.path().join("f"), &AtomicBool::new(true), &())
            .unwrap_err();
        assert!(matches!(error, Error::Cancelled));
    }
}
