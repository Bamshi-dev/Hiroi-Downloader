# Hiroi Downloader

Multithreaded download manager for Linux (and other operating systems in the future). This code will be written in Rust mostly. I will be using Agentic coding as I don't have enough time to maintain this project. The goal is for me to learn networks, security, Rust programming, parallelism, amd other topics. Hiroi
splits every file across several connections and keeps all of them busy
until the last byte. It can also pick a download up again after a pause, a
crash, or a reboot.

I will be modifying this code as I don't like vibe coding and I need to understand every single line. I still need to learn UI design, so I can't tell when I will be able to add a GUI. these texts are AI generated and I will be changing them slowly.
Thank you for your attention.

This repository holds the **engine** (a library a GUI or browser
integration can build on) and **`hiroi`**, a command-line front end.

## Features

- **Dynamic segmentation** — a file is split across up to 32
  connections. When a connection finishes early it takes over half of
  the largest range still in flight, so no connection sits idle.
- **Resume anywhere** — progress is flushed to disk and checkpointed
  every two seconds. Pausing, Ctrl+C, a crash, or a power cut lose at
  most the last moments of data.
- **Integrity first** — `If-Range` requests and strict `Content-Range`
  checks mean a file that changes on the server is never mixed with
  old data. Optional MD5/SHA-1/SHA-256/SHA-512 verification.
- **Mirrors** — download one file from several servers at once; broken
  or mismatching mirrors are dropped automatically.
- **Polite to servers** — per-host connection limits that halve when a
  server answers `429`/`503`, `Retry-After` honoured, exponential
  backoff with jitter, uncompressed transfers only (so byte offsets are
  exact), and one probe request instead of a burst.
- **Queue** — several downloads at once with priorities, pause, resume,
  and cancel per download.
- **Speed limits** — per download and global, adjustable while running.
- **Expired links** — signed URLs that expire mid-download are resolved
  again from the original link.
- **Safe on disk** — free space is checked and the full size reserved
  up front, part files are locked so two processes never write the same
  download, and an existing file is never overwritten unless asked.
- **Safe with secrets** — server-supplied file names can't leave the
  download folder, cookies and credentials are never written to disk,
  URLs are stripped of credentials and query strings in logs and
  errors, and HTTPS-to-HTTP redirects are refused.
- **Browser-ready requests** — custom headers, cookies, referer, user
  agent, and HTTP/SOCKS5 proxies.

## Requirements

- **Rust 1.89 or newer** (`rustup update stable`).
- **Linux.** Other Unix systems should work; Windows is not tested yet.
- A C compiler (`gcc` or `clang`) for the TLS library.

## Build

From the repo root:

```sh
cargo build --release
```

The binary is written to `target/release/hiroi`. To install it on your
`PATH`:

```sh
cargo install --path crates/hiroi-cli
```

From RustRover, open the repo root and use the green ▶ next to `main`
in `crates/hiroi-cli/src/main.rs`. Put the arguments (a URL and any
options) in the run configuration.

## Usage

```sh
hiroi get https://example.com/big.iso                      # 8 connections
hiroi get https://example.com/big.iso -c 16 -l 5M          # 16 connections, 5 MiB/s
hiroi get https://example.com/big.iso -m https://mirror.example.org/big.iso
hiroi get https://example.com/big.iso --checksum sha256:<hex>
hiroi get -i urls.txt -j 3 -d ~/Downloads                  # a list, 3 at a time
hiroi --help
```

Press **Ctrl+C** to pause. Run the same command again to resume.

While a download runs, two files sit next to it: `NAME.hiroi-part` (the
data) and `NAME.hiroi-state` (the resume information). Both are
replaced by the finished file.

| Option            | Meaning                                                |
|-------------------|--------------------------------------------------------|
| `-c, --connections` | Connections per download (1–32, default 8)          |
| `-j, --jobs`      | Downloads running at the same time (default 3)         |
| `--per-host`      | Most connections to one server (default 16)            |
| `-l, --limit`     | Speed limit per download (`500K`, `2M`, `1.5G`)        |
| `--global-limit`  | Speed limit for everything together                    |
| `-m, --mirror`    | Another server with the same file                      |
| `-H, --header`    | Extra header, e.g. `-H "Authorization: Bearer …"`      |
| `--cookie`, `--referer`, `--user-agent` | Request details, e.g. from a browser |
| `--proxy`         | `http://host:3128` or `socks5h://host:1080`            |
| `--on-conflict`   | `rename` (default), `overwrite`, or `skip`             |

## Using the engine

```rust
use std::sync::Arc;
use hiroi_engine::{DownloadListener, DownloadRequest, DownloadSnapshot, Engine, EngineConfig};

struct Printer;

impl DownloadListener for Printer {
    fn on_progress(&self, s: &DownloadSnapshot) {
        println!("{} {}/{:?} bytes at {} B/s", s.id, s.downloaded, s.total_size, s.speed);
    }
}

#[tokio::main]
async fn main() -> hiroi_engine::Result<()> {
    let engine = Engine::new(EngineConfig::default())?;
    engine.add_listener(Arc::new(Printer));

    let id = engine.add(DownloadRequest::new("https://example.com/big.iso")?.connections(16));
    println!("finished: {:?}", engine.wait(id).await);
    Ok(())
}
```

`Engine` also offers `pause`, `resume`, `cancel`, `set_speed_limit`,
`snapshot`, and `pause_all`. Every `DownloadSnapshot` includes the
segment layout, ready for an IDM-style segment bar.

## Project layout

| Path                                 | Contents                                         |
|--------------------------------------|--------------------------------------------------|
| `crates/hiroi-engine/src/engine.rs`  | Queue, scheduling, pause/resume/cancel           |
| `crates/hiroi-engine/src/task/`      | One download: segment table, connections, single-stream fallback |
| `crates/hiroi-engine/src/net/`       | HTTP client, probing, header parsing, retry policy, mirrors |
| `crates/hiroi-engine/src/limit/`     | Adaptive per-host limits, speed limiting         |
| `crates/hiroi-engine/src/storage/`   | Part file, control file, file naming             |
| `crates/hiroi-engine/tests/`         | End-to-end tests against a local test server     |
| `crates/hiroi-cli/`                  | The `hiroi` command                              |

## Tests

```sh
cargo test --workspace
```

The integration tests start a local HTTP server that throttles, drops
connections, rate-limits with `429`, changes files mid-download, and
sends hostile file names. CI runs `fmt`, `clippy`, and the tests on
every push.

## Roadmap

- Background service with a local API for the GUI and browser extension
- GUI
- Browser extension that hands downloads (with cookies) to Hiroi
- Scheduler (start and stop queues at set times)
- Saving the queue across restarts
- Video stream downloads (HLS/DASH)

## License

Copyright (C) 2026 Bamshi-dev

Hiroi Downloader is free software: you can redistribute it and/or modify
it under the terms of the GNU Affero General Public License as published
by the Free Software Foundation, either version 3 of the License, or (at
your option) any later version. See [LICENSE](LICENSE).

In short: you may use, study, change, and share Hiroi, but anything you
build on it and distribute, or offer to users over a network, must be
released under the same license with its full source code.
