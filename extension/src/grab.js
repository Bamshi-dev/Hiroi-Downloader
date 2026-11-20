// The grab page: collects links from a tab, lets the user choose, and
// hands the choice to Hiroi.
"use strict";

const query = new URLSearchParams(location.search);
const tabId = Number(query.get("tab"));
/** From the context menu: media, image, or files, chosen up front. */
const preset = query.get("kind") || "";
const rows = [];
const $ = (id) => document.getElementById(id);
let pageTitle = "";

async function load() {
  const tab = await api.tabs.get(tabId);
  $("source").textContent = tab.url;
  pageTitle = tab.title || "";
  let results;
  try {
    results = await api.scripting.executeScript({
      target: { tabId, allFrames: true },
      files: ["collect.js"],
    });
  } catch (error) {
    $("source").textContent = `Cannot read this page: ${error.message}`;
    return;
  }
  const seen = new Set();
  for (const { result } of results) {
    for (const link of result || []) {
      if (seen.has(link.url)) continue;
      seen.add(link.url);
      const kind = kindOf(link.url) || (link.where === "anchor" ? "page" : link.where === "image" ? "image" : "other");
      rows.push({ ...link, kind, page: tab.url, selected: false, status: "" });
    }
  }
  // What the tab played but the page does not show.
  for (const entry of (await api.runtime.sendMessage({ type: "media", tabId })) || []) {
    if (seen.has(entry.url)) continue;
    seen.add(entry.url);
    rows.push({ url: entry.url, where: "network", text: "played by the page", kind: entry.kind, page: tab.url, selected: false, status: "" });
  }
  for (const [group, kinds] of Object.entries(KIND_GROUPS)) {
    if (rows.some((row) => kinds.includes(row.kind))) $("kind").append(new Option(`${group} (all)`, `group:${group}`));
  }
  const kinds = [...new Set(rows.map((row) => row.kind))].filter((kind) => kind !== "page").sort();
  for (const kind of kinds) $("kind").append(new Option(kind, kind));
  if (preset && KIND_GROUPS[preset]) {
    $("kind").value = `group:${preset}`;
    for (const row of visible()) row.selected = true;
  }
  render();
}

function visible() {
  const kind = $("kind").value;
  const group = kind.startsWith("group:") ? KIND_GROUPS[kind.slice("group:".length)] : null;
  const needle = $("filter").value.toLowerCase();
  const pages = $("pages").checked;
  return rows.filter(
    (row) =>
      (pages || row.kind !== "page") &&
      (!kind || (group ? group.includes(row.kind) : row.kind === kind)) &&
      (!needle || row.url.toLowerCase().includes(needle) || row.text.toLowerCase().includes(needle)),
  );
}

function render() {
  const body = $("links");
  body.replaceChildren();
  for (const row of visible()) {
    const tr = document.createElement("tr");
    const box = document.createElement("input");
    box.type = "checkbox";
    box.checked = row.selected;
    box.addEventListener("change", () => {
      row.selected = box.checked;
      updateButton();
    });
    const name = displayName(row.url);
    // textContent only: page text must never become markup here.
    for (const [value, className] of [[null], [row.kind], [name, "name"], [row.text], [row.status, `status ${row.state || ""}`]]) {
      const td = document.createElement("td");
      if (value === null) td.append(box);
      else td.textContent = value;
      if (className) td.className = className;
      tr.append(td);
    }
    tr.title = row.url;
    body.append(tr);
  }
  updateButton();
}

function updateButton() {
  const count = rows.filter((row) => row.selected).length;
  $("go").disabled = count === 0;
  $("go").textContent = count ? `Download selected (${count})` : "Download selected";
}

$("all").addEventListener("change", (event) => {
  for (const row of visible()) row.selected = event.target.checked;
  render();
});
for (const id of ["kind", "filter", "pages"]) $(id).addEventListener("input", render);

// Everything chosen goes over in one message and becomes one group.
$("go").addEventListener("click", async () => {
  $("go").disabled = true;
  const chosen = rows.filter((row) => row.selected);
  for (const row of chosen) row.status = "sending…";
  render();
  const items = chosen.map((row) => ({ url: row.url, referrer: row.page }));
  const answer =
    items.length === 1
      ? await sendToHiroi(items[0])
      : await sendBatch(items, pageTitle, $("group").value.trim());
  for (const row of chosen) {
    row.state = answer.ok ? "ok" : "error";
    row.status = answer.ok ? "sent" : answer.error || "not added";
    row.selected = !answer.ok;
  }
  const notes = [];
  if (answer.group) notes.push(`Added as the group "${answer.group}".`);
  if (answer.errors?.length) notes.push(`Not added: ${answer.errors.join("; ")}`);
  if (notes.length) $("source").textContent = notes.join(" ");
  render();
});

load();
