//! The Hugging Face layer over the general core, against loopback hubs.

mod common;

use common::{Body, Server, sha256};
use model_fetch::{Downloader, Host, HubFile};

/// The first hub lacks the revision; the second has it. A token is not given to
/// either, because neither is Hugging Face.
#[test]
fn a_hub_file_falls_through_hubs_at_its_revision() {
    let data = b"weights";
    let missing = Server::start();
    let hub = Server::start();
    hub.serve(
        "/org/model/resolve/refs%2Fpr%2F3/onnx/model.onnx",
        Body::bytes(data),
    );
    let hosts = [Host::Hub(missing.base()), Host::Hub(hub.base())];
    let file = HubFile::new("org/model", "onnx/model.onnx").at("refs/pr/3");
    let target = file
        .target(&hosts, Some("hf_secret"))
        .unwrap()
        .with_size(data.len() as u64)
        .with_sha256(sha256(data));
    let root = tempfile::tempdir().unwrap();
    let dest = root.path().join(file.file);
    Downloader::default()
        .download(&target, &dest, &(), &())
        .unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), data);
    assert_eq!(
        missing.requests()[0].path,
        "/org/model/resolve/refs%2Fpr%2F3/onnx/model.onnx"
    );
    for request in missing.requests().iter().chain(&hub.requests()) {
        assert_eq!(request.authorization, None);
    }
}

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

/// A file in LFS: the hub redirects to a CDN on another origin, and its own
/// redirect carries the size, SHA-256 and commit.
#[test]
fn metadata_of_an_lfs_file_comes_from_the_hubs_redirect() {
    let data = common::payload(50_000);
    let cdn = Server::start();
    cdn.serve("/blob", Body::bytes(&data));
    let hub = Server::start();
    hub.serve(
        "/org/model/resolve/main/w.bin",
        Body::redirect(cdn.url("/blob"))
            .with_header("X-Linked-Size", &data.len().to_string())
            .with_header("X-Linked-Etag", &format!("\"{}\"", sha256(&data)))
            .with_header("X-Repo-Commit", COMMIT),
    );
    let hosts = [Host::Hub(hub.base())];
    let file = HubFile::new("org/model", "w.bin");
    let downloader = Downloader::default();
    let metadata = downloader
        .hub_metadata(&file.mirrors(&hosts, None).unwrap())
        .unwrap();
    assert_eq!(metadata.size, Some(data.len() as u64));
    assert_eq!(metadata.sha256.as_deref(), Some(sha256(&data).as_str()));
    assert_eq!(metadata.git_sha1, None);
    assert_eq!(metadata.commit.as_deref(), Some(COMMIT));
    assert!(cdn.requests().is_empty(), "the CDN is not asked");
    assert_eq!(hub.requests()[0].method, "HEAD");
}

/// A file kept in Git: the hub answers itself, with its blob hash as ETag; the
/// download is checked against it.
#[test]
fn a_git_file_is_checked_against_its_blob_hash() {
    let data = b"{\"model_type\": \"test\"}\n".to_vec();
    let hub = Server::start();
    hub.serve(
        "/org/model/resolve/main/config.json",
        Body::bytes(&data).with_etag(&format!("\"{}\"", common::git_sha1(&data))),
    );
    let hosts = [Host::Hub(hub.base())];
    let file = HubFile::new("org/model", "config.json");
    let downloader = Downloader::default();
    let metadata = downloader
        .hub_metadata(&file.mirrors(&hosts, None).unwrap())
        .unwrap();
    assert_eq!(metadata.git_sha1, Some(common::git_sha1(&data)));
    assert_eq!(metadata.size, Some(data.len() as u64));
    let target = metadata.apply(file.target(&hosts, None).unwrap());
    let root = tempfile::tempdir().unwrap();
    let dest = root.path().join("config.json");
    downloader.download(&target, &dest, &(), &()).unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), data);

    // The same hash over other bytes of the same length fails.
    let forged = Body::bytes(b"{\"model_type\": \"evil\"}\n")
        .with_etag(&format!("\"{}\"", common::git_sha1(&data)));
    hub.serve("/org/model/resolve/main/config.json", forged);
    let error = downloader
        .download(&target, &root.path().join("again.json"), &(), &())
        .unwrap_err();
    assert!(error.to_string().contains("checksum mismatch"), "{error}");
}

#[test]
fn no_metadata_is_an_error_naming_each_host() {
    let hub = Server::start();
    hub.serve(
        "/org/model/resolve/main/a",
        Body::bytes(b"a").without_head(),
    );
    let hosts = [Host::Hub(hub.base())];
    let mirrors = HubFile::new("org/model", "a")
        .mirrors(&hosts, None)
        .unwrap();
    let error = Downloader::default().hub_metadata(&mirrors).unwrap_err();
    assert!(error.to_string().contains("HTTP 405"), "{error}");
}
