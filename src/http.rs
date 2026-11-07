//! Requests with redirects followed by hand, so a token follows only same-origin
//! hops and a caller can stop at the first hop to another origin.

use crate::proxy::Route;
use std::time::Duration;

pub(crate) const USER_AGENT: &str = concat!("model-fetch/", env!("CARGO_PKG_VERSION"));

/// Hosts hand large files to a CDN; this many hops are followed.
const MAX_REDIRECTS: usize = 5;

/// An agent that returns redirects instead of following them, never re-encodes
/// bytes, and reports error statuses as responses.
pub(crate) fn agent(route: &Route, per_read: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .proxy(route.proxy())
        .max_redirects(0)
        .max_redirects_will_error(false)
        .accept_encoding("identity")
        .user_agent(USER_AGENT)
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_per_read(Some(per_read))
        .http_status_as_error(false)
        .build()
        .new_agent()
}

/// Which redirects to follow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Follow {
    All,
    /// Stop at the first redirect to another origin and return it.
    SameOrigin,
}

/// `GET` or `HEAD`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Method {
    Get,
    Head,
}

/// Same scheme, host and port.
pub(crate) fn same_origin(a: &url::Url, b: &url::Url) -> bool {
    a.scheme() == b.scheme()
        && a.host_str() == b.host_str()
        && a.port_or_known_default() == b.port_or_known_default()
}

/// Sends `headers` on every hop, and `bearer` only while each hop stays on the
/// first URL's origin: once a redirect leaves it, the token is gone for good.
pub(crate) fn send(
    agent: &ureq::Agent,
    method: Method,
    url: &str,
    headers: &[(&str, String)],
    bearer: Option<&str>,
    follow: Follow,
) -> Result<ureq::http::Response<ureq::Body>, String> {
    let mut current = url::Url::parse(url).map_err(|error| format!("{url}: {error}"))?;
    let mut bearer = bearer;
    for _ in 0..=MAX_REDIRECTS {
        let mut request = match method {
            Method::Get => agent.get(current.as_str()),
            Method::Head => agent.head(current.as_str()),
        };
        for (name, value) in headers {
            request = request.header(*name, value);
        }
        if let Some(token) = bearer {
            request = request.header("Authorization", &format!("Bearer {token}"));
        }
        let response = request.call().map_err(|error| error.to_string())?;
        let code = response.status().as_u16();
        let location = response
            .headers()
            .get("location")
            .and_then(|value| value.to_str().ok());
        let (true, Some(location)) = (matches!(code, 301 | 302 | 303 | 307 | 308), location) else {
            return Ok(response);
        };
        let next = current
            .join(location)
            .map_err(|error| format!("redirect to {location}: {error}"))?;
        if !same_origin(&current, &next) {
            if follow == Follow::SameOrigin {
                return Ok(response);
            }
            bearer = None;
        }
        current = next;
    }
    Err(format!("more than {MAX_REDIRECTS} redirects"))
}
