//! Where model files come from: Hugging Face, or domestic copies for mainland China.
//!
//! hf-mirror.com proxies the Hub API but redirects large files to Hugging Face's
//! overseas CDN; ModelScope serves the same repositories, byte for byte, from a
//! mainland CDN. Direct file downloads therefore try ModelScope first there, while
//! libraries that speak the Hub API get the mirror.
//!
//! This module holds the mechanism. The caller decides what is configured, what the
//! country is and whether anyone is asked: [`Endpoints::choose`] takes both as values.

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
    /// The file at revision `main` (ModelScope's `master`). A [`Host::Url`] is
    /// already the file.
    pub fn file_url(&self, repo: &str, file: &str) -> String {
        match self {
            Self::Hub(base) => format!("{base}/{repo}/resolve/main/{file}"),
            Self::ModelScope => format!("{MODELSCOPE}/models/{repo}/resolve/master/{file}"),
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
}

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
    fn host_names_drop_scheme_and_path() {
        assert_eq!(host_name("https://hf-mirror.com/a/b.gguf"), "hf-mirror.com");
        assert_eq!(host_name(ORIGIN), "huggingface.co");
    }
}
