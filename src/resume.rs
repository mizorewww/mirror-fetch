//! When a partial file may be continued: only with bytes known to belong to the
//! same resource. A strong validator (an ETag that is not weak, else
//! Last-Modified) from the response that started the file is kept beside it in
//! `<part>.meta`; a later request sends it as `If-Range`, so a changed file
//! comes back whole (200) instead of as a range spliced onto old bytes.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// What identifies the bytes in a partial file, and where they came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Validator {
    pub url: String,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

impl Validator {
    /// From a response's headers: a strong ETag, or Last-Modified. `None` when the
    /// server gives neither.
    pub fn from_headers(url: &str, headers: &ureq::http::HeaderMap) -> Option<Self> {
        let text = |name: &str| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        };
        let etag = text("etag").filter(|tag| !tag.starts_with("W/"));
        let last_modified = text("last-modified");
        (etag.is_some() || last_modified.is_some()).then(|| Self {
            url: url.to_owned(),
            etag,
            last_modified,
        })
    }

    /// The `If-Range` value: the ETag if there is one.
    pub fn if_range(&self) -> &str {
        self.etag
            .as_deref()
            .or(self.last_modified.as_deref())
            .unwrap_or_default()
    }

    /// Whether `other` names the same bytes: equal ETags, or with none on either
    /// side, equal Last-Modified dates.
    pub fn same_resource(&self, other: &Self) -> bool {
        match (&self.etag, &other.etag) {
            (Some(a), Some(b)) => a == b,
            (None, None) => {
                self.last_modified.is_some() && self.last_modified == other.last_modified
            }
            _ => false,
        }
    }

    pub fn load(part: &Path) -> Option<Self> {
        let text = fs::read_to_string(meta_path(part)).ok()?;
        let value: serde_json::Value = serde_json::from_str(&text).ok()?;
        let field = |name: &str| value[name].as_str().map(str::to_owned);
        Some(Self {
            url: field("url")?,
            etag: field("etag"),
            last_modified: field("last_modified"),
        })
        .filter(|validator| validator.etag.is_some() || validator.last_modified.is_some())
    }

    /// Records this validator for `part`; a failure only costs a later resume.
    pub fn save(&self, part: &Path) {
        let value = serde_json::json!({
            "url": self.url,
            "etag": self.etag,
            "last_modified": self.last_modified,
        });
        let write = || -> std::io::Result<()> {
            let mut file = fs::File::create(meta_path(part))?;
            file.write_all(value.to_string().as_bytes())
        };
        if write().is_err() {
            let _ = fs::remove_file(meta_path(part));
        }
    }
}

/// `<part>.meta`.
pub(crate) fn meta_path(part: &Path) -> PathBuf {
    let mut name = part.as_os_str().to_owned();
    name.push(".meta");
    PathBuf::from(name)
}

/// Saves `validator` for `part`, or forgets any old one.
pub(crate) fn record(part: &Path, validator: Option<&Validator>) {
    match validator {
        Some(validator) => validator.save(part),
        None => forget(part),
    }
}

pub(crate) fn forget(part: &Path) {
    let _ = fs::remove_file(meta_path(part));
}

/// Removes a partial file and everything kept beside it: aria2's control file
/// and the validator.
pub(crate) fn discard(part: &Path) {
    let _ = fs::remove_file(part);
    let _ = fs::remove_file(crate::native::aria2_control(part));
    forget(part);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &'static str)]) -> ureq::http::HeaderMap {
        let mut map = ureq::http::HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, value.parse().unwrap());
        }
        map
    }

    #[test]
    fn only_strong_validators_count() {
        assert_eq!(
            Validator::from_headers("u", &headers(&[("etag", "W/\"x\"")])),
            None
        );
        let dated = Validator::from_headers(
            "u",
            &headers(&[
                ("etag", "W/\"x\""),
                ("last-modified", "Sat, 07 Nov 2026 10:00:00 GMT"),
            ]),
        )
        .unwrap();
        assert_eq!(dated.if_range(), "Sat, 07 Nov 2026 10:00:00 GMT");
        let tagged = Validator::from_headers("u", &headers(&[("etag", "\"abc\"")])).unwrap();
        assert_eq!(tagged.if_range(), "\"abc\"");
        assert!(!tagged.same_resource(&dated));
        assert!(tagged.same_resource(&tagged.clone()));
    }

    #[test]
    fn a_validator_round_trips_beside_its_part() {
        let root = tempfile::tempdir().unwrap();
        let part = root.path().join("f.part");
        let validator = Validator {
            url: "http://e.example/f".into(),
            etag: Some("\"1\"".into()),
            last_modified: None,
        };
        record(&part, Some(&validator));
        assert_eq!(Validator::load(&part), Some(validator));
        fs::write(&part, b"x").unwrap();
        discard(&part);
        assert!(!part.exists());
        assert_eq!(Validator::load(&part), None);
    }
}
