//! `model-fetch`: resumable, verified downloads from the command line.
//!
//! Exit codes: 0 done, 1 failed, 2 usage error, 130 cancelled.

mod progress;

use clap::{Args, Parser, Subcommand};
use model_fetch::{
    Cancel, Downloader, Error, Host, HubFile, MIRROR, ORIGIN, Target, file_matches, file_sha256,
    normalize_endpoint, part_path,
};
use progress::{Printer, human_bytes};
use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Set by Ctrl-C; downloads stop at their next read and keep their `.part`.
static CANCELLED: AtomicBool = AtomicBool::new(false);

/// The partial file being written, for the message if a stalled read outlasts
/// the grace period after Ctrl-C.
static CURRENT_PART: Mutex<Option<PathBuf>> = Mutex::new(None);

/// How long a cancelled download may take to notice before the program leaves anyway.
const GRACE: Duration = Duration::from_secs(3);

#[derive(Parser)]
#[command(
    name = "model-fetch",
    version,
    about = "Resumable, verified downloads from ordered mirrors",
    long_about = "Resumable, verified downloads from ordered mirrors.\n\n\
        Partial data is kept in <file>.part, so running the same command again resumes. \
        Proxies come from ALL_PROXY, HTTPS_PROXY, HTTP_PROXY and NO_PROXY; loopback is \
        always reached directly.\n\n\
        Exit status: 0 done, 1 failed, 2 usage error, 130 cancelled."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Download one file from a URL, falling back to mirrors
    Get(Get),
    /// Download files from a Hugging Face repository
    #[command(long_about = "Download files from a Hugging Face repository.\n\n\
        Hosts are tried in order: the --endpoint values if given; otherwise HF_ENDPOINT \
        (if set), huggingface.co and hf-mirror.com. HF_TOKEN, if set, is sent only to \
        huggingface.co and hf.co over HTTPS, never to a mirror, and never printed.")]
    Hf(Hf),
    /// Check a file's SHA-256
    Verify(Verify),
}

#[derive(Args)]
struct Transfer {
    /// Use aria2c for several connections: its path, or `auto` to find it on PATH
    #[arg(long, value_name = "PATH|auto")]
    aria2: Option<String>,
    /// Connections per server for aria2c, 1 to 16 [default: 8]
    #[arg(long, value_name = "N", requires = "aria2",
          value_parser = clap::value_parser!(u32).range(1..=16))]
    connections: Option<u32>,
    /// Print only errors
    #[arg(short, long)]
    quiet: bool,
}

#[derive(Args)]
struct Get {
    /// The file's URL
    #[arg(value_parser = parse_url)]
    url: String,
    /// Another URL for the same file, tried in order after the first
    #[arg(long = "mirror", value_name = "URL", value_parser = parse_url)]
    mirrors: Vec<String>,
    /// Where to save it: a file, or an existing directory [default: the URL's file name]
    #[arg(short, long, value_name = "PATH")]
    output: Option<PathBuf>,
    /// The expected SHA-256, in hex; a mirror whose bytes differ is skipped
    #[arg(long, value_name = "HEX", value_parser = parse_sha256)]
    sha256: Option<String>,
    /// The expected size in bytes
    #[arg(long, value_name = "N")]
    size: Option<u64>,
    #[command(flatten)]
    transfer: Transfer,
}

#[derive(Args)]
struct Hf {
    /// The repository, as `org/name`
    #[arg(value_parser = parse_repo)]
    repo: String,
    /// Paths inside the repository
    #[arg(required = true, value_name = "FILE")]
    files: Vec<String>,
    /// A branch, tag or commit
    #[arg(long, default_value = "main")]
    revision: String,
    /// A Hub-compatible base URL, tried in order; replaces the default hosts
    #[arg(long = "endpoint", value_name = "URL", value_parser = parse_url)]
    endpoints: Vec<String>,
    /// The directory to save into; each file keeps its path in the repository
    #[arg(short, long, value_name = "DIR", default_value = ".")]
    output: PathBuf,
    #[command(flatten)]
    transfer: Transfer,
}

#[derive(Args)]
struct Verify {
    /// The file to check
    file: PathBuf,
    /// The expected SHA-256, in hex
    #[arg(long, value_name = "HEX", value_parser = parse_sha256)]
    sha256: String,
}

/// How the program ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Done,
    Failed,
    Usage,
    Cancelled,
}

impl Outcome {
    fn code(self) -> ExitCode {
        ExitCode::from(match self {
            Self::Done => 0,
            Self::Failed => 1,
            Self::Usage => 2,
            Self::Cancelled => 130,
        })
    }
}

fn parse_url(value: &str) -> Result<String, String> {
    let url = url::Url::parse(value).map_err(|error| format!("not a URL: {error}"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err("only http:// and https:// URLs are supported".into());
    }
    Ok(value.to_owned())
}

fn parse_sha256(value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(value.to_ascii_lowercase())
    } else {
        Err("expected 64 hexadecimal characters".into())
    }
}

fn parse_repo(value: &str) -> Result<String, String> {
    let parts: Vec<&str> = value.split('/').collect();
    let plain = |part: &&str| {
        !part.is_empty()
            && *part != "."
            && *part != ".."
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    };
    if parts.len() == 2 && parts.iter().all(plain) {
        Ok(value.to_owned())
    } else {
        Err("expected a repository as org/name".into())
    }
}

/// A relative path inside the output directory, or why it is not one.
fn repo_path(file: &str) -> Result<PathBuf, String> {
    let path = Path::new(file);
    let plain = !file.is_empty()
        && !file.contains('\\')
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
        && !file.split('/').any(str::is_empty);
    if plain {
        Ok(path.to_path_buf())
    } else {
        Err(format!(
            "'{file}' is not a plain path inside the repository"
        ))
    }
}

/// The last segment of the URL's path, decoded, if it makes a safe file name.
fn name_from_url(url: &str) -> Option<String> {
    let url = url::Url::parse(url).ok()?;
    let segment = url.path_segments()?.next_back()?;
    let name = percent_decode(segment)?;
    let safe = !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\', '\0'])
        && !name.chars().any(char::is_control);
    safe.then_some(name)
}

fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Reads [`CANCELLED`].
struct CtrlC;

impl Cancel for CtrlC {
    fn is_cancelled(&self) -> bool {
        CANCELLED.load(Ordering::Relaxed)
    }
}

fn error(message: impl std::fmt::Display) {
    eprintln!("model-fetch: error: {message}");
}

fn usage(message: impl std::fmt::Display) -> Outcome {
    eprintln!("model-fetch: usage error: {message}");
    Outcome::Usage
}

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            let _ = error.print();
            // Help and version go to stdout and succeed; anything else is misuse.
            return if error.use_stderr() {
                Outcome::Usage.code()
            } else {
                Outcome::Done.code()
            };
        }
    };
    // The first Ctrl-C asks downloads to stop; a second leaves at once.
    let handler = ctrlc::set_handler(|| {
        if CANCELLED.swap(true, Ordering::Relaxed) {
            std::process::exit(130);
        }
    });
    if let Err(reason) = handler {
        error(format!("cannot handle Ctrl-C: {reason}"));
    }
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(run(cli));
    });
    let mut cancelled_at = None;
    loop {
        match receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(outcome) => return outcome.code(),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                error("the download stopped unexpectedly");
                return Outcome::Failed.code();
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if !CANCELLED.load(Ordering::Relaxed) {
                    continue;
                }
                let since = *cancelled_at.get_or_insert_with(Instant::now);
                if since.elapsed() >= GRACE {
                    // A read is still waiting on a silent server; what arrived is on disk.
                    let part = CURRENT_PART.lock().ok().and_then(|part| part.clone());
                    report_cancelled(part.as_deref());
                    return Outcome::Cancelled.code();
                }
            }
        }
    }
}

fn run(cli: Cli) -> Outcome {
    match cli.command {
        Command::Get(get) => run_get(get),
        Command::Hf(hf) => run_hf(hf),
        Command::Verify(verify) => run_verify(verify),
    }
}

/// aria2c on PATH, as `auto` asks.
fn find_on_path(program: &str) -> Option<PathBuf> {
    let names: Vec<String> = if cfg!(windows) {
        vec![format!("{program}.exe"), program.to_owned()]
    } else {
        vec![program.to_owned()]
    };
    std::env::split_paths(&std::env::var_os("PATH")?)
        .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
        .find(|path| path.is_file())
}

fn downloader(transfer: &Transfer) -> Downloader {
    let aria2 = match transfer.aria2.as_deref() {
        None => None,
        Some("auto") => {
            let found = find_on_path("aria2c");
            if found.is_none() && !transfer.quiet {
                eprintln!("model-fetch: aria2c not found on PATH; using the built-in client");
            }
            found
        }
        Some(path) => Some(PathBuf::from(path)),
    };
    let mut downloader = Downloader::default().with_aria2(aria2);
    if let Some(connections) = transfer.connections {
        downloader = downloader.with_connections(connections);
    }
    downloader
}

fn run_get(get: Get) -> Outcome {
    let dest = match &get.output {
        Some(output) if output.is_dir() || output.as_os_str().to_string_lossy().ends_with('/') => {
            match name_from_url(&get.url) {
                Some(name) => output.join(name),
                None => return usage("cannot name the file from its URL; pass -o FILE"),
            }
        }
        Some(output) => output.clone(),
        None => match name_from_url(&get.url) {
            Some(name) => PathBuf::from(name),
            None => return usage("cannot name the file from its URL; pass -o FILE"),
        },
    };
    let mut target = Target::new(std::iter::once(get.url.clone()).chain(get.mirrors.clone()));
    if let Some(size) = get.size {
        target = target.with_size(size);
    }
    if let Some(sha256) = &get.sha256 {
        target = target.with_sha256(sha256);
    }
    download(
        &downloader(&get.transfer),
        &target,
        &dest,
        get.transfer.quiet,
    )
}

fn run_hf(hf: Hf) -> Outcome {
    let mut paths = Vec::new();
    for file in &hf.files {
        match repo_path(file) {
            Ok(path) => paths.push(path),
            Err(reason) => return usage(reason),
        }
    }
    if hf.revision.trim().is_empty() {
        return usage("the revision is empty");
    }
    let mut bases: Vec<String> = if hf.endpoints.is_empty() {
        normalize_endpoint(std::env::var("HF_ENDPOINT").ok())
            .into_iter()
            .chain([ORIGIN.to_owned(), MIRROR.to_owned()])
            .collect()
    } else {
        hf.endpoints
            .iter()
            .filter_map(|endpoint| normalize_endpoint(Some(endpoint.clone())))
            .collect()
    };
    let mut seen = std::collections::HashSet::new();
    bases.retain(|base| seen.insert(base.clone()));
    let hosts: Vec<Host> = bases.into_iter().map(Host::Hub).collect();
    let token = std::env::var("HF_TOKEN")
        .ok()
        .filter(|token| !token.trim().is_empty());
    let downloader = downloader(&hf.transfer);
    let mut failed = 0;
    for (file, path) in hf.files.iter().zip(&paths) {
        let file = HubFile::new(&hf.repo, file).at(&hf.revision);
        let mirrors = match file.mirrors(&hosts, token.as_deref()) {
            Ok(mirrors) => mirrors,
            Err(reason) => return usage(reason),
        };
        // The Hub's size and hash, at the commit the revision names, so a branch
        // that moves meanwhile cannot mix two versions.
        let target = match downloader.hub_metadata(&mirrors) {
            Ok(metadata) => {
                let pinned = match &metadata.commit {
                    Some(commit) => file.at(commit),
                    None => file,
                };
                match pinned.target(&hosts, token.as_deref()) {
                    Ok(target) => metadata.apply(target),
                    Err(reason) => return usage(reason),
                }
            }
            Err(reason) => {
                if !hf.transfer.quiet {
                    let why = match &reason {
                        Error::AllHostsFailed(failures) => failures
                            .iter()
                            .map(|failed| format!("{}: {}", failed.host, failed.failure))
                            .collect::<Vec<_>>()
                            .join("; "),
                        other => other.to_string(),
                    };
                    eprintln!(
                        "{}: the Hub gave no size or hash ({why}); the download will be unverified",
                        file.file
                    );
                }
                Target {
                    mirrors,
                    ..Target::default()
                }
            }
        };
        match download(
            &downloader,
            &target,
            &hf.output.join(path),
            hf.transfer.quiet,
        ) {
            Outcome::Done => {}
            Outcome::Failed => failed += 1,
            other => return other,
        }
    }
    if failed > 0 {
        if hf.files.len() > 1 {
            error(format!("{failed} of {} files failed", hf.files.len()));
        }
        Outcome::Failed
    } else {
        Outcome::Done
    }
}

fn run_verify(verify: Verify) -> Outcome {
    let label = verify.file.display().to_string();
    let printer = Printer::new(&label, false);
    let result = file_sha256(&verify.file, &CtrlC, &printer);
    printer.finish();
    match result {
        Ok(actual) if actual == verify.sha256 => {
            println!("{label}: OK");
            Outcome::Done
        }
        Ok(actual) => {
            error(format!(
                "{label}: sha256 mismatch\n  expected {}\n  actual   {actual}",
                verify.sha256
            ));
            Outcome::Failed
        }
        Err(Error::Cancelled) => {
            eprintln!("model-fetch: cancelled");
            Outcome::Cancelled
        }
        Err(Error::Io(reason)) => {
            error(format!("{label}: {reason}"));
            Outcome::Failed
        }
        Err(other) => {
            error(other);
            Outcome::Failed
        }
    }
}

fn report_cancelled(part: Option<&Path>) {
    let kept = part.and_then(|part| Some((part, std::fs::metadata(part).ok()?.len())));
    match kept {
        Some((part, bytes)) if bytes > 0 => eprintln!(
            "model-fetch: cancelled; {} kept in {}; run the same command to resume",
            human_bytes(bytes),
            part.display()
        ),
        _ => eprintln!("model-fetch: cancelled"),
    }
}

/// How a finished file was checked.
fn checked(target: &Target) -> &'static str {
    if target.sha256.is_some() {
        "sha256 verified"
    } else if target.git_sha1.is_some() {
        "git hash verified"
    } else if target.size.is_some() {
        "size checked, unverified"
    } else {
        "unverified"
    }
}

/// One file: skip it if already verified, else download, and say how it went.
fn download(downloader: &Downloader, target: &Target, dest: &Path, quiet: bool) -> Outcome {
    let label = dest
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| dest.display().to_string());
    let printer = Printer::new(&label, quiet);
    if dest.is_file() && target.has_hash() {
        match file_matches(dest, target, &CtrlC, &printer) {
            Ok(true) => {
                printer.finish();
                if !quiet {
                    eprintln!("{}: already complete ({})", dest.display(), checked(target));
                }
                return Outcome::Done;
            }
            Err(Error::Cancelled) => {
                printer.finish();
                report_cancelled(None);
                return Outcome::Cancelled;
            }
            _ => {}
        }
    }
    let part = part_path(dest);
    if let Ok(mut current) = CURRENT_PART.lock() {
        *current = Some(part.clone());
    }
    if let Ok(metadata) = std::fs::metadata(&part)
        && metadata.len() > 0
    {
        printer.resuming(metadata.len());
    }
    let result = downloader.download(target, dest, &CtrlC, &printer);
    printer.finish();
    match result {
        Ok(()) => {
            if !quiet {
                let size = std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0);
                eprintln!(
                    "saved {} ({}, {})",
                    dest.display(),
                    human_bytes(size),
                    checked(target)
                );
            }
            Outcome::Done
        }
        Err(Error::Cancelled) => {
            report_cancelled(Some(&part));
            Outcome::Cancelled
        }
        Err(Error::AllHostsFailed(failures)) => {
            let mut message = format!("could not download {label}");
            for failure in &failures {
                message.push_str(&format!("\n  {}: {}", failure.host, failure.failure));
            }
            if failures.is_empty() {
                message.push_str(": no URL to try");
            }
            error(message);
            Outcome::Failed
        }
        Err(Error::Io(reason)) => {
            error(format!("{}: {reason}", dest.display()));
            Outcome::Failed
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_come_from_the_url_path() {
        assert_eq!(
            name_from_url("https://e.example/a/model%20v2.gguf?x=1").as_deref(),
            Some("model v2.gguf")
        );
        for url in [
            "https://e.example/",
            "https://e.example/a/..",
            "https://e.example/a%2Fb",
            "https://e.example/%ZZ",
        ] {
            assert_eq!(name_from_url(url), None, "{url}");
        }
    }

    #[test]
    fn repository_paths_stay_inside_the_output() {
        assert!(repo_path("onnx/model.onnx").is_ok());
        for bad in ["", "../x", "/etc/passwd", "a//b", "a\\b", "./a", "a/"] {
            assert!(repo_path(bad).is_err(), "{bad}");
        }
        assert!(parse_repo("org/name").is_ok());
        for bad in ["org", "org/name/x", "../name", "org/"] {
            assert!(parse_repo(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn arguments_are_checked() {
        assert!(parse_sha256(&"A".repeat(64)).is_ok());
        assert!(parse_sha256("abc").is_err());
        assert!(parse_url("ftp://e.example/f").is_err());
        assert!(parse_url("https://e.example/f").is_ok());
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }
}
