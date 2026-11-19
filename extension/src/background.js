// Context menus and download capture.
"use strict";

if (typeof importScripts === "function") {
  importScripts("common.js"); // Chrome's service worker
}

const MENU_LINK = "hiroi-download";
const MENU_PAGE = "hiroi-grab";

api.runtime.onInstalled.addListener(() => {
  api.contextMenus.create({
    id: MENU_LINK,
    title: "Download with Hiroi",
    contexts: ["link", "image", "video", "audio"],
  });
  api.contextMenus.create({ id: MENU_PAGE, title: "Grab this page with Hiroi", contexts: ["page"] });
});

api.contextMenus.onClicked.addListener(async (info, tab) => {
  if (info.menuItemId === MENU_LINK) {
    const url = info.linkUrl || info.srcUrl;
    const answer = await sendToHiroi(url, info.pageUrl);
    notify(answer.ok ? `Added to Hiroi as job ${answer.job}` : `Hiroi: ${answer.error}`);
  } else if (info.menuItemId === MENU_PAGE && tab) {
    openGrabber(tab.id);
  }
});

api.runtime.onMessage.addListener((message, _sender, reply) => {
  if (message?.type === "grab" && typeof message.tabId === "number") {
    openGrabber(message.tabId);
    reply({ ok: true });
  }
});

function openGrabber(tabId) {
  api.tabs.create({ url: api.runtime.getURL(`grab.html?tab=${tabId}`) });
}

// Optional capture of downloads the browser starts by itself. The
// browser's download is cancelled only after Hiroi accepted the job, so
// nothing is lost when Hiroi is not running.
api.downloads.onCreated.addListener(async (item) => {
  const { capture } = await api.storage.local.get({ capture: false });
  if (!capture || !/^https?:/.test(item.finalUrl || item.url)) return;
  const url = item.finalUrl || item.url;
  const name = item.filename ? item.filename.split(/[\\/]/).pop() : null;
  const answer = await sendToHiroi(url, item.referrer, name);
  if (answer.ok) {
    await api.downloads.cancel(item.id).catch(() => {});
    await api.downloads.erase({ id: item.id }).catch(() => {});
    notify(`Hiroi took over ${name || url}`);
  }
});

function notify(message) {
  api.action.setBadgeText({ text: "•" });
  api.action.setTitle({ title: message });
  setTimeout(() => api.action.setBadgeText({ text: "" }), 3000);
}
