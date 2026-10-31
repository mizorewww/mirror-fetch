//! One file, across hosts: transfer, verify, then replace.

use crate::error::{Error, Failure, HostFailure, Stop};
use crate::hub::Host;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// A host that sends nothing for this long is treated as stalled.
pub(crate) const STALL: Duration = Duration::from_secs(60);

/// A file published in a model repository, with its size and SHA-256.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Remote<'a> {
    pub repo: &'a str,
    pub file: &'a str,
    pub size: u64,
    pub sha256: &'a str,
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
/// [`Downloader::fetch`](crate::Downloader::fetch).
pub trait Progress {
    fn event(&self, event: Event<'_>);
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
    aria2_broken: AtomicBool,
}

/// What one transfer attempt needs.
pub(crate) struct Transfer<'a> {
    pub url: &'a str,
    pub host: &'a Host,
    pub part: &'a Path,
    pub remote: &'a Remote<'a>,
    pub cancel: &'a dyn Cancel,
    pub progress: &'a dyn Progress,
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

impl Downloader {
    /// `hosts` are tried in order. `aria2` is the program's path, if it should be used.
    pub fn new(hosts: Vec<Host>, aria2: Option<PathBuf>) -> Self {
        Self {
            hosts,
            aria2,
            aria2_broken: AtomicBool::new(false),
        }
    }

    /// Downloads `remote` to `dest`, resuming from `dest` with the extension `.part`.
    /// Writes `dest` with the extension `manifest.json` afterwards, recording the
    /// verified size and hash.
    pub fn fetch(
        &self,
        remote: &Remote<'_>,
        dest: &Path,
        cancel: &dyn Cancel,
        progress: &dyn Progress,
    ) -> Result<(), Error> {
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(Error::Io)?;
        }
        let part = dest.with_extension("part");
        let mut failures = Vec::new();
        for host in &self.hosts {
            if cancel.is_cancelled() {
                return Err(Error::Cancelled);
            }
            let url = host.file_url(remote.repo, remote.file);
            let transfer = Transfer {
                url: &url,
                host,
                part: &part,
                remote,
                cancel,
                progress,
            };
            let failure = match self.transfer(&transfer) {
                Ok(()) => match verify(&part, remote, cancel, progress) {
                    Ok(true) => {
                        fs::rename(&part, dest).map_err(Error::Io)?;
                        write_manifest(dest, remote, progress);
                        progress.event(Event::Completed { host });
                        return Ok(());
                    }
                    Ok(false) => {
                        let _ = fs::remove_file(&part);
                        failures.push(HostFailure {
                            host: host.name(),
                            failure: Failure::ChecksumMismatch,
                        });
                        continue;
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
        let program = self
            .aria2
            .as_deref()
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

/// Whether `part` has the published size and SHA-256; reports hashing progress.
pub(crate) fn verify(
    part: &Path,
    remote: &Remote<'_>,
    cancel: &dyn Cancel,
    progress: &dyn Progress,
) -> Result<bool, Stop> {
    use sha2::{Digest, Sha256};
    if fs::metadata(part).map(|m| m.len()).unwrap_or(0) != remote.size {
        return Ok(false);
    }
    progress.event(Event::Phase {
        phase: Phase::Verify,
        host: None,
    });
    progress.event(Event::Position { bytes: 0 });
    let mut file = fs::File::open(part)?;
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
    let digest: String = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if digest != remote.sha256 {
        progress.event(Event::ChecksumMismatch {
            expected: remote.sha256,
            actual: &digest,
        });
    }
    Ok(digest == remote.sha256)
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
        fs::write(&part, b"12346").unwrap();
        assert!(!verify(&part, &remote, &(), &()).unwrap());
        fs::write(&part, b"1234").unwrap();
        assert!(!verify(&part, &remote, &(), &()).unwrap());
        fs::write(&part, b"12345").unwrap();
        assert!(verify(&part, &remote, &(), &()).unwrap());
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
