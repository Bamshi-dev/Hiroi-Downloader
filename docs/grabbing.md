# Grabbing pages

`hiroi grab` lists what a web page offers (files, pictures, videos,
audio, subtitles, and streams) and downloads what you choose.

```sh
hiroi grab https://example.com/gallery            # list, then ask which
hiroi grab https://example.com/gallery -t image --all
hiroi grab https://example.com/files --ext zip,iso --pick 1,3,5-9
hiroi grab https://example.com/course -t video --queue --group "Course"
```

## Choosing

- `--list` only lists.
- `--all` takes everything that matches, `--first N` the first N, and
  `--pick 1,3,5-9` those numbers. Without them, Hiroi asks.
- `--queue` sends the choice to the background service as one
  [group](organizing.md#groups), named after the files or the page's
  title unless `--group NAME` names it.

## Filtering

- `-t TYPES` keeps types: video, audio, image, stream, archive,
  document, program, disk-image, subtitle, torrent, metalink, other.
- `--ext mp4,zip` keeps extensions, and `--contains TEXT` links whose
  address or text contains TEXT.
- `--min-size` and `--max-size` keep sizes, which needs `--probe`:
  that asks each server for the size and type with a one-byte request,
  which also finds files behind addresses like `download.php?id=7`.

## Following links

`--depth N` also reads pages the page links to, N levels deep, on the
same site unless `--other-hosts` is given, and at most `--max-pages`
pages (50). robots.txt is respected; `--ignore-robots` ignores it.

The browser extension has a grab page as well, which also sees what
scripts added to the page; see [the browser
extension](browser.md#the-grab-page).
