# The browser extension

The extension hands downloads to the background service, with the
cookies, referrer, and user agent the browser would have sent, so
downloads behind a login work. See [getting
started](getting-started.md#the-browser-extension) for installing it.

## Right-click menus

- **Download link with Hiroi**, on a link.
- **Download image with Hiroi**, on a picture.
- **Download this video with Hiroi**, on a video or audio player. A
  player that streams (its address starts with `blob:`) has no file
  behind it; Hiroi then takes the stream playlist the tab loaded, or
  hands the page to the site extractor (yt-dlp).
- **Download selected links with Hiroi**, on selected text: every link
  the selection touches, as one [group](organizing.md#groups).
- **Hiroi Downloader**, on the page:
  - **Download the video on this page**, through the stream the tab
    played or the site extractor.
  - **Choose videos and audio…**, **Choose images…**, and **Choose
    files and documents…** open the grab page with that kind shown and
    selected.
  - **Choose from everything on this page…** opens the grab page with
    nothing selected.

The toolbar button shows ✓ or ! for a few seconds afterwards; its
tooltip says what happened.

## The grab page

The grab page lists what the page shows, including what scripts added
after it loaded, and the media the tab played that the page does not
show. Filter by type or text, select, give the group a name if you
like, and choose **Download selected**. The selection goes over in one
message and becomes one group.

## Media on the page

As a tab plays video or audio, the extension notes the stream
playlists (HLS and DASH) and media files it receives, and the toolbar
button counts them. The popup lists them with a **Download** button
each, and offers **Download the video on this page** for the site
extractor. Only the headers of responses are looked at, never their
content, and files under 512 KB and the pieces of a stream are left
out. The option "Find videos and audio as pages play them" turns this
off.

## The video button

With the option "Show a download button on videos", a **Download with
Hiroi** button appears over videos while the pointer is on them. It is
off by default; while it is off, the extension adds nothing to pages.

## Capturing browser downloads

With "Send browser downloads to Hiroi" (in the popup or the options),
downloads the browser starts go to Hiroi instead, when they match:

- **File types to send**: extensions separated by spaces; empty sends
  every type.
- **Smallest download to send**: in MB; smaller ones stay in the
  browser, and those of unknown size are sent.
- **Sites to leave alone**: host names separated by spaces, with their
  subdomains.

The browser's download is cancelled only after Hiroi accepted it, so
nothing is lost when Hiroi is not running.

## Options

The options page (the popup's **Options** link) lists every option of
the extension, with a search over their names and descriptions. Video
quality ("best", "worst", or a height such as 720) applies to streams
and video pages. Hiroi's own settings are separate; see
[settings](settings.md).
