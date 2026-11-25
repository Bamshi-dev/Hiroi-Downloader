# Downloading

Everything here works with `hiroi get` (in the foreground) and
`hiroi add` (through the background service), unless it says
otherwise.

## Connections

A file is split across connections, 8 by default and up to 32
(`-c 16`). When a connection finishes early, it takes over half of the
largest piece still in flight, so none sits idle until the last byte.
A connection far slower than the others is replaced;
`--min-speed 50K` also replaces any connection slower than that.

Servers get a limited number of connections across all downloads (16,
or `--per-host N`). When a server answers 429 or 503, Hiroi halves
that number for it, waits as long as `Retry-After` asks, and grows
back slowly.

## Speed limits

- `-l 2M` limits one download.
- `hiroi get --global-limit 5M` limits all downloads of that command.
- `hiroi limit 5M` limits the whole service, `hiroi limit off` lifts
  it, and `hiroi limit 1M --job 3` limits one job.
- Schedules can limit the speed at set times; see
  [schedules](organizing.md#schedules).

Speeds take K, M, and G (1K is 1024 bytes per second).

## Names and folders

- `-d DIR` saves into a folder, `-o NAME` under a name.
- `--on-conflict rename` (the default) saves as `name (1).ext` when the
  file exists, `overwrite` replaces it, and `skip` leaves it.
- Names from servers are cleaned: no folders, no characters that some
  systems refuse, and no names Windows reserves.

## Resuming

Progress is saved every two seconds, after the data is on disk. Every
answer is checked with `If-Range` and `Content-Range`, so a file that
changed on the server is never mixed with old data: the download
stops and says so.

`--no-sync` skips the flush to disk before progress is saved. It is
faster on slow disks, but a power cut can then leave holes in a file.
`--no-prealloc` does not reserve the file's size on disk first.

## Checksums and mirrors

```sh
hiroi get URL --checksum sha256:9f86d0...
hiroi get URL -m https://mirror.example.org/file.iso -m https://other.example.net/file.iso
```

MD5, SHA-1, SHA-256, and SHA-512 are checked once the file is
complete. Mirrors serve the same file at the same time; a mirror that
answers with a different size or fails keeps out of it.

Any file can be checked, wherever it came from:

```sh
hiroi checksum debian.iso                         # prints sha256:...
hiroi checksum debian.iso 9f86d081884c7d65...     # OK or MISMATCH
hiroi checksum file.zip --algorithm md5
```

A hash given as plain hex is read by its length (32 is MD5, 40 SHA-1,
64 SHA-256, and 128 SHA-512). A mismatch exits with an error.

## Many files at once

`hiroi get` takes several URLs, or `-i FILE` with one per line, and
runs 3 at a time (`-j N`). `hiroi add` with several URLs makes a
[group](organizing.md#groups).

A numbered series can be written as one URL:

```sh
hiroi add 'https://example.com/course/lecture[01-20].mp4'
hiroi add 'https://example.com/backup.part[1-9].rar'
hiroi get 'https://example.com/scans/page[0-100:10].png'
hiroi get 'https://example.com/{intro,setup,end}.pdf'
```

- `[1-20]` counts, and `[01-20]` keeps the zeros: `01`, `02`, and so
  on.
- `[0-100:10]` counts in steps of 10.
- `[a-z]` runs through letters.
- `{intro,setup,end}` takes each word.

Several give every combination, in order. Quote the URL so the shell
leaves the brackets alone. Brackets that are not a range, such as an
IPv6 address, stay as written, and more than 10,000 URLs at once are
refused as a likely typo. `--literal` takes URLs exactly as written.

## Expired links

Many sites hand out links that stop working after a while, sometimes
in the middle of a big download. Copy a fresh link from the site and
give it to the job:

```sh
hiroi pause 7                       # if it is still trying
hiroi refresh 7 'https://example.com/new-link?token=...'
hiroi resume 7
```

What was downloaded is kept when the file at the new address has the
same size, and it keeps the name it had. Hiroi cannot tell for certain
that it is the same file, so give a checksum where the site offers
one.

## Request headers

- `-H "Name: value"` adds a header.
- `--cookie "a=1; b=2"` sends cookies, as copied from the browser.
  They go only to the site they belong to and its subdomains.
- `--referer URL` and `--user-agent UA` set those headers.

Cookies a server sets during a download are kept in memory for that
download, since some download pages set a session cookie and then
redirect to the file; `--no-cookie-jar` ignores them. Neither kind of
cookie is ever written to disk or to a log.

## Streams

HLS (`.m3u8`) and DASH (`.mpd`) links are downloaded as the video they
describe, and the pieces are joined (with ffmpeg when video and audio
come apart).

- `--quality best` (the default), `worst`, or a height such as `720`.
- Live streams are recorded until they end, or for `--live-limit 30m`.

## Video sites

```sh
hiroi get -x https://video.example/watch?v=abc
hiroi get -f "bv*+ba/b" https://video.example/watch?v=abc
```

`-x` hands the page to yt-dlp, which finds the media; Hiroi then
downloads it with its own connections. `-f` passes a yt-dlp format
selection (and implies `-x`).

yt-dlp first reads the page without your browser's cookies, since
signed in, some sites offer it fewer formats; the cookies are used
when that fails, for videos that need a login.

Sites change often, and an old yt-dlp is the commonest reason a video
page fails, so Hiroi says when yours is more than four months old.
Distributions often ship old versions; yt-dlp's own build, which
updates itself with `yt-dlp -U`, avoids that. `--yt-dlp PATH`, or
`hiroi settings set yt-dlp PATH`, picks the program to use.

## Metalink

`.meta4` and `.metalink` files are read: their mirrors are used
together, the strongest checksum is checked, and when the file has
piece checksums, only the pieces that fail are fetched again, from a
mirror that did not send them.

## FTP and SFTP

With the `ftp` and `sftp` features:

```sh
hiroi get ftp://ftp.example.org/pub/file.iso
hiroi get ftps://user:password@example.org/file.iso
hiroi get sftp://user@example.org/home/user/file.iso
```

FTP downloads over several connections too, when the server allows
resuming. `ftps://` uses TLS for the login and the data; plain `ftp://`
sends both in clear text. SFTP servers are checked against
`~/.ssh/known_hosts`, and an unknown or changed key stops the download.

With a SOCKS5 proxy, FTP and SFTP go through it. They refuse to run
with an HTTP proxy rather than connect around it.

## BitTorrent

With the `torrent` feature, magnet links and links to `.torrent` files work
like other downloads. Every peer sees your address, so torrents refuse
to run when a proxy is set.

## Asking first

`hiroi info URL` shows the name, size, type, and whether the download
can be resumed, and which mirrors agree. `hiroi exposure URL` shows who
would see the download; see [network and
privacy](network-and-privacy.md#what-a-download-gives-away).
