// Finds the media a tab plays by watching the responses it receives:
// stream playlists (HLS, DASH) and direct video and audio files. Players
// often load these from scripts, so they never appear in the page.
//
// Only response headers are read, never bodies. What is found stays in
// memory (and the browser's session storage, which a restart clears).
"use strict";

/** Direct media smaller than this are previews, ads, or segments. */
const MIN_MEDIA_BYTES = 512 * 1024;
/** Most entries kept per tab; the oldest go first. */
const MAX_PER_TAB = 50;

const PLAYLIST_TYPES = /mpegurl|dash\+xml/i;
const MEDIA_TYPES = /^(video|audio)\//i;
/** Pieces of a stream, not something to download on its own. */
const SEGMENT = /\.(ts|m4s|m4f|aac|cmfv|cmfa)(\?|$)|\/(seg|segment|chunk|frag)[-_]?\d/i;

const detected = new Map(); // tabId -> Map(key -> entry)

function header(headers, name) {
  const found = headers?.find((h) => h.name.toLowerCase() === name);
  return found ? found.value : "";
}

/** Full size from Content-Range (`bytes 0-99/1234`) or Content-Length. */
function sizeOf(headers) {
  const range = header(headers, "content-range").match(/\/(\d+)$/);
  if (range) return Number(range[1]);
  const length = Number(header(headers, "content-length"));
  return Number.isFinite(length) ? length : 0;
}

function classify(details) {
  const type = header(details.responseHeaders, "content-type").split(";")[0].trim();
  const url = details.url;
  if (PLAYLIST_TYPES.test(type) || /\.(m3u8|mpd)(\?|$)/i.test(url)) {
    return { kind: "stream", size: 0 };
  }
  if (!MEDIA_TYPES.test(type) || SEGMENT.test(url)) return null;
  const size = sizeOf(details.responseHeaders);
  if (size && size < MIN_MEDIA_BYTES) return null;
  return { kind: type.startsWith("audio") ? "audio" : "video", size };
}

async function record(details) {
  if (details.tabId < 0 || !/^https?:/.test(details.url)) return;
  if (details.statusCode && details.statusCode >= 400) return;
  const { detectMedia } = await getOptions();
  if (!detectMedia) return;
  const found = classify(details);
  if (!found) return;
  const key = keyOf(details.url);
  let tab = detected.get(details.tabId);
  if (!tab) {
    tab = new Map();
    detected.set(details.tabId, tab);
  }
  if (tab.has(key)) return;
  tab.set(key, { url: details.url, ...found, name: displayName(details.url), seen: Date.now() });
  while (tab.size > MAX_PER_TAB) tab.delete(tab.keys().next().value);
  await save(details.tabId);
  showCount(details.tabId);
}

/** The same file asked for in ranges, or with new tokens, is one entry. */
function keyOf(address) {
  const url = new URL(address);
  return url.origin + url.pathname;
}

async function save(tabId) {
  if (!api.storage.session) return;
  const entries = [...(detected.get(tabId)?.values() ?? [])];
  await api.storage.session.set({ [`media:${tabId}`]: entries }).catch(() => {});
}

/** What tab `tabId` played, newest last. */
async function mediaOf(tabId) {
  if (!detected.has(tabId) && api.storage.session) {
    // The background worker may have been restarted since.
    const stored = await api.storage.session.get(`media:${tabId}`).catch(() => ({}));
    const entries = stored[`media:${tabId}`] ?? [];
    detected.set(tabId, new Map(entries.map((entry) => [keyOf(entry.url), entry])));
  }
  return [...(detected.get(tabId)?.values() ?? [])];
}

function forget(tabId) {
  detected.delete(tabId);
  api.storage.session?.remove(`media:${tabId}`).catch(() => {});
  api.action.setBadgeText({ tabId, text: "" }).catch(() => {});
}

function showCount(tabId) {
  const count = detected.get(tabId)?.size ?? 0;
  api.action.setBadgeText({ tabId, text: count ? String(count) : "" }).catch(() => {});
}

api.webRequest.onHeadersReceived.addListener(
  (details) => {
    record(details);
  },
  { urls: ["<all_urls>"], types: ["media", "xmlhttprequest", "other"] },
  ["responseHeaders"],
);

api.tabs.onRemoved.addListener(forget);
api.tabs.onUpdated.addListener((tabId, change) => {
  // A new page starts with a clean list.
  if (change.status === "loading" && change.url) forget(tabId);
});
