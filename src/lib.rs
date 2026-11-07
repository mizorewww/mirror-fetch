//! Resumable, verified downloads of any HTTP(S) file from an ordered list of
//! mirrors, with a thin Hugging Face layer on top.
//!
//! [`Downloader::download`] takes a [`Target`]: the [`Mirror`]s that serve one file,
//! tried in turn, and optionally its size and SHA-256. aria2 fetches over several
//! connections when the caller supplies its path; otherwise a built-in client
//! resumes a `.part` file with HTTP ranges. Either way a mirror that stops sending is
//! abandoned within a minute, and the bytes must match what is expected before
//! they replace anything.
//!
//! [`Downloader::fetch`] turns a [`Remote`] in a model repository into one mirror
//! per [`Host`] (Hugging Face, hf-mirror.com, ModelScope or a configured endpoint),
//! with its published size and hash.
//!
//! The policy belongs to the caller: which mirrors, in what order, where aria2 lives,
//! and whether to ask anyone for this machine's country. The crate reads no
//! environment variable of its own beyond the standard proxy ones (`ALL_PROXY`,
//! `HTTPS_PROXY`, `HTTP_PROXY`, `NO_PROXY`), which it follows for every host but
//! loopback, and has no user-facing text: it returns typed [`Event`]s and [`Error`]s
//! for the application to word.

mod aria2;
mod download;
mod error;
mod hub;
mod native;
mod proxy;

pub use download::{
    Cancel, DEFAULT_CONNECTIONS, Downloader, Event, MAX_CONNECTIONS, Mirror, Phase, Progress,
    Remote, Target, file_sha256, part_path,
};
pub use error::{Error, Failure, HostFailure};
#[cfg(feature = "ip-lookup")]
pub use hub::lookup_country;
pub use hub::{
    Endpoints, Host, MIRROR, ORIGIN, Reason, host_name, local_country, normalize_endpoint,
    system_time_zone,
};
