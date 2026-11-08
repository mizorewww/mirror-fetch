//! Its own test binary, because it sets the proxy variables for the whole process.

mod common;

use common::{Body, Server, sha256};
use mirror_fetch::{Downloader, Target};

/// With every proxy variable pointing at nothing, a loopback mirror is still
/// reached directly.
#[test]
fn loopback_bypasses_the_environment_proxy() {
    for name in [
        "ALL_PROXY",
        "HTTPS_PROXY",
        "HTTP_PROXY",
        "all_proxy",
        "http_proxy",
    ] {
        // SAFETY: this binary's only test, before any other thread reads the environment.
        unsafe { std::env::set_var(name, "http://127.0.0.1:1") };
    }
    for name in ["NO_PROXY", "no_proxy"] {
        // SAFETY: as above.
        unsafe { std::env::remove_var(name) };
    }
    let server = Server::start();
    server.serve("/f", Body::bytes(b"direct"));
    let root = tempfile::tempdir().unwrap();
    let dest = root.path().join("f");
    let target = Target::new([server.url("/f")]).with_sha256(sha256(b"direct"));
    Downloader::default()
        .download(&target, &dest, &(), &())
        .unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), b"direct");
    assert!(server.requests().iter().all(|r| !r.via_proxy));
}
