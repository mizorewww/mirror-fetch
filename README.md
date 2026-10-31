# model-fetch

Resumable, verified downloads of model files from Hugging Face and its mirrors.

- **Resumes**: a `.part` file keeps what arrived; HTTP ranges or aria2 continue from it.
- **Verifies**: bytes must match the published SHA-256 before they replace anything.
- **Falls through**: each host is tried in order; a stalled host is dropped within a minute.
- **Uses aria2 if you say so**: pass its path for parallel connections; if it can't
  run, the built-in client takes over.
- **Leaves policy to you**: hosts, aria2's location, cancellation and progress are all
  passed in. It reads no environment variable of its own and has no user-facing text:
  events and errors are typed.

```toml
[dependencies]
model-fetch = { git = "https://github.com/mizorewww/model-fetch" }
```

```rust,no_run
use model_fetch::{Downloader, Endpoints, Event, Progress, Remote};
use std::path::Path;

struct Print;

impl Progress for Print {
    fn event(&self, event: Event<'_>) {
        if let Event::Position { bytes } = event {
            println!("{bytes} bytes");
        }
    }
}

let remote = Remote {
    repo: "org/model-GGUF",
    file: "model-Q8_0.gguf",
    size: 1_234_567,
    sha256: "<published sha256>",
};
let hosts = Endpoints::choose(std::env::var("HF_ENDPOINT").ok(), None).hosts;
Downloader::new(hosts, None).fetch(&remote, Path::new("models/model.gguf"), &(), &Print)?;
# Ok::<(), model_fetch::Error>(())
```

## Choosing hosts

`Endpoints::choose(configured, country)` takes both as values. With `Some("CN")` it
tries [ModelScope](https://modelscope.cn) and [hf-mirror.com](https://hf-mirror.com)
before Hugging Face; otherwise Hugging Face first. The others stay as fallbacks, so a
wrong guess costs one failed connection. `local_country` and `system_time_zone` read
that from the machine's own settings without sending anything.

`lookup_country`, behind the `ip-lookup` feature, asks Cloudflare, ipip.net and ipinfo.io
for the country. That tells all three this machine's address. It is off by default and
nothing calls it for you; call it only when the person running your program chose it.

Not on crates.io yet; depend on it by Git revision.

## License

[MIT](LICENSE)
