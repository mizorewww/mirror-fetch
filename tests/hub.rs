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
