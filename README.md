# Hiroi Downloader

Multithreaded download manager for Linux (and other operating systems in the future). This code will be written in Rust mostly. I will be using Agentic coding as I don't have enough time to maintain this project. The goal is for me to learn networks, security, Rust programming, parallelism, amd other topics. Hiroi
splits every file across several connections and keeps all of them busy
until the last byte. It can also pick a download up again after a pause, a
crash, or a reboot.

I will be modifying this code as I don't like vibe coding and I need to understand every single line. I still need to learn UI design, so I can't tell when I will be able to add a GUI. these texts are AI generated and I will be changing them slowly.
Thank you for your attention.

This repository holds the **engine**, a library that a GUI or a
browser integration can build on, and **`hiroi`**, its command-line
front end, with a background service and a browser extension.

## What it does

Hiroi splits a file across as many as 32 connections, and when one
finishes early it takes over half of the largest piece still in
flight, so none sits idle until the last byte. It checks every answer
with `If-Range` and `Content-Range`, so a file that changes on the
server is never mixed with old data, and it saves its progress every
two seconds, after the data is on disk. A pause, a crash, or a power
cut costs a few seconds at most. One file can come from several
mirrors at once, and an MD5, SHA-1, SHA-256, or SHA-512 checksum can
verify the result.

It is polite to servers. Every server gets a limited number of
connections, which halves when the server answers `429` or `503` and
grows back slowly; `Retry-After` is honoured, retries back off with
jitter, and one probe request replaces a burst.

Beyond single files, `hiroi grab` lists what a web page offers (files,
pictures, videos, and streams) and downloads what you pick. HLS and
DASH streams are downloaded as the videos they describe, live or
recorded, and video sites go through yt-dlp. Metalink files are
followed, and FTP, FTPS, SFTP, and BitTorrent are optional features.

The background service keeps a queue in SQLite, runs queues on a
schedule, sorts downloads into folders by type, and accepts links from
the browser extension and the clipboard. Downloads added together form
a group, named after them when you give no name, and finished episodes
of a series (`talk 1`, `talk 2`, ...) gather in a folder of their own.

The browser extension adds right-click menus for links, pictures,
videos, selections, and whole pages, finds the media a tab plays, and
can take over the browser's own downloads.

`hiroi exposure` shows who would see a download and what it would give
away. Downloads can be bound to a VPN's interface, so they stop when
the VPN drops, kept away from the addresses of an IP filter, and kept
from being redirected into the local network; `--require-proxy`,
`--strip-tracking`, `--no-referer`, and `--anonymous` send less.
Cookies and credentials are never written to disk or to a log.

Every setting can be kept in a settings file, and `hiroi search` finds
a setting, a command, or an option by what it does. The
[guide](docs/README.md) explains every feature.

## Requirements

Rust 1.89 or newer (`rustup update stable`) and a C compiler (`gcc` or
`clang`) for the TLS library. Linux is the main platform. CI also
builds and tests macOS and Windows, where everything but the
background service works; the service needs a Unix socket for now.

ffmpeg joins separate video and audio tracks, and yt-dlp reads video
sites. Both are optional.

## Build

```sh
cargo build --release
cargo install --path crates/hiroi-cli          # puts `hiroi` on your PATH
```

The optional protocols are cargo features of `hiroi-cli`:

```sh
cargo build --release --features hiroi-cli/ftp,hiroi-cli/sftp,hiroi-cli/torrent
```

From RustRover, open the repository's root and use the green ▶ next to
`main` in `crates/hiroi-cli/src/main.rs`, with the arguments in the run
configuration.

## Usage

```sh
hiroi get https://example.com/big.iso                       # 8 connections
hiroi get https://example.com/big.iso -c 16 -l 5M           # 16 connections, 5 MiB/s
hiroi get https://example.com/big.iso -m https://mirror.example.org/big.iso
hiroi get https://example.com/live/index.m3u8 --live-limit 30m
hiroi get -x https://video.example/watch?v=abc              # through yt-dlp
hiroi grab https://example.com/gallery -t image --all       # everything on a page
hiroi exposure https://example.com/big.iso                  # who would see it

hiroi daemon &                                              # the background service
hiroi add https://example.com/big.iso --queue night
hiroi schedule add --queue night --from 01:00 --to 07:00
hiroi add URL1 URL2 URL3 --group "Lecture videos"
hiroi list

hiroi settings set interface wg0                            # downloads stop if the VPN drops
hiroi search proxy                                          # find a setting or an option
```

Ctrl+C pauses `hiroi get`, and the same command continues. While a
download runs, `NAME.hiroi-part` holds the data and
`NAME.hiroi-state` the progress; the finished file replaces both.
`hiroi --help`, and `--help` after any command, list every option.

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
`snapshot`, `inspect`, and `exposure`. Every `DownloadSnapshot` carries
the segment layout, ready for a segment bar.

## Project layout

| Path | Contents |
|---|---|
| `crates/hiroi-engine/` | Downloads: probing, segments, limits, storage, streams, protocols, network rules, privacy |
| `crates/hiroi-grab/` | Reading web pages and finding what they offer |
| `crates/hiroi-organize/` | Series in file names, and names for groups |
| `crates/hiroi-proto/` | Messages between the service and its clients |
| `crates/hiroi-store/` | The service's SQLite database |
| `crates/hiroi-daemon/` | The background service, its client, and the browser's native host |
| `crates/hiroi-cli/` | The `hiroi` command |
| `crates/hiroi-testserver/` | Misbehaving HTTP and FTP servers for tests and benchmarks |
| `extension/` | Browser extension for Firefox and Chromium-based browsers |
| `docs/` | The guide to every feature |
| `fuzz/` | cargo-fuzz targets (nightly) |
| `bench/` | Benchmarks against curl, wget, and aria2 |
| `contrib/` | A systemd user unit for the service |

## Tests

```sh
cargo test --workspace                  # 202 tests
cargo test --workspace --all-features   # 221, with FTP, SFTP, and torrents
```

The tests download from servers they start themselves, which throttle,
drop connections, answer `429`, change files halfway, and send hostile
names. Every parser also gets 3000 mangled inputs on each run;
`fuzz/` holds the cargo-fuzz targets for longer runs. CI runs `fmt`,
`clippy`, and the tests on every push.

## Roadmap

Done: the engine, the background service with its SQLite queue,
queues and schedules, categories, groups and smart folders, the
browser extension with its menus and media detection, the clipboard
watcher, page grabbing, HLS and DASH streams, video sites through
yt-dlp, Metalink with piece repair, FTP, SFTP, and BitTorrent (through
SOCKS5 proxies where they can), interface binding, the IP filter, the
exposure report, the settings file and search, fuzzing, and
benchmarks.

Next:

- A GUI, which I want to build myself.
- The background service on Windows (named pipes), and browser
  registration through the Windows registry.
- Subtitles and the choice of audio language for streams.
- Choosing which files of a torrent to download, and seeding limits.
- Telegram and other apps as sources. The engine's protocol boundary
  (probe a source, open a range of it) is where they would plug in.

## License

Copyright (C) 2026 Bamshi-dev

Hiroi Downloader is free software: you can redistribute it and/or modify
it under the terms of the GNU Affero General Public License as published
by the Free Software Foundation, either version 3 of the License, or (at
your option) any later version. See [LICENSE](LICENSE).

In short: you may use, study, change, and share Hiroi, but anything you
build on it and distribute, or offer to users over a network, must be
released under the same license with its full source code.
