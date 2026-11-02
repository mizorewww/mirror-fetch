//! The proxy the environment names. Downloads go through it; this machine's own
//! services never do.

use ureq::{Proxy, ProxyProtocol};

/// Hosts that are always this machine.
const LOOPBACK: [&str; 4] = ["localhost", "127.0.0.1", "::1", "[::1]"];

/// Proxy variables aria2 would otherwise read itself, by rules of its own.
pub(crate) const ARIA2_ENVIRONMENT: [&str; 10] = [
    "ALL_PROXY",
    "all_proxy",
    "HTTPS_PROXY",
    "https_proxy",
    "HTTP_PROXY",
    "http_proxy",
    "FTP_PROXY",
    "ftp_proxy",
    "NO_PROXY",
    "no_proxy",
];

/// Which requests go through a proxy, and which one.
#[derive(Debug, Clone)]
pub(crate) struct Route {
    proxy: Option<Proxy>,
    /// `NO_PROXY`'s entries, for aria2; `proxy` already carries them.
    no_proxy: Vec<String>,
}

/// No proxy, for tests.
#[cfg(test)]
pub(crate) static DIRECT: Route = Route {
    proxy: None,
    no_proxy: Vec::new(),
};

impl Route {
    /// The proxy `ALL_PROXY`, `HTTPS_PROXY` or `HTTP_PROXY` names (or their lower-case
    /// forms), with `NO_PROXY`, for networks that reach the internet only through one.
    pub fn from_environment() -> Self {
        let no_proxy = ["NO_PROXY", "no_proxy"]
            .into_iter()
            .find_map(|name| std::env::var(name).ok());
        Self::new(Proxy::try_from_env(), no_proxy.as_deref())
    }

    /// `proxy`, except for loopback and the hosts `no_proxy` lists (ureq's own reading
    /// of the environment would send loopback through the proxy too). No proxy at all
    /// when `no_proxy` is `*`.
    pub fn new(proxy: Option<Proxy>, no_proxy: Option<&str>) -> Self {
        let no_proxy: Vec<String> = no_proxy
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .map(str::to_owned)
            .collect();
        let proxy = proxy
            .filter(|_| !no_proxy.iter().any(|entry| entry == "*"))
            .and_then(|proxy| bypassing(&proxy, &no_proxy));
        Self { proxy, no_proxy }
    }

    /// The proxy for a ureq agent.
    pub fn proxy(&self) -> Option<Proxy> {
        self.proxy.clone()
    }

    /// aria2's configuration lines that route `url` the same way, or why aria2 cannot:
    /// it speaks only to HTTP proxies. Empty when there is no proxy.
    pub fn aria2_options(&self, url: &str) -> Result<Vec<String>, String> {
        let Some(proxy) = &self.proxy else {
            return Ok(Vec::new());
        };
        let direct = url.parse().is_ok_and(|uri| proxy.is_no_proxy(&uri));
        if proxy.protocol() != ProxyProtocol::Http {
            return if direct {
                Ok(Vec::new())
            } else {
                Err(format!("aria2 cannot use a {:?} proxy", proxy.protocol()))
            };
        }
        let mut lines = vec![format!(
            "all-proxy=http://{}:{}",
            proxy.host(),
            proxy.port()
        )];
        if let Some(username) = proxy.username() {
            lines.push(format!("all-proxy-user={username}"));
        }
        if let Some(password) = proxy.password() {
            lines.push(format!("all-proxy-passwd={password}"));
        }
        let hosts: Vec<&str> = LOOPBACK
            .into_iter()
            .filter(|host| !host.starts_with('['))
            .chain(self.no_proxy.iter().filter_map(|entry| aria2_domain(entry)))
            .collect();
        lines.push(format!("no-proxy={}", hosts.join(",")));
        Ok(lines)
    }
}

/// `proxy`, leaving loopback and `no_proxy` direct.
fn bypassing(proxy: &Proxy, no_proxy: &[String]) -> Option<Proxy> {
    let mut builder = Proxy::builder(proxy.protocol())
        .host(proxy.host())
        .port(proxy.port())
        .resolve_target(proxy.resolve_target());
    if let Some(username) = proxy.username() {
        builder = builder.username(username);
    }
    if let Some(password) = proxy.password() {
        builder = builder.password(password);
    }
    for entry in LOOPBACK
        .into_iter()
        .chain(no_proxy.iter().map(String::as_str))
    {
        builder = builder.no_proxy(entry);
    }
    builder.build().ok()
}

/// A `NO_PROXY` entry as aria2 reads one, if it can: aria2 takes a leading dot as a
/// domain suffix and has no form for a prefix.
fn aria2_domain(entry: &str) -> Option<&str> {
    let entry = entry.strip_prefix('*').unwrap_or(entry);
    (!entry.is_empty() && !entry.ends_with('*') && !entry.ends_with('.')).then_some(entry)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(proxy: &str, no_proxy: Option<&str>) -> Route {
        Route::new(Some(Proxy::new(proxy).unwrap()), no_proxy)
    }

    /// A proxy from the environment carries remote hosts, never loopback ones, and
    /// leaves out what NO_PROXY lists.
    #[test]
    fn an_environment_proxy_leaves_loopback_and_no_proxy_hosts_direct() {
        let route = route(
            "http://user:secret@proxy.example:3128",
            Some("internal.example, .corp"),
        );
        let proxy = route.proxy().unwrap();
        assert_eq!((proxy.host(), proxy.port()), ("proxy.example", 3128));
        assert_eq!(proxy.username(), Some("user"));
        assert_eq!(proxy.password(), Some("secret"));
        let direct = |url: &str| proxy.is_no_proxy(&url.parse().unwrap());
        assert!(direct("http://127.0.0.1:8080/model"));
        assert!(direct("http://localhost:8080/model"));
        assert!(direct("http://[::1]:8080/model"));
        assert!(direct("https://internal.example/model"));
        assert!(direct("https://mirror.corp/model"));
        assert!(!direct("https://huggingface.co/org/repo/resolve/main/f"));
        assert!(!direct("https://www.modelscope.cn/models/org/repo"));
    }

    #[test]
    fn no_proxy_star_turns_the_proxy_off() {
        assert!(
            route("http://proxy.example:3128", Some("*"))
                .proxy()
                .is_none()
        );
        assert!(
            Route::new(None, None)
                .aria2_options("https://hf.co/f")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn aria2_is_told_the_same_route() {
        let route = route(
            "http://user:secret@proxy.example:3128",
            Some("internal.example,*.corp,10.*"),
        );
        assert_eq!(
            route.aria2_options("https://huggingface.co/f").unwrap(),
            [
                "all-proxy=http://proxy.example:3128",
                "all-proxy-user=user",
                "all-proxy-passwd=secret",
                "no-proxy=localhost,127.0.0.1,::1,internal.example,.corp",
            ]
        );
    }

    /// aria2 speaks only to HTTP proxies, so the built-in client takes over, unless
    /// the request would not use the proxy anyway.
    #[test]
    fn aria2_cannot_use_a_socks_proxy() {
        let route = route("socks5://proxy.example:1080", None);
        assert!(route.aria2_options("https://huggingface.co/f").is_err());
        assert!(
            route
                .aria2_options("http://127.0.0.1:9/f")
                .unwrap()
                .is_empty()
        );
    }
}
