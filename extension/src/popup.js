"use strict";

const $ = (id) => document.getElementById(id);

(async () => {
  const [tab] = await api.tabs.query({ active: true, currentWindow: true });

  const answer = await ping();
  $("status").textContent = answer.ok ? `Connected to Hiroi ${answer.version}` : `Hiroi is not reachable: ${answer.error}`;
  $("status").className = answer.ok ? "" : "error";

  const media = (await api.runtime.sendMessage({ type: "media", tabId: tab.id })) || [];
  $("none").hidden = media.length > 0;
  for (const entry of media) {
    const row = document.createElement("li");
    const name = document.createElement("span");
    name.textContent = entry.name;
    name.title = entry.url;
    const info = document.createElement("small");
    info.textContent = [entry.kind, humanSize(entry.size)].filter(Boolean).join(", ");
    const button = document.createElement("button");
    button.textContent = "Download";
    button.addEventListener("click", async () => {
      button.disabled = true;
      const sent = await sendToHiroi({ url: entry.url, referrer: tab.url });
      button.textContent = sent.ok ? `Job ${sent.job}` : "Failed";
      button.title = sent.ok ? "" : sent.error;
    });
    row.append(name, info, button);
    $("media").append(row);
  }

  $("video").addEventListener("click", async () => {
    $("video").disabled = true;
    const sent = await sendToHiroi({ url: tab.url, extract: true });
    $("video").textContent = sent.ok ? `Added as job ${sent.job}` : `Hiroi: ${sent.error}`;
  });

  $("grab").addEventListener("click", async () => {
    await api.runtime.sendMessage({ type: "grab", tabId: tab.id });
    window.close();
  });

  const capture = $("capture");
  capture.checked = (await getOptions()).capture;
  capture.addEventListener("change", () => api.storage.local.set({ capture: capture.checked }));

  $("options").addEventListener("click", (event) => {
    event.preventDefault();
    api.runtime.openOptionsPage();
    window.close();
  });
})();
