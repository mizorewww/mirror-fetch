//! What can go wrong, without wording it for a person's language.

use std::fmt;

/// Why [`Downloader::fetch`](crate::Downloader::fetch) stopped.
#[derive(Debug)]
pub enum Error {
    Cancelled,
    /// Preparing or replacing a local file failed.
    Io(std::io::Error),
    /// Every host was tried, in this order.
    AllHostsFailed(Vec<HostFailure>),
}

/// One host's failure.
#[derive(Debug)]
pub struct HostFailure {
    /// `huggingface.co`, as from [`Host::name`](crate::Host::name).
    pub host: String,
    pub failure: Failure,
}

/// Why one attempt on one host failed.
#[derive(Debug)]
pub enum Failure {
    /// The request itself failed; the text is the HTTP client's.
    Request(String),
    /// An error status, with the end of the response body, which usually names the cause.
    Status {
        code: u16,
        body: String,
    },
    /// The connection ended early.
    Incomplete {
        expected: u64,
        received: u64,
    },
    /// aria2 received nothing for a minute.
    Stalled,
    /// The right size, with the wrong bytes.
    ChecksumMismatch,
    /// aria2 reported this failure.
    Aria2 {
        code: String,
        message: String,
    },
    /// aria2 exited during the transfer; `stderr` holds the end of its output.
    Aria2Exited {
        status: String,
        stderr: String,
    },
    /// aria2 stopped answering its local control connection.
    Aria2Rpc(String),
    Io(std::io::Error),
}

impl Failure {
    /// Whether this host simply lacks the file, so trying it again is pointless:
    /// a 4xx status other than 429.
    pub fn is_permanent(&self) -> bool {
        matches!(self, Self::Status { code, .. } if is_permanent_status(*code))
    }
}

pub(crate) fn is_permanent_status(code: u16) -> bool {
    code != 429 && (400..500).contains(&code)
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("cancelled"),
            Self::Io(error) => error.fmt(f),
            Self::AllHostsFailed(failures) => {
                f.write_str("every download host failed")?;
                for failure in failures {
                    write!(f, "\n{}: {}", failure.host, failure.failure)?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Cancelled | Self::AllHostsFailed(_) => None,
        }
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Request(message) => write!(f, "request failed: {message}"),
            Self::Status { code, body } => write!(f, "HTTP {code}: {body}"),
            Self::Incomplete { expected, received } => {
                write!(
                    f,
                    "incomplete download (expected {expected} bytes, received {received})"
                )
            }
            Self::Stalled => f.write_str("no data for a minute"),
            Self::ChecksumMismatch => f.write_str("checksum mismatch"),
            Self::Aria2 { code, message } => write!(f, "aria2 {code}: {message}"),
            Self::Aria2Exited { status, stderr } => write!(f, "aria2c {status}: {stderr}"),
            Self::Aria2Rpc(message) => write!(f, "aria2 RPC: {message}"),
            Self::Io(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for Failure {}

impl From<std::io::Error> for Failure {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// How one attempt ended, inside the crate.
#[derive(Debug)]
pub(crate) enum Stop {
    Cancelled,
    Failed(Failure),
    /// A failure that retrying on the same host cannot fix; the built-in client
    /// turns it into `Failed` before it leaves.
    Final(Failure),
    /// aria2 could not run at all, as opposed to failing a transfer.
    Aria2Unavailable(String),
}

impl From<Failure> for Stop {
    fn from(failure: Failure) -> Self {
        Self::Failed(failure)
    }
}

impl From<std::io::Error> for Stop {
    fn from(error: std::io::Error) -> Self {
        Self::Failed(Failure::Io(error))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permanent_status_classification() {
        for code in [400, 401, 403, 404, 422] {
            assert!(is_permanent_status(code), "{code}");
        }
        for code in [429, 500, 502, 503] {
            assert!(!is_permanent_status(code), "{code}");
        }
    }

    #[test]
    fn every_failed_host_is_named() {
        let error = Error::AllHostsFailed(vec![HostFailure {
            host: "example.org".into(),
            failure: Failure::ChecksumMismatch,
        }]);
        assert_eq!(
            error.to_string(),
            "every download host failed\nexample.org: checksum mismatch"
        );
    }
}
