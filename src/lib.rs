//! Resumable, verified downloads of model files, from Hugging Face and its mirrors.
//!
//! Each [`Host`] is tried in turn. aria2 fetches over several connections when the
//! caller supplies its path; otherwise a built-in client resumes with HTTP ranges.
//! Either way a host that stops sending is abandoned within a minute, and the bytes
//! must match the published SHA-256 before they replace anything.
//!
//! The policy belongs to the caller: which hosts, in what order, where aria2 lives,
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

pub use download::{Cancel, Downloader, Event, Phase, Progress, Remote};
pub use error::{Error, Failure, HostFailure};
#[cfg(feature = "ip-lookup")]
pub use hub::lookup_country;
pub use hub::{
    Endpoints, Host, MIRROR, ORIGIN, Reason, host_name, local_country, normalize_endpoint,
    system_time_zone,
};
