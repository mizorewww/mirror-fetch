//! Where model files come from: Hugging Face, or domestic copies for mainland China.
//!
//! hf-mirror.com proxies the Hub API but redirects large files to Hugging Face's
//! overseas CDN; ModelScope serves the same repositories, byte for byte, from a
//! mainland CDN. Direct file downloads therefore try ModelScope first there, while
//! libraries that speak the Hub API get the mirror.
//!
//! This module holds the mechanism. The caller decides what is configured, what the
//! country is and whether anyone is asked: [`Endpoints::choose`] takes both as values.

use crate::download::{Downloader, Mirror, Target};
use crate::error::{Error, Failure, HostFailure};
use crate::http::{self, Follow, Method};
use std::time::Duration;

pub const ORIGIN: &str = "https://huggingface.co";
pub const MIRROR: &str = "https://hf-mirror.com";
const MODELSCOPE: &str = "https://modelscope.cn";

/// A place that serves model files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Host {
    /// A Hub-compatible base URL: the origin, the mirror or a configured endpoint.
    Hub(String),
    ModelScope,
    /// One file's own URL, for a [`Mirror`](crate::Mirror) outside any hub.
    Url(String),
}

impl Host {
    /// The file at revision `main` (ModelScope's `master`).
    pub fn file_url(&self, repo: &str, file: &str) -> String {
        self.file_url_at(repo, "main", file)
    }

    /// The file at `revision`: a branch, tag or commit. A `/` in the revision is
    /// escaped (`refs/pr/1` → `refs%2Fpr%2F1`); the file's own `/` separate
    /// directories. A [`Host::Url`] is already the file and ignores all three.
    pub fn file_url_at(&self, repo: &str, revision: &str, file: &str) -> String {
        let repo = escape(repo, true);
        let file = escape(file, true);
        match self {
            Self::Hub(base) => format!("{base}/{repo}/resolve/{}/{file}", escape(revision, false)),
            Self::ModelScope => {
                let revision = if revision == "main" {
                    "master"
                } else {
                    revision
                };
                format!(
                    "{MODELSCOPE}/models/{repo}/resolve/{}/{file}",
                    escape(revision, false)
                )
            }
            Self::Url(url) => url.clone(),
        }
    }

    /// `huggingface.co`, `hf-mirror.com`, `modelscope.cn`: for progress and messages.
    pub fn name(&self) -> String {
        match self {
            Self::Hub(base) | Self::Url(base) => host_name(base),
            Self::ModelScope => host_name(MODELSCOPE),
        }
    }

    /// Whether this is Hugging Face itself, over HTTPS: `huggingface.co`, `hf.co`
    /// or a subdomain of either. Only these are given a Hugging Face token.
    pub fn is_hugging_face(&self) -> bool {
        let Self::Hub(base) = self else {
            return false;
        };
        let Ok(url) = url::Url::parse(base) else {
            return false;
        };
        let Some(host) = url.host_str() else {
            return false;
        };
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        url.scheme() == "https"
            && ["huggingface.co", "hf.co"]
                .iter()
                .any(|own| host == *own || host.ends_with(&format!(".{own}")))
    }
}

/// Percent-escapes everything but unreserved characters, and `/` when `slashes`.
/// A `.` or `..` segment is escaped too, so no URL can climb out of its path.
fn escape(text: &str, slashes: bool) -> String {
    if slashes {
        return text
            .split('/')
            .map(|segment| escape(segment, false))
            .collect::<Vec<_>>()
            .join("/");
    }
    if text == "." || text == ".." {
        return text.replace('.', "%2E");
    }
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            b'/' if slashes => out.push('/'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// A file in a Hugging Face (or ModelScope) repository, at a revision. It becomes
/// one [`Mirror`] per host; size and hash are added to the [`Target`] when known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HubFile<'a> {
    /// `org/name`.
    pub repo: &'a str,
    /// A branch, tag or commit; `main` unless set.
    pub revision: &'a str,
    /// The path inside the repository, `/`-separated.
    pub file: &'a str,
}

impl<'a> HubFile<'a> {
    pub fn new(repo: &'a str, file: &'a str) -> Self {
        Self {
            repo,
            revision: "main",
            file,
        }
    }

    pub fn at(mut self, revision: &'a str) -> Self {
        self.revision = revision;
        self
    }

    /// Whether the repository, revision and file are plain paths: no empty, `.`
    /// or `..` segment, no leading `/`, no `\`. The revision may contain `/`
    /// (`refs/pr/1`).
    pub fn validate(&self) -> Result<(), InvalidHubPath> {
        for (field, value) in [
            ("repository", self.repo),
            ("revision", self.revision),
            ("file", self.file),
        ] {
            let plain = !value.contains('\\')
                && value
                    .split('/')
                    .all(|segment| !segment.is_empty() && segment != "." && segment != "..");
            if !plain {
                return Err(InvalidHubPath {
                    field,
                    value: value.to_owned(),
                });
            }
        }
        Ok(())
    }

    /// One mirror per host, in order, once [`HubFile::validate`] passes. `token`
    /// goes only to hosts that [`Host::is_hugging_face`]; a third-party mirror
    /// never sees it.
    pub fn mirrors(
        &self,
        hosts: &[Host],
        token: Option<&str>,
    ) -> Result<Vec<Mirror>, InvalidHubPath> {
        self.validate()?;
        Ok(hosts
            .iter()
            .map(|host| {
                let mirror = Mirror::on(
                    host.clone(),
                    host.file_url_at(self.repo, self.revision, self.file),
                );
                match token {
                    Some(token) if host.is_hugging_face() => mirror.with_bearer(token),
                    _ => mirror,
                }
            })
            .collect())
    }

    /// [`HubFile::mirrors`] as a [`Target`], without size or hash.
    pub fn target(&self, hosts: &[Host], token: Option<&str>) -> Result<Target, InvalidHubPath> {
        Ok(Target {
            mirrors: self.mirrors(hosts, token)?,
            ..Target::default()
        })
    }
}

/// What a hub says about one file, from the headers of a `HEAD` on its URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HubMetadata {
    /// The host that answered.
    pub host: Host,
    /// `X-Linked-Size` for a file in LFS, else `Content-Length`.
    pub size: Option<u64>,
    /// `X-Linked-Etag` for a file in LFS: its SHA-256.
    pub sha256: Option<String>,
    /// The `ETag` of a file kept in Git itself: its blob hash.
    pub git_sha1: Option<String>,
    /// `X-Repo-Commit`: the commit the revision named. Downloading at it keeps
    /// the file from changing under a moving branch.
    pub commit: Option<String>,
}

impl HubMetadata {
    /// `target` with this size and hash.
    pub fn apply(&self, mut target: Target) -> Target {
        if let Some(size) = self.size {
            target = target.with_size(size);
        }
        if let Some(sha256) = &self.sha256 {
            target = target.with_sha256(sha256);
        }
        if let Some(sha1) = &self.git_sha1 {
            target = target.with_git_sha1(sha1);
        }
        target
    }

    fn from_headers(host: &Host, code: u16, headers: &ureq::http::HeaderMap) -> Option<Self> {
        let text = |name: &str| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(|value| {
                    value
                        .trim()
                        .trim_start_matches("W/")
                        .trim_matches('"')
                        .to_owned()
                })
                .filter(|value| !value.is_empty())
        };
        let hex = |value: String, len: usize| {
            (value.len() == len && value.bytes().all(|b| b.is_ascii_hexdigit()))
                .then(|| value.to_ascii_lowercase())
        };
        let commit = text("x-repo-commit").and_then(|commit| hex(commit, 40));
        let linked = text("x-linked-etag").and_then(|tag| hex(tag, 64));
        let (size, sha256, git_sha1) = if linked.is_some() {
            let size = text("x-linked-size").and_then(|size| size.parse().ok());
            (size, linked, None)
        } else if (200..300).contains(&code) {
            // Not in LFS: the hub answers for the file itself. Any server may send
            // a 40-digit ETag, so it is read as a blob hash only from a hub: one
            // that names the commit, or Hugging Face itself.
            let hub = commit.is_some() || host.is_hugging_face();
            let git_sha1 = text("etag").and_then(|tag| hex(tag, 40)).filter(|_| hub);
            // A length alone, or an empty one, is not taken as the file's: an error
            // page or a HEAD answered without a body says it as easily.
            let size = text("content-length")
                .and_then(|size| size.parse::<u64>().ok())
                .filter(|_| hub)
                .filter(|size| *size > 0 || git_sha1.is_some());
            (size, None, git_sha1)
        } else {
            (None, None, None)
        };
        (size.is_some() || sha256.is_some() || git_sha1.is_some()).then(|| Self {
            host: host.clone(),
            size,
            sha256,
            git_sha1,
            commit,
        })
    }
}

impl Downloader {
    /// Asks each of `mirrors` (from [`HubFile::mirrors`]) in turn for the file's
    /// size and hash: a `HEAD` that follows only redirects on its own origin, so
    /// the hub's own headers are read before any CDN's. A token is sent as for the
    /// download. [`Error::AllHostsFailed`] when no hub says anything useful.
    pub fn hub_metadata(&self, mirrors: &[Mirror]) -> Result<HubMetadata, Error> {
        let agent = http::agent(
            self.route(),
            Duration::from_secs(15),
            Some(Duration::from_secs(30)),
        );
        let mut failures = Vec::new();
        for mirror in mirrors {
            let failure = match http::send(
                &agent,
                Method::Head,
                mirror.url(),
                &[],
                mirror.bearer(),
                Follow::SameOrigin,
            ) {
                Ok(response) => {
                    let code = response.status().as_u16();
                    if let Some(found) =
                        HubMetadata::from_headers(mirror.host(), code, response.headers())
                    {
                        return Ok(found);
                    }
                    if code >= 400 {
                        Failure::Status {
                            code,
                            body: String::new(),
                        }
                    } else {
                        Failure::Request(format!("HTTP {code} without a size or hash"))
                    }
                }
                Err(message) => Failure::Request(match mirror.bearer() {
                    Some(token) => message.replace(token, "<redacted>"),
                    None => message,
                }),
            };
            failures.push(HostFailure {
                host: mirror.host().name(),
                failure,
            });
        }
        Err(Error::AllHostsFailed(failures))
    }
}

/// A [`HubFile`] part that could reach outside its repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidHubPath {
    /// `repository`, `revision` or `file`.
    pub field: &'static str,
    pub value: String,
}

impl std::fmt::Display for InvalidHubPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the {} '{}' has an empty, '.' or '..' segment or a backslash",
            self.field, self.value
        )
    }
}

impl std::error::Error for InvalidHubPath {}

/// Why [`Endpoints`] has its order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// An endpoint was configured; nothing else is tried.
    Configured,
    MainlandChina,
    Default,
}

/// The hosts to try, in order, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoints {
    pub hosts: Vec<Host>,
    pub reason: Reason,
}

impl Endpoints {
    /// A configured endpoint is the only host. Otherwise the country orders them and
    /// the rest remain fallbacks, so a wrong guess costs one failed connection, not
    /// the download.
    pub fn choose(configured: Option<String>, country: Option<&str>) -> Self {
        if let Some(endpoint) = normalize_endpoint(configured) {
            return Self {
                hosts: vec![Host::Hub(endpoint)],
                reason: Reason::Configured,
            };
        }
        let origin = Host::Hub(ORIGIN.into());
        let mirror = Host::Hub(MIRROR.into());
        if country == Some("CN") {
            Self {
                hosts: vec![Host::ModelScope, mirror, origin],
                reason: Reason::MainlandChina,
            }
        } else {
            Self {
                hosts: vec![origin, Host::ModelScope, mirror],
                reason: Reason::Default,
            }
        }
    }

    /// The Hub endpoint for libraries that take `HF_ENDPOINT`, when it should differ
    /// from what the environment already says.
    pub fn hub_override(&self) -> Option<&'static str> {
        (self.reason == Reason::MainlandChina).then_some(MIRROR)
    }
}

/// `CN` when this machine says it is set up for mainland China: a locale in that
/// region (`zh-CN`, `en-CN`, `zh_CN.UTF-8`), or a mainland time zone. Taiwan, Hong
/// Kong and Macau have their own regions and zones and keep the default order.
/// Nothing is sent anywhere to decide this.
pub fn local_country(locale: Option<&str>, time_zone: Option<&str>) -> Option<String> {
    let region = locale.is_some_and(|locale| {
        locale
            .split(['.', '@'])
            .next()
            .unwrap_or_default()
            .split(['-', '_'])
            .skip(1)
            .any(|subtag| subtag.eq_ignore_ascii_case("CN"))
    });
    let zone = time_zone.is_some_and(|zone| {
        // `/usr/share/zoneinfo/Asia/Shanghai`, `:Asia/Shanghai` or a bare name.
        let name = zone.rsplit("zoneinfo/").next().unwrap_or(zone);
        matches!(
            name.trim_start_matches(':').trim(),
            "Asia/Shanghai"
                | "Asia/Urumqi"
                | "Asia/Chongqing"
                | "Asia/Chungking"
                | "Asia/Harbin"
                | "Asia/Kashgar"
                | "PRC"
        )
    });
    (region || zone).then(|| "CN".to_owned())
}

/// The system's IANA time zone, where it can be read without another dependency:
/// `TZ`, then `/etc/localtime`'s link. Windows has neither.
pub fn system_time_zone() -> Option<String> {
    std::env::var("TZ")
        .ok()
        .filter(|zone| !zone.trim().is_empty())
        .or_else(|| {
            std::fs::read_link("/etc/localtime")
                .ok()
                .map(|target| target.to_string_lossy().into_owned())
        })
}

/// Trims whitespace and trailing slashes; blank means not configured.
pub fn normalize_endpoint(endpoint: Option<String>) -> Option<String> {
    endpoint
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
}

/// `https://hf-mirror.com/a/b` → `hf-mirror.com`, for progress and messages.
pub fn host_name(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .unwrap_or_else(|| url.to_owned())
}

#[cfg(feature = "ip-lookup")]
mod lookup {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    /// How long to wait for the country.
    const DEADLINE: Duration = Duration::from_secs(3);
    /// After a non-mainland answer, how long the other services may still say CN.
    /// Split-routing proxies send domestic services direct and foreign ones abroad.
    const GRACE: Duration = Duration::from_millis(800);

    type Parser = fn(&str) -> Option<String>;

    /// Services answer from different networks: Cloudflare and ipinfo abroad,
    /// ipip.net reliably from inside mainland China. The first answer wins.
    const PROBES: [(&str, Parser); 3] = [
        ("https://www.cloudflare.com/cdn-cgi/trace", parse_trace),
        ("https://myip.ipip.net/json", parse_ipip),
        ("https://ipinfo.io/country", parse_ipinfo),
    ];

    /// The ISO country code of this machine's public IP, or `None` when no service
    /// answered in time. Any mainland answer wins.
    ///
    /// This tells three public services this machine's address. Call it only when the
    /// person running the program has chosen that.
    pub fn lookup_country() -> Option<String> {
        let (sender, receiver) = mpsc::channel();
        for (url, parse) in PROBES {
            let sender = sender.clone();
            std::thread::spawn(move || {
                let agent = ureq::Agent::config_builder()
                    .proxy(crate::proxy::Route::from_environment().proxy())
                    .timeout_global(Some(DEADLINE))
                    .build()
                    .new_agent();
                let country = agent
                    .get(url)
                    .call()
                    .ok()
                    .and_then(|mut response| response.body_mut().read_to_string().ok())
                    .and_then(|body| parse(&body));
                let _ = sender.send(country);
            });
        }
        drop(sender);
        let mut deadline = Instant::now() + DEADLINE;
        let mut answer = None;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match receiver.recv_timeout(left) {
                Ok(Some(country)) if country == "CN" => return Some(country),
                Ok(Some(country)) => {
                    if answer.is_none() {
                        deadline = deadline.min(Instant::now() + GRACE);
                        answer = Some(country);
                    }
                }
                Ok(None) => continue,
                Err(_) => break,
            }
        }
        answer
    }

    fn country_code(value: &str) -> Option<String> {
        let value = value.trim();
        (value.len() == 2 && value.bytes().all(|b| b.is_ascii_alphabetic()))
            .then(|| value.to_ascii_uppercase())
    }

    pub(super) fn parse_trace(body: &str) -> Option<String> {
        body.lines()
            .find_map(|line| line.strip_prefix("loc="))
            .and_then(country_code)
    }

    pub(super) fn parse_ipinfo(body: &str) -> Option<String> {
        country_code(body)
    }

    /// The service answers in Chinese, and lists Hong Kong, Macau and Taiwan under
    /// China with the region second. The escapes spell the names it answers with.
    pub(super) fn parse_ipip(body: &str) -> Option<String> {
        const CHINA: &str = "\u{4e2d}\u{56fd}";
        const HONG_KONG: &str = "\u{9999}\u{6e2f}";
        const MACAU: &str = "\u{6fb3}\u{95e8}";
        const TAIWAN: &str = "\u{53f0}\u{6e7e}";
        let value: serde_json::Value = serde_json::from_str(body).ok()?;
        let location = value.pointer("/data/location")?.as_array()?;
        let country = location.first()?.as_str()?;
        let region = location.get(1).and_then(|v| v.as_str()).unwrap_or_default();
        Some(match (country, region) {
            (CHINA, HONG_KONG) => "HK".into(),
            (CHINA, MACAU) => "MO".into(),
            (CHINA, TAIWAN) => "TW".into(),
            (CHINA, _) => "CN".into(),
            _ => "ZZ".into(),
        })
    }
}

#[cfg(feature = "ip-lookup")]
pub use lookup::lookup_country;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_endpoint_is_the_only_host() {
        let chosen = Endpoints::choose(Some(" https://example.org/ ".into()), Some("CN"));
        assert_eq!(chosen.hosts, [Host::Hub("https://example.org".into())]);
        assert_eq!(chosen.hub_override(), None);
        assert_eq!(chosen.reason, Reason::Configured);
        assert_eq!(
            Endpoints::choose(Some("  ".into()), None).reason,
            Reason::Default
        );
    }

    #[test]
    fn mainland_china_prefers_domestic_hosts_and_keeps_the_origin() {
        let origin = Host::Hub(ORIGIN.into());
        let mirror = Host::Hub(MIRROR.into());
        let chosen = Endpoints::choose(None, Some("CN"));
        assert_eq!(
            chosen.hosts,
            [Host::ModelScope, mirror.clone(), origin.clone()]
        );
        assert_eq!(chosen.hub_override(), Some(MIRROR));
        for country in [None, Some("HK"), Some("US")] {
            let chosen = Endpoints::choose(None, country);
            assert_eq!(chosen.hosts[0], origin);
            assert_eq!(chosen.hub_override(), None);
        }
    }

    #[test]
    fn local_settings_alone_find_mainland_china() {
        for (locale, zone) in [
            (Some("zh-CN"), None),
            (Some("zh-Hans-CN"), None),
            (Some("zh_CN.UTF-8"), None),
            (Some("en-CN"), None),
            (Some("en-US"), Some("Asia/Shanghai")),
            (None, Some("/var/db/timezone/zoneinfo/Asia/Shanghai")),
            (None, Some("/usr/share/zoneinfo/Asia/Urumqi")),
            (None, Some(":PRC")),
        ] {
            assert_eq!(
                local_country(locale, zone).as_deref(),
                Some("CN"),
                "{locale:?} {zone:?}"
            );
        }
        for (locale, zone) in [
            (Some("zh-TW"), Some("Asia/Taipei")),
            (Some("zh-HK"), Some("Asia/Hong_Kong")),
            (Some("zh-Hant-MO"), Some("Asia/Macau")),
            (Some("zh"), Some("America/Vancouver")),
            (Some("en-US"), Some("UTC")),
            // Neither a language nor a word that merely contains the region.
            (Some("cn"), None),
            (Some("en_US.CN"), None),
            (None, Some("Not/Asia/Shanghai")),
            (None, None),
        ] {
            assert_eq!(local_country(locale, zone), None, "{locale:?} {zone:?}");
        }
    }

    #[test]
    fn file_urls_follow_each_host() {
        let repo = "ggml-org/Qwen3-ASR-1.7B-GGUF";
        assert_eq!(
            Host::Hub(MIRROR.into()).file_url(repo, "a.gguf"),
            "https://hf-mirror.com/ggml-org/Qwen3-ASR-1.7B-GGUF/resolve/main/a.gguf"
        );
        assert_eq!(
            Host::ModelScope.file_url(repo, "a.gguf"),
            "https://modelscope.cn/models/ggml-org/Qwen3-ASR-1.7B-GGUF/resolve/master/a.gguf"
        );
        assert_eq!(Host::ModelScope.name(), "modelscope.cn");
    }

    #[cfg(feature = "ip-lookup")]
    #[test]
    fn lookup_answers_parse() {
        use lookup::{parse_ipinfo, parse_ipip, parse_trace};
        assert_eq!(
            parse_trace("fl=1\nip=1.2.3.4\nloc=CN\ntls=TLSv1.3\n").as_deref(),
            Some("CN")
        );
        assert_eq!(parse_trace("ip=1.2.3.4\n"), None);
        assert_eq!(parse_ipinfo("us\n").as_deref(), Some("US"));
        assert_eq!(parse_ipinfo("<html>"), None);
        let ipip = |place: &str| {
            parse_ipip(&format!(
                r#"{{"ret":"ok","data":{{"ip":"1.2.3.4","location":[{place}]}}}}"#
            ))
        };
        assert_eq!(
            ipip(r#""中国","广东","深圳","","电信""#).as_deref(),
            Some("CN")
        );
        assert_eq!(ipip(r#""中国","香港","""#).as_deref(), Some("HK"));
        assert_eq!(ipip(r#""美国","加利福尼亚""#).as_deref(), Some("ZZ"));
    }

    #[test]
    fn revisions_and_paths_are_escaped() {
        let origin = Host::Hub(ORIGIN.into());
        assert_eq!(
            origin.file_url_at("org/repo", "refs/pr/1", "onnx/model q.onnx"),
            "https://huggingface.co/org/repo/resolve/refs%2Fpr%2F1/onnx/model%20q.onnx"
        );
        assert_eq!(
            Host::ModelScope.file_url_at("org/repo", "v1.0", "a.bin"),
            "https://modelscope.cn/models/org/repo/resolve/v1.0/a.bin"
        );
        let url = Host::Url("https://example.org/x?y=1".into());
        assert_eq!(url.file_url_at("a", "b", "c"), "https://example.org/x?y=1");
        assert_eq!(url.name(), "example.org");
    }

    fn headers(pairs: &[(&'static str, &str)]) -> ureq::http::HeaderMap {
        let mut map = ureq::http::HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, value.parse().unwrap());
        }
        map
    }

    /// A 40-digit ETag and a length are believed only from a hub: one naming
    /// the commit, or Hugging Face itself; an empty length needs the blob hash.
    #[test]
    fn metadata_is_believed_only_from_a_hub() {
        let sha1 = "a".repeat(40);
        let commit = "c".repeat(40);
        let plain = Host::Hub("http://127.0.0.1:9".into());
        let tagged = format!("\"{sha1}\"");
        let page = headers(&[("etag", &tagged), ("content-length", "5")]);
        assert_eq!(HubMetadata::from_headers(&plain, 200, &page), None);
        let hf = Host::Hub(ORIGIN.into());
        let found = HubMetadata::from_headers(&hf, 200, &page).unwrap();
        assert_eq!(
            (found.size, found.git_sha1.as_deref()),
            (Some(5), Some(sha1.as_str()))
        );
        let named = headers(&[
            ("etag", &tagged),
            ("content-length", "5"),
            ("x-repo-commit", &commit),
        ]);
        assert!(HubMetadata::from_headers(&plain, 200, &named).is_some());
        let empty = headers(&[("content-length", "0"), ("x-repo-commit", &commit)]);
        assert_eq!(HubMetadata::from_headers(&plain, 200, &empty), None);
        let empty_file = headers(&[
            ("etag", &tagged),
            ("content-length", "0"),
            ("x-repo-commit", &commit),
        ]);
        assert_eq!(
            HubMetadata::from_headers(&plain, 200, &empty_file)
                .unwrap()
                .size,
            Some(0)
        );
    }

    #[test]
    fn hub_paths_cannot_climb_out() {
        let hosts = [Host::Hub(ORIGIN.into())];
        for (repo, revision, file) in [
            ("org/repo", "main", "../other/x"),
            ("org/repo", "main", "a/./b"),
            ("org/repo", "main", "/abs"),
            ("org/repo", "main", "a\\b"),
            ("org/repo", "..", "a"),
            ("org/repo", "refs/../x", "a"),
            ("org/..", "main", "a"),
            ("", "main", "a"),
        ] {
            let file = HubFile::new(repo, file).at(revision);
            assert!(file.target(&hosts, None).is_err(), "{file:?}");
        }
        assert!(HubFile::new("gpt2", "config.json").validate().is_ok());
        // Even a URL built directly keeps dot segments as data.
        assert_eq!(
            Host::Hub(ORIGIN.into()).file_url_at("org/repo", "..", "../x"),
            "https://huggingface.co/org/repo/resolve/%2E%2E/%2E%2E/x"
        );
    }

    #[test]
    fn only_hugging_face_over_https_counts_as_hugging_face() {
        for base in [
            ORIGIN,
            "https://hf.co",
            "https://cdn-lfs.huggingface.co",
            "https://HuggingFace.co.",
        ] {
            assert!(Host::Hub(base.into()).is_hugging_face(), "{base}");
        }
        for base in [
            MIRROR,
            "http://huggingface.co",
            "https://huggingface.co.evil.example",
            "https://nothuggingface.co",
            "https://evil.example/huggingface.co",
        ] {
            assert!(!Host::Hub(base.into()).is_hugging_face(), "{base}");
        }
        assert!(!Host::ModelScope.is_hugging_face());
        assert!(!Host::Url(format!("{ORIGIN}/a/resolve/main/b")).is_hugging_face());
    }

    #[test]
    fn a_hub_file_gives_its_token_only_to_hugging_face() {
        let hosts = [Host::Hub(MIRROR.into()), Host::Hub(ORIGIN.into())];
        let file = HubFile::new("org/repo", "a.gguf").at("v2");
        let mirrors = file.mirrors(&hosts, Some("hf_secret")).unwrap();
        assert_eq!(
            mirrors[0].url(),
            "https://hf-mirror.com/org/repo/resolve/v2/a.gguf"
        );
        assert!(!mirrors[0].has_bearer());
        assert!(mirrors[1].has_bearer());
        assert_eq!(mirrors[1].host(), &hosts[1]);
        assert!(!format!("{mirrors:?}").contains("hf_secret"));
        let target = file.target(&hosts, None).unwrap();
        assert!(target.mirrors.iter().all(|mirror| !mirror.has_bearer()));
        assert_eq!((target.size, target.sha256), (None, None));
    }

    #[test]
    fn host_names_drop_scheme_and_path() {
        assert_eq!(host_name("https://hf-mirror.com/a/b.gguf"), "hf-mirror.com");
        assert_eq!(host_name(ORIGIN), "huggingface.co");
    }
}
