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

/** Kind of a URL from its file extension, or null. */
function kindOf(url) {
  let path;
  try {
    path = new URL(url).pathname.toLowerCase();
  } catch {
    return null;
  }
  const dot = path.lastIndexOf(".");
  if (dot < 0 || dot < path.lastIndexOf("/")) return null;
  const extension = path.slice(dot + 1);
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

/** Hands one download to Hiroi. Resolves to the host's answer. */
async function sendToHiroi(url, referrer, fileName) {
  const message = {
    type: "download",
    url,
    referrer: referrer || null,
    cookies: await cookiesFor(url),
    userAgent: navigator.userAgent,
    fileName: fileName || null,
  };
  try {
    return await api.runtime.sendNativeMessage(HOST, message);
  } catch (error) {
    return { ok: false, error: `Hiroi is not installed for this browser (${error.message})` };
  }
}

async function ping() {
  try {
    return await api.runtime.sendNativeMessage(HOST, { type: "ping" });
  } catch (error) {
    return { ok: false, error: error.message };
  }
}
