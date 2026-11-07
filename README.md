# model-fetch

Resumable, verified downloads of any HTTP(S) file from an ordered list of mirrors,
as a Rust library and a small command-line tool. A thin Hugging Face layer turns a
repository, revision and file into mirror URLs across huggingface.co, hf-mirror.com
and your own endpoint.

- **Resumes**: a `.part` file keeps what arrived; HTTP ranges or aria2 continue from
  it, in the same run after a dropped connection or in the next run after Ctrl-C.
  Only bytes known to be the same file are continued: the next run sends the ETag
  or Last-Modified it started with as `If-Range` (a changed file comes back whole),
  and without a hash a partial file never passes from one mirror to another.
- **Verifies**: give a size and a SHA-256 and the bytes must match before they
  replace anything. Both are optional. With a hash, a resumed file that fails it is
  fetched once more from the start.
- **One at a time**: a lock beside the `.part` file makes a second download of the
  same destination fail at once instead of interleaving bytes. On a file system
  without locks (some network mounts) it goes ahead without one.
- **Falls through**: each mirror is tried in order. A missing file, a stalled
  server (no data for a minute) or bytes with the wrong hash move on to the next.
- **Uses aria2 if you say so**: pass its path for several connections per server;
  if it can't run, the built-in client takes over.
- **Follows the proxy**: `ALL_PROXY`, `HTTPS_PROXY` or `HTTP_PROXY` (and lower-case
  forms), with `NO_PROXY`. `localhost`, `127.0.0.1` and `::1` always go direct.
- **Stays small**: blocking I/O on [ureq](https://crates.io/crates/ureq); no async
  runtime. The CLI's dependencies are behind a feature, so the library never
  pulls them in.

## Command line

```sh
cargo install --git https://github.com/mizorewww/model-fetch --features cli
```

### `get`: any URL

```sh
model-fetch get https://example.org/files/model.gguf \
    --mirror https://mirror.example.net/model.gguf \
    --sha256 b2ad8199aa54564790f81ff19f9736e0f6fa5fe928a66dbfab00f1756e3efb11
```

```text
model.gguf: connecting to example.org...
model.gguf: example.org failed (HTTP 404: not here); trying the next mirror
model.gguf: connecting to mirror.example.net...
model.gguf: downloading from mirror.example.net
model.gguf: checking sha256
saved model.gguf (2.9 MiB, sha256 verified)
```

| Option | |
|---|---|
| `--mirror URL` | Another URL for the same file, tried after the first; repeatable |
| `-o PATH` | A file, or an existing directory. Default: the URL's file name |
| `--sha256 HEX` | Expected SHA-256. A mirror whose bytes differ is skipped. With it, a file that is already there and matches is not downloaded again |
| `--size N` | Expected size in bytes |
| `--aria2 PATH\|auto` | Use aria2c; `auto` looks for it on `PATH` |
| `--connections N` | Connections per server for aria2c, 1 to 16 (default 8) |
| `-q`, `--quiet` | Print only errors |

### `hf`: files from a Hugging Face repository

```sh
model-fetch hf ggml-org/Qwen3-ASR-1.7B-GGUF Qwen3-ASR-1.7B-Q8_0.gguf -o models
model-fetch hf org/model config.json onnx/model.onnx --revision v1.0 -o models
```

Hosts are tried in order: the `--endpoint URL` values if any are given (repeatable);
otherwise `HF_ENDPOINT` if set, then `https://huggingface.co`, then
`https://hf-mirror.com`. Each file keeps its path under `-o DIR` (default `.`).
`--aria2`, `--connections` and `--quiet` work as for `get`.

Each file is checked against what the Hub says about it: the SHA-256 and size of a
file in LFS, the Git blob hash and size of any other. It is downloaded at the commit
the revision names, so a branch that moves meanwhile cannot mix two versions, and a
file already there that matches is not downloaded again. If no host gives that
information, the program says so and the download is unverified.

```text
model.gguf: connecting to huggingface.co...
model.gguf: downloading from cas-bridge.xethub.hf.co
saved models/model.gguf (1.9 GiB, sha256 verified)
config.json: connecting to huggingface.co...
saved models/config.json (1.2 KiB, git hash verified)
```

`HF_TOKEN`, if set, is sent as a bearer token only to `huggingface.co` and `hf.co`
(and their subdomains) over HTTPS: never to hf-mirror.com or another endpoint, never
past a redirect to another origin, never to aria2 (those requests use the built-in
client), and never printed.

### `verify`

```sh
model-fetch verify models/model.gguf --sha256 b2ad8199aa54564790f81ff19f9736e0f6fa5fe928a66dbfab00f1756e3efb11
```

### Behaviour

Progress goes to stderr: a single line redrawn in place on a terminal, a plain line
per step otherwise. Ctrl-C stops the download and keeps `<file>.part`; run the same
command again to resume. A second Ctrl-C leaves at once.

| Exit status | |
|---|---|
| 0 | Done |
| 1 | Failed: every mirror failed, a hash did not match, or a file could not be written |
| 2 | Usage error |
| 130 | Cancelled |

## Library

```toml
[dependencies]
model-fetch = { git = "https://github.com/mizorewww/model-fetch" }
```

Not on crates.io; pin a Git revision with `rev = "..."`.

### Any URL

```rust,no_run
use model_fetch::{Downloader, Event, Progress, Target};
use std::path::Path;
use std::sync::atomic::AtomicBool;

struct Print;

impl Progress for Print {
    fn event(&self, event: Event<'_>) {
        if let Event::Position { bytes } = event {
            eprint!("\r{bytes} bytes");
        }
    }

    fn length(&self, bytes: u64) {
        eprintln!("{bytes} bytes in all");
    }
}

let target = Target::new([
    "https://example.org/files/data.tar.zst",
    "https://mirror.example.net/data.tar.zst",
])
.with_size(1_234_567)
.with_sha256("b2ad8199aa54564790f81ff19f9736e0f6fa5fe928a66dbfab00f1756e3efb11");

// Set it from another thread (a Ctrl-C handler, a Stop button) to stop;
// `data.tar.zst.part` keeps what arrived for the next call.
let cancel = AtomicBool::new(false);
Downloader::default()
    .with_aria2(Some("aria2c".into()))
    .with_connections(8)
    .download(&target, Path::new("data/data.tar.zst"), &cancel, &Print)?;
# Ok::<(), model_fetch::Error>(())
```

`Target` holds `Mirror`s; `Mirror::new(url).with_bearer(token)` sends a token to
that URL's origin only, never past a redirect that leaves it. Without a size, the
server's length is trusted; without a hash, the bytes are not checked.
`file_matches` tells whether a finished file is a target's. A second download of the
same destination while one runs fails with `Error::Io` of kind `WouldBlock`. `Error::AllHostsFailed` lists each mirror's `Failure`;
`Event`s report phases, retries, failed mirrors and completion. The crate has no
user-facing text: wording them is the application's job.

### Hugging Face

```rust,no_run
use model_fetch::{Downloader, Host, HubFile, MIRROR, ORIGIN};
use std::path::Path;

let hosts = [Host::Hub(ORIGIN.into()), Host::Hub(MIRROR.into())];
let token = std::env::var("HF_TOKEN").ok();
let file = HubFile::new("org/model", "onnx/model.onnx").at("v1.0");
// The token goes only to hosts that are Hugging Face itself.
// A repository, revision or file with a `..` segment is refused.
let mirrors = file.mirrors(&hosts, token.as_deref()).expect("a plain path");
let downloader = Downloader::default();
// The Hub's size and SHA-256 (or Git blob hash), and the commit to fetch at.
let metadata = downloader.hub_metadata(&mirrors)?;
let pinned = match &metadata.commit {
    Some(commit) => file.at(commit),
    None => file,
};
let target = metadata.apply(pinned.target(&hosts, token.as_deref()).expect("a plain path"));
downloader.download(&target, Path::new("models/model.onnx"), &(), &())?;
# Ok::<(), model_fetch::Error>(())
```

`Downloader::fetch` keeps the original model-file API: a `Remote` with its
published size and SHA-256, tried across the downloader's own hosts at `main`, with
`model.part` beside the file and `model.manifest.json` written after it.

```rust,no_run
use model_fetch::{Downloader, Endpoints, Remote};
use std::path::Path;

let remote = Remote {
    repo: "org/model-GGUF",
    file: "model-Q8_0.gguf",
    size: 1_234_567,
    sha256: "b2ad8199aa54564790f81ff19f9736e0f6fa5fe928a66dbfab00f1756e3efb11",
};
let hosts = Endpoints::choose(std::env::var("HF_ENDPOINT").ok(), None).hosts;
Downloader::new(hosts, None).fetch(&remote, Path::new("models/model.gguf"), &(), &())?;
# Ok::<(), model_fetch::Error>(())
```

### Choosing hosts

`Endpoints::choose(configured, country)` takes both as values. A configured endpoint
is the only host. With `Some("CN")` it tries [ModelScope](https://modelscope.cn) and
[hf-mirror.com](https://hf-mirror.com) before Hugging Face; otherwise Hugging Face
first. The others stay as fallbacks, so a wrong guess costs one failed connection.
`local_country` and `system_time_zone` read the country from the machine's own
settings without sending anything.

`lookup_country`, behind the `ip-lookup` feature, asks Cloudflare, ipip.net and
ipinfo.io for the country. That tells all three this machine's address. It is off by
default and nothing calls it for you; call it only when the person running your
program chose it.

### Proxies

The library reads no environment variable of its own beyond the proxy ones. aria2 is
given the same route in a private config file; it speaks only to HTTP proxies, so
with a SOCKS or HTTPS proxy the built-in client takes over.

Rust 1.89 or later.

## License

[MIT](LICENSE)
