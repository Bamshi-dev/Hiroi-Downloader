// Shared by the background script and the extension pages.
"use strict";

const api = globalThis.browser ?? globalThis.chrome;
const HOST = "dev.hiroi.downloader";

const KINDS = {
  video: ["mp4", "mkv", "webm", "avi", "mov", "m4v", "ts", "flv", "wmv", "3gp"],
  audio: ["mp3", "flac", "ogg", "opus", "m4a", "wav", "aac"],
  image: ["jpg", "jpeg", "png", "gif", "webp", "avif", "svg", "bmp", "heic"],
  stream: ["m3u8", "mpd"],
  archive: ["zip", "rar", "7z", "tar", "gz", "tgz", "xz", "bz2", "zst"],
  document: ["pdf", "epub", "doc", "docx", "odt", "xls", "xlsx", "ppt", "pptx", "csv", "txt"],
  program: ["deb", "rpm", "appimage", "flatpak", "exe", "msi", "dmg", "apk"],
  "disk-image": ["iso", "img"],
  subtitle: ["srt", "vtt", "ass"],
  torrent: ["torrent"],
  metalink: ["meta4", "metalink"],
};

/** Groups of kinds the menus and the grab page offer together. */
const KIND_GROUPS = {
  media: ["video", "audio", "stream"],
  image: ["image"],
  files: ["archive", "document", "program", "disk-image", "torrent", "metalink"],
};

/**
 * Every option of the extension, for the options page and its search.
 * The app's own settings are separate: `hiroi settings list`.
 */
const OPTIONS = [
  {
    key: "capture",
    type: "bool",
    default: false,
    label: "Send browser downloads to Hiroi",
    help: "Downloads the browser starts go to Hiroi instead, if they match the rules below. The browser keeps the download when Hiroi is not running.",
  },
  {
    key: "captureTypes",
    type: "text",
    default: "zip rar 7z iso exe msi dmg deb rpm apk mp4 mkv webm mp3 flac pdf epub torrent",
    label: "File types to send",
    help: "Extensions separated by spaces. Empty sends every type.",
  },
  {
    key: "captureMinSize",
    type: "number",
    default: 1,
    label: "Smallest download to send (MB)",
    help: "Smaller downloads stay in the browser. Downloads of unknown size are sent.",
  },
  {
    key: "captureExcluded",
    type: "text",
    default: "",
    label: "Sites to leave alone",
    help: "Host names separated by spaces; their subdomains are left alone too.",
  },
  {
    key: "videoButton",
    type: "bool",
    default: false,
    label: "Show a download button on videos",
    help: "A small button over videos on every site. It needs access to the pages you visit.",
  },
  {
    key: "detectMedia",
    type: "bool",
    default: true,
    label: "Find videos and audio as pages play them",
    help: "Watches what tabs load for media and stream playlists, and counts them on the toolbar button.",
  },
  {
    key: "quality",
    type: "text",
    default: "best",
    label: "Video quality",
    help: "best, worst, or a height such as 720, for streams and video pages.",
  },
];

async function getOptions() {
  const defaults = Object.fromEntries(OPTIONS.map((option) => [option.key, option.default]));
  return api.storage.local.get(defaults);
}

/** Kind of a URL from its file extension, or null. */
function kindOf(url) {
  let path;
  try {
    path = new URL(url).pathname.toLowerCase();
  } catch {
    return null;
  }
  return kindOfName(path.slice(path.lastIndexOf("/") + 1));
}

function kindOfName(name) {
  const dot = name.lastIndexOf(".");
  if (dot < 0) return null;
  const extension = name.slice(dot + 1).toLowerCase();
  for (const [kind, extensions] of Object.entries(KINDS)) {
    if (extensions.includes(extension)) return kind;
  }
  return null;
}

/** Cookies the browser would send to `url`, as a Cookie header value. */
async function cookiesFor(url) {
  try {
    const cookies = await api.cookies.getAll({ url });
    return cookies.map((cookie) => `${cookie.name}=${cookie.value}`).join("; ");
  } catch {
    return "";
  }
}

/**
 * The message for one download. `item` has `url`, and may have
 * `referrer`, `fileName`, and `extract` (a video page for the site
 * extractor). `cookies` caches lookups by origin within a batch.
 */
async function downloadMessage(item, quality, cookies = new Map()) {
  let origin = item.url;
  try {
    origin = new URL(item.url).origin;
  } catch {
    // The host rejects it with a clear message.
  }
  if (!cookies.has(origin)) cookies.set(origin, await cookiesFor(item.url));
  return {
    url: item.url,
    referrer: item.referrer || null,
    cookies: cookies.get(origin),
    userAgent: navigator.userAgent,
    fileName: item.fileName || null,
    extract: Boolean(item.extract),
    quality: quality || null,
  };
}

async function native(message) {
  try {
    return await api.runtime.sendNativeMessage(HOST, message);
  } catch (error) {
    return { ok: false, error: `Hiroi is not installed for this browser (${error.message})` };
  }
}

/** Hands one download to Hiroi. Resolves to the host's answer. */
async function sendToHiroi(item) {
  const { quality } = await getOptions();
  return native({ type: "download", ...(await downloadMessage(item, quality)) });
}

/**
 * Hands several downloads to Hiroi in one message; they form a group,
 * named after them or `title` when `group` is empty.
 */
async function sendBatch(items, title, group) {
  const { quality } = await getOptions();
  const cookies = new Map();
  const messages = [];
  for (const item of items) messages.push(await downloadMessage(item, quality, cookies));
  return native({ type: "batch", items: messages, title: title || null, group: group || null });
}

async function ping() {
  return native({ type: "ping" });
}

/** A short name for a URL, for lists. */
function displayName(url) {
  try {
    const path = new URL(url).pathname;
    return decodeURIComponent(path.split("/").pop() || url);
  } catch {
    return url;
  }
}

function humanSize(bytes) {
  if (!bytes || bytes < 0) return "";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value.toFixed(unit ? 1 : 0)} ${units[unit]}`;
}
