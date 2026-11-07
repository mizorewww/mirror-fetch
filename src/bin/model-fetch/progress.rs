//! Progress on stderr: one line redrawn in place on a terminal, plain lines for
//! each step otherwise.

use model_fetch::{Event, Phase, Progress};
use std::io::{IsTerminal, Write};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// The bytes the current download has confirmed, for the message after Ctrl-C.
static KEPT: AtomicU64 = AtomicU64::new(0);

pub fn kept() -> u64 {
    KEPT.load(Ordering::Relaxed)
}

pub fn reset_kept() {
    KEPT.store(0, Ordering::Relaxed);
}

/// How often the terminal line is redrawn.
const REDRAW: Duration = Duration::from_millis(100);

pub struct Printer {
    label: String,
    tty: bool,
    quiet: bool,
    /// How many mirrors will be tried: after the last fails there is no next.
    mirrors: usize,
    /// What the verify phase computes: `sha256` or `git hash`.
    hash: &'static str,
    state: Mutex<State>,
}

struct State {
    phase: Option<Phase>,
    host: String,
    total: Option<u64>,
    position: u64,
    /// Where the current download phase started, so resumed bytes are not speed.
    base: u64,
    started: Instant,
    drawn: Option<Instant>,
    /// Bytes on disk from before, said once the connection starts.
    resumed: Option<u64>,
    failed_mirrors: usize,
}

impl Printer {
    pub fn new(label: &str, quiet: bool) -> Self {
        Self {
            label: label.to_owned(),
            tty: std::io::stderr().is_terminal(),
            quiet,
            mirrors: 0,
            hash: "sha256",
            state: Mutex::new(State {
                phase: None,
                host: String::new(),
                total: None,
                position: 0,
                base: 0,
                started: Instant::now(),
                drawn: None,
                resumed: None,
                failed_mirrors: 0,
            }),
        }
    }

    /// Clears the progress line, if one is showing.
    pub fn finish(&self) {
        if let Ok(mut state) = self.state.lock()
            && state.drawn.take().is_some()
        {
            let _ = write!(std::io::stderr(), "\r\x1b[2K");
        }
    }

    /// Names the hash being checked as Git's blob hash.
    pub fn git_hash(mut self, git: bool) -> Self {
        self.hash = if git { "git hash" } else { "sha256" };
        self
    }

    pub fn mirrors(mut self, mirrors: usize) -> Self {
        self.mirrors = mirrors;
        self
    }

    /// A line of its own above the progress line.
    fn say(&self, state: &mut State, message: &str) {
        if self.quiet {
            return;
        }
        let mut err = std::io::stderr().lock();
        if self.tty && state.drawn.take().is_some() {
            let _ = write!(err, "\r\x1b[2K");
        }
        let _ = writeln!(err, "{message}");
    }

    fn line(&self, state: &State) -> String {
        let host = &state.host;
        let label = &self.label;
        let percent = |done: u64| {
            state
                .total
                .filter(|total| *total > 0)
                .map(|total| format!("{:5.1}%  ", done as f64 * 100.0 / total as f64))
                .unwrap_or_default()
        };
        match state.phase {
            Some(Phase::Connect) => format!("{label}: connecting to {host}..."),
            Some(Phase::Retry) => format!("{label}: reconnecting to {host}..."),
            Some(Phase::Verify) => {
                format!(
                    "{label}: checking {}  {}",
                    self.hash,
                    percent(state.position)
                )
            }
            Some(Phase::Download) | None => {
                let done = human_bytes(state.position);
                let of = state
                    .total
                    .map(|total| format!(" / {}", human_bytes(total)))
                    .unwrap_or_default();
                let seconds = state.started.elapsed().as_secs_f64();
                let speed = if seconds > 0.5 {
                    let rate = state.position.saturating_sub(state.base) as f64 / seconds;
                    format!("  {}/s", human_bytes(rate as u64))
                } else {
                    String::new()
                };
                format!(
                    "{label}  {}{done}{of}{speed}  {host}",
                    percent(state.position)
                )
            }
        }
    }

    fn draw(&self, state: &mut State, force: bool) {
        if self.quiet || !self.tty {
            return;
        }
        let now = Instant::now();
        if !force && state.drawn.is_some_and(|at| now - at < REDRAW) {
            return;
        }
        let width = std::env::var("COLUMNS")
            .ok()
            .and_then(|columns| columns.parse::<usize>().ok())
            .unwrap_or(80)
            .max(20);
        let line: String = self.line(state).chars().take(width - 1).collect();
        let _ = write!(std::io::stderr().lock(), "\r\x1b[2K{line}");
        state.drawn = Some(now);
    }
}

impl Progress for Printer {
    fn event(&self, event: Event<'_>) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        match event {
            Event::Phase { phase, host } => {
                state.phase = Some(phase);
                // Bytes kept from before are announced as a mirror is first
                // connected; a retry's are covered by "interrupted".
                if let Some(bytes) = state.resumed.take()
                    && phase == Phase::Connect
                {
                    let message = format!("{}: resuming at {}", self.label, human_bytes(bytes));
                    self.say(&mut state, &message);
                }
                if let Some(host) = host {
                    state.host = host.name();
                }
                match phase {
                    Phase::Download => {
                        state.base = state.position;
                        state.started = Instant::now();
                    }
                    Phase::Verify => {
                        state.position = 0;
                        state.total = None;
                    }
                    Phase::Connect | Phase::Retry => {}
                }
                if self.tty {
                    self.draw(&mut state, true);
                } else if !self.quiet && phase != Phase::Retry {
                    let line = self.line(&state);
                    let line = match phase {
                        Phase::Download => {
                            format!("{}: downloading from {}", self.label, state.host)
                        }
                        Phase::Verify => format!("{}: checking {}", self.label, self.hash),
                        _ => line,
                    };
                    self.say(&mut state, &line);
                }
            }
            Event::Resumed { bytes } => {
                state.position = bytes;
                state.base = bytes;
                state.resumed = (bytes > 0).then_some(bytes);
                KEPT.store(bytes, Ordering::Relaxed);
            }
            Event::Position { bytes } => {
                state.position = bytes;
                if state.phase == Some(Phase::Download) {
                    KEPT.store(bytes, Ordering::Relaxed);
                }
                self.draw(&mut state, false);
            }
            Event::Request { .. } | Event::Completed { .. } => {}
            Event::Interrupted {
                host,
                received,
                failure,
            } => {
                let message = format!(
                    "{}: {} interrupted at {} ({failure}); resuming",
                    self.label,
                    host.name(),
                    human_bytes(received)
                );
                self.say(&mut state, &message);
            }
            Event::HostFailed { host, failure } => {
                state.failed_mirrors += 1;
                // After the last, the error lists every mirror's failure.
                if state.failed_mirrors < self.mirrors {
                    let message = format!(
                        "{}: {} failed ({failure}); trying the next mirror",
                        self.label,
                        host.name()
                    );
                    self.say(&mut state, &message);
                }
            }
            Event::ChecksumMismatch { expected, actual } => {
                let kind = if expected.len() == 64 {
                    "sha256"
                } else {
                    "git hash"
                };
                let message = format!(
                    "{}: {kind} mismatch: expected {expected}, got {actual}",
                    self.label
                );
                self.say(&mut state, &message);
            }
            Event::Aria2Unavailable { reason } => {
                let message = format!("aria2c unavailable ({reason}); using the built-in client");
                self.say(&mut state, &message);
            }
            Event::ManifestFailed { error } => {
                let message = format!("{}: could not write the manifest: {error}", self.label);
                self.say(&mut state, &message);
            }
        }
    }

    fn length(&self, bytes: u64) {
        if let Ok(mut state) = self.state.lock() {
            state.total = Some(bytes);
        }
    }
}

/// `1.5 MiB`.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["KiB", "MiB", "GiB", "TiB", "PiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::human_bytes;

    #[test]
    fn sizes_read_as_binary_units() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(5 * 1024 * 1024 * 1024), "5.0 GiB");
    }
}
