# Getting started

## Building

You need Rust 1.89 or newer and a C compiler (`gcc` or `clang`).

```sh
cargo build --release
cargo install --path crates/hiroi-cli      # puts `hiroi` on your PATH
```

FTP, SFTP, and BitTorrent are optional; build them in with:

```sh
cargo install --path crates/hiroi-cli --features ftp,sftp,torrent
```

Two other programs are optional too: ffmpeg joins separate video and
audio tracks, and yt-dlp reads video sites.

## The first download

```sh
hiroi get https://example.com/big.iso
```

The file goes into the current folder, over 8 connections. While it
runs, `big.iso.hiroi-part` holds the data and `big.iso.hiroi-state`
the progress; the finished file replaces both. Ctrl+C pauses, and
running the same command again continues where it stopped, after a
crash or a reboot too.

`hiroi info URL` asks the server about a file (name, size, and whether
it can be resumed) without downloading it.

## The background service

The service keeps a list of downloads, runs them in the background,
and remembers them across restarts.

```sh
hiroi daemon &                  # start it (or use contrib/hiroi.service)
hiroi add https://example.com/big.iso
hiroi list                      # the jobs and their state
hiroi watch                     # live progress
hiroi pause 1                   # a job by its number, or `all`
hiroi resume 1
hiroi cancel 1                  # stop it and delete its temporary files
hiroi remove 1                  # forget a finished job; the file stays
hiroi stop                      # stop the service; jobs continue next time
```

Downloads that name no folder go into your download folder, sorted by
type (see [categories](organizing.md#categories)). `hiroi daemon --dir
DIR` picks another folder.

`hiroi add` takes several URLs, or `--input FILE` with one URL per
line (`#` starts a comment); several URLs become a
[group](organizing.md#groups). Other options of `hiroi add`:

- `--queue NAME` puts the jobs in a queue other than `main`.
- `--paused` adds them without starting.
- `--high-priority` starts them before other waiting jobs.
- `--allow-duplicate` adds a file that is already in the list.
- `--then COMMAND` runs a program when the job finishes; `{path}`,
  `{name}`, and `{dir}` are replaced, and no shell is involved.
- `--open-folder` opens the folder when the job finishes.

`hiroi notifications on|off` turns desktop notifications on or off,
and `hiroi power-off-when-done on` powers the computer off once every
download is done.

The service runs on Linux and macOS. On Windows, `hiroi get` and the
other foreground commands work; the service does not yet.

## The browser extension

```sh
extension/build.sh                       # builds extension/dist/firefox and dist/chrome
hiroi browser install --firefox          # lets Firefox's extension reach Hiroi
hiroi browser install --chrome ID        # the same for Chromium-based browsers
```

Load `extension/dist/firefox` in Firefox (about:debugging, "Load
Temporary Add-on") or `extension/dist/chrome` in Chrome (chrome://extensions,
developer mode, "Load unpacked"); the Chrome extension's ID is shown
there. The service must be running. See [the browser
extension](browser.md) for what it does.

## Copied links

`hiroi clipboard` watches the clipboard and offers copied download
links to the service; `--auto` adds them without asking.
