"use strict";

(async () => {
  const status = document.getElementById("status");
  const answer = await ping();
  status.textContent = answer.ok ? `Connected to Hiroi ${answer.version}` : `Hiroi is not reachable: ${answer.error}`;
  status.className = answer.ok ? "" : "error";

  const capture = document.getElementById("capture");
  capture.checked = (await api.storage.local.get({ capture: false })).capture;
  capture.addEventListener("change", () => api.storage.local.set({ capture: capture.checked }));

  document.getElementById("grab").addEventListener("click", async () => {
    const [tab] = await api.tabs.query({ active: true, currentWindow: true });
    await api.runtime.sendMessage({ type: "grab", tabId: tab.id });
    window.close();
  });
})();
