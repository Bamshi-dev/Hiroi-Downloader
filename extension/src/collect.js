// Runs inside the page (every frame) and returns what it can see. This
// sees the live DOM, so it finds content that scripts added after load,
// and the resource timing list, which names stream playlists a player
// fetched even when no element shows them.
"use strict";

function hiroiCollect() {
  const found = new Map();
  const add = (raw, where, text) => {
    if (!raw) return;
    let url;
    try {
      url = new URL(raw, document.baseURI);
    } catch {
      return;
    }
    if (!/^https?:$/.test(url.protocol)) return;
    url.hash = "";
    const key = url.href;
    if (!found.has(key)) found.set(key, { url: key, where, text: (text || "").trim().slice(0, 120) });
  };
  const largest = (srcset) => {
    let best = null;
    let bestSize = -1;
    for (const candidate of (srcset || "").split(",")) {
      const [url, descriptor] = candidate.trim().split(/\s+/);
      const size = descriptor ? parseFloat(descriptor) * (descriptor.endsWith("x") ? 1000 : 1) : 1;
      if (url && size > bestSize) [best, bestSize] = [url, size];
    }
    return best;
  };

    for (const a of document.querySelectorAll("a[href], area[href]")) {
    // "#section" links point into this page, not to a download.
    if (a.getAttribute("href").trim().startsWith("#")) continue;
    add(a.href, "anchor", a.textContent || a.title);
  }
  for (const img of document.images) {
    add(img.currentSrc || img.src, "image", img.alt);
    add(largest(img.srcset), "image", img.alt);
    add(img.dataset.src, "image", img.alt);
  }
  for (const media of document.querySelectorAll("video, audio")) {
    add(media.currentSrc || media.src, media.tagName.toLowerCase());
    if (media.poster) add(media.poster, "image");
    for (const source of media.querySelectorAll("source")) add(source.src, media.tagName.toLowerCase());
    for (const track of media.querySelectorAll("track")) add(track.src, "subtitle", track.label);
  }
  for (const meta of document.querySelectorAll('meta[property^="og:"], meta[name^="twitter:"]')) {
    if (/video|image|audio|stream/.test(meta.getAttribute("property") || meta.name)) add(meta.content, "meta");
  }
  for (const entry of performance.getEntriesByType("resource")) {
    if (/\.(m3u8|mpd|mp4|webm|m4s|mp3)(\?|$)/i.test(entry.name)) add(entry.name, "network");
  }
  return [...found.values()];
}

hiroiCollect();
