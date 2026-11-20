// The video button: a small "Download with Hiroi" button over videos,
// shown while the pointer is on one. Registered only when the option is
// on. The button lives in a closed shadow root, so pages can neither
// style it nor read it, and it is the only thing this script adds.
"use strict";

(() => {
  if (globalThis.hiroiVideoButton) return;
  globalThis.hiroiVideoButton = true;

  const api = globalThis.browser ?? globalThis.chrome;
  /** Smaller videos are icons, previews, or ads. */
  const MIN_WIDTH = 200;
  const MIN_HEIGHT = 120;
  const HIDE_AFTER_MS = 1500;

  const host = document.createElement("hiroi-video-button");
  const shadow = host.attachShadow({ mode: "closed" });
  const button = document.createElement("button");
  button.textContent = "Download with Hiroi";
  const style = document.createElement("style");
  style.textContent = `
    button {
      position: fixed; z-index: 2147483647; display: none;
      font: 12px system-ui, sans-serif; padding: 4px 8px; border: 0;
      border-radius: 4px; background: rgba(20, 20, 20, 0.85); color: #fff;
      cursor: pointer;
    }
    button:hover { background: rgba(20, 90, 160, 0.95); }
  `;
  shadow.append(style, button);
  document.documentElement.append(host);

  let current = null;
  let hideTimer = 0;

  const place = (video) => {
    const box = video.getBoundingClientRect();
    if (box.width < MIN_WIDTH || box.height < MIN_HEIGHT) return false;
    button.style.top = `${Math.max(box.top + 8, 8)}px`;
    button.style.left = `${Math.max(box.right - 160, 8)}px`;
    return true;
  };

  const show = (video) => {
    clearTimeout(hideTimer);
    if (!place(video)) return;
    current = video;
    button.style.display = "block";
    hideTimer = setTimeout(hide, HIDE_AFTER_MS);
  };

  const hide = () => {
    if (button.matches(":hover")) {
      hideTimer = setTimeout(hide, HIDE_AFTER_MS);
      return;
    }
    button.style.display = "none";
    current = null;
  };

  // One listener for the whole page instead of one per video.
  document.addEventListener(
    "pointermove",
    (event) => {
      const video = event.target.closest?.("video");
      if (video) show(video);
    },
    { capture: true, passive: true },
  );

  button.addEventListener("click", async (event) => {
    event.preventDefault();
    event.stopPropagation();
    if (!current) return;
    button.textContent = "Sending…";
    const answer = await api.runtime.sendMessage({
      type: "video",
      src: current.currentSrc || current.src || null,
    });
    button.textContent = answer?.ok ? "Added to Hiroi" : "Hiroi: not added";
    setTimeout(() => {
      button.textContent = "Download with Hiroi";
    }, 2000);
  });
})();
