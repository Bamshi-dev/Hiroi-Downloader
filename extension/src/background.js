// Context menus, download capture, and the video button.
"use strict";

if (typeof importScripts === "function") {
  importScripts("common.js", "detect.js"); // Chrome's service worker
}

const MENUS = [
  { id: "link", title: "Download link with Hiroi", contexts: ["link"] },
  { id: "image", title: "Download image with Hiroi", contexts: ["image"] },
  { id: "media", title: "Download this video with Hiroi", contexts: ["video", "audio"] },
  { id: "selection", title: "Download selected links with Hiroi", contexts: ["selection"] },
  { id: "page", title: "Hiroi Downloader", contexts: ["page", "frame"] },
  { id: "page-video", parentId: "page", title: "Download the video on this page", contexts: ["page", "frame"] },
  { id: "page-media", parentId: "page", title: "Choose videos and audio…", contexts: ["page", "frame"] },
  { id: "page-image", parentId: "page", title: "Choose images…", contexts: ["page", "frame"] },
  { id: "page-files", parentId: "page", title: "Choose files and documents…", contexts: ["page", "frame"] },
  { id: "page-all", parentId: "page", title: "Choose from everything on this page…", contexts: ["page", "frame"] },
];

const VIDEO_BUTTON_SCRIPT = "hiroi-video-button";

api.runtime.onInstalled.addListener(() => {
  api.contextMenus.removeAll(() => {
    for (const menu of MENUS) api.contextMenus.create(menu);
  });
  syncVideoButton();
});
api.runtime.onStartup?.addListener(syncVideoButton);
api.storage.onChanged.addListener((changes) => {
  if (changes.videoButton) syncVideoButton();
});

api.contextMenus.onClicked.addListener(async (info, tab) => {
  const page = info.frameUrl || info.pageUrl;
  switch (info.menuItemId) {
    case "link":
      return report(await sendToHiroi({ url: info.linkUrl, referrer: page }));
    case "image":
      return report(await sendToHiroi({ url: info.srcUrl, referrer: page }));
    case "media":
      return report(await downloadVideo(info.srcUrl, tab, page));
    case "selection":
      return downloadSelection(tab, info.frameId);
    case "page-video":
      return report(await downloadVideo(null, tab, page));
    case "page-media":
    case "page-image":
    case "page-files":
      return openGrabber(tab.id, info.menuItemId.slice("page-".length));
    case "page-all":
      return openGrabber(tab.id, "");
  }
});

api.runtime.onMessage.addListener((message, sender, reply) => {
  const tabId = message?.tabId ?? sender.tab?.id;
  if (message?.type === "grab" && typeof tabId === "number") {
    openGrabber(tabId, message.kind ?? "");
    reply({ ok: true });
  } else if (message?.type === "video" && sender.tab) {
    // From the video button inside a page.
    downloadVideo(message.src, sender.tab, sender.url).then((answer) => {
      report(answer);
      reply(answer);
    });
    return true;
  } else if (message?.type === "media" && typeof tabId === "number") {
    mediaOf(tabId).then(reply);
    return true;
  }
  return false;
});

/**
 * A video by its address when it has a real one; otherwise (a `blob:`
 * address, as streaming players use) the stream the tab loaded, or the
 * page itself for the site extractor.
 */
async function downloadVideo(src, tab, page) {
  if (src && /^https?:/.test(src)) return sendToHiroi({ url: src, referrer: page });
  const streams = (await mediaOf(tab.id)).filter((entry) => entry.kind === "stream");
  // The first playlist a player loads is usually the one listing every
  // quality; Hiroi picks the quality from it.
  if (streams.length) return sendToHiroi({ url: streams[0].url, referrer: page });
  return sendToHiroi({ url: page, extract: true });
}

/** Links inside the selection, from the frame it is in. */
async function downloadSelection(tab, frameId) {
  let results;
  try {
    results = await api.scripting.executeScript({
      target: { tabId: tab.id, frameIds: [frameId ?? 0] },
      func: selectedLinks,
    });
  } catch (error) {
    return report({ ok: false, error: error.message });
  }
  const urls = [...new Set(results.flatMap(({ result }) => result || []))];
  if (!urls.length) return report({ ok: false, error: "no links in the selection" });
  const items = urls.map((url) => ({ url, referrer: tab.url }));
  report(items.length === 1 ? await sendToHiroi(items[0]) : await sendBatch(items, tab.title));
}

/** Runs in the page: the http(s) links the selection touches. */
function selectedLinks() {
  const selection = window.getSelection();
  const links = [];
  for (let i = 0; i < selection.rangeCount; i += 1) {
    const range = selection.getRangeAt(i);
    const root = range.commonAncestorContainer;
    const scope = root.nodeType === Node.ELEMENT_NODE ? root : root.parentElement;
    const anchor = scope?.closest("a[href]");
    if (anchor) links.push(anchor.href);
    for (const a of scope?.querySelectorAll("a[href]") ?? []) {
      if (selection.containsNode(a, true)) links.push(a.href);
    }
  }
  return links.filter((href) => /^https?:/.test(href));
}

function openGrabber(tabId, kind) {
  const query = new URLSearchParams({ tab: String(tabId), kind });
  api.tabs.create({ url: api.runtime.getURL(`grab.html?${query}`) });
}

// Capture of downloads the browser starts by itself. The browser's
// download is cancelled only after Hiroi accepted the job, so nothing
// is lost when Hiroi is not running.
api.downloads.onCreated.addListener(async (item) => {
  const options = await getOptions();
  const url = item.finalUrl || item.url;
  if (!options.capture || !/^https?:/.test(url)) return;
  const name = item.filename ? item.filename.split(/[\\/]/).pop() : displayName(url);
  if (!shouldCapture(options, url, name, item.totalBytes || item.fileSize)) return;
  const answer = await sendToHiroi({ url, referrer: item.referrer, fileName: name });
  if (answer.ok) {
    await api.downloads.cancel(item.id).catch(() => {});
    await api.downloads.erase({ id: item.id }).catch(() => {});
    report({ ok: true, message: `Hiroi took over ${name || url}` });
  }
});

function shouldCapture(options, url, name, size) {
  const host = new URL(url).hostname;
  const excluded = options.captureExcluded.split(/\s+/).filter(Boolean);
  if (excluded.some((site) => host === site || host.endsWith(`.${site}`))) return false;
  const types = options.captureTypes.toLowerCase().split(/\s+/).filter(Boolean);
  const extension = (name || "").toLowerCase().split(".").pop();
  if (types.length && !types.includes(extension)) return false;
  // Unknown sizes (0 or -1) are sent: they are often large.
  return !(size > 0 && size < options.captureMinSize * 1024 * 1024);
}

/** The video button is a content script only while the option is on. */
async function syncVideoButton() {
  const { videoButton } = await getOptions();
  const registered = await api.scripting
    .getRegisteredContentScripts({ ids: [VIDEO_BUTTON_SCRIPT] })
    .catch(() => []);
  if (videoButton && !registered.length) {
    await api.scripting.registerContentScripts([
      {
        id: VIDEO_BUTTON_SCRIPT,
        matches: ["http://*/*", "https://*/*"],
        js: ["overlay.js"],
        runAt: "document_idle",
        allFrames: true,
      },
    ]);
  } else if (!videoButton && registered.length) {
    await api.scripting.unregisterContentScripts({ ids: [VIDEO_BUTTON_SCRIPT] });
  }
}

/** Shows the outcome on the toolbar button for a few seconds. */
function report(answer) {
  let message = answer.message;
  if (!message && answer.ok) {
    message = answer.group
      ? `Added ${answer.jobs.length} downloads to Hiroi as "${answer.group}"`
      : `Added to Hiroi as job ${answer.job}`;
  }
  message ??= `Hiroi: ${answer.error || (answer.errors || []).join("; ")}`;
  api.action.setBadgeText({ text: answer.ok ? "✓" : "!" });
  api.action.setTitle({ title: message });
  setTimeout(() => {
    api.action.setBadgeText({ text: "" });
    api.action.setTitle({ title: "Hiroi Downloader" });
  }, 4000);
}
