// The grab page: collects links from a tab, lets the user choose, and
// hands the choice to Hiroi.
"use strict";

const tabId = Number(new URLSearchParams(location.search).get("tab"));
const rows = [];
const $ = (id) => document.getElementById(id);

async function load() {
  const tab = await api.tabs.get(tabId);
  $("source").textContent = tab.url;
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
  const kinds = [...new Set(rows.map((row) => row.kind))].filter((kind) => kind !== "page").sort();
  for (const kind of kinds) $("kind").append(new Option(kind, kind));
  render();
}

function visible() {
  const kind = $("kind").value;
  const needle = $("filter").value.toLowerCase();
  const pages = $("pages").checked;
  return rows.filter(
    (row) =>
      (pages || row.kind !== "page") &&
      (!kind || row.kind === kind) &&
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
    const name = decodeURIComponent(new URL(row.url).pathname.split("/").pop() || row.url);
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

$("go").addEventListener("click", async () => {
  $("go").disabled = true;
  for (const row of rows.filter((row) => row.selected)) {
    row.status = "sending…";
    render();
    const answer = await sendToHiroi(row.url, row.page);
    row.status = answer.ok ? `job ${answer.job}` : answer.error;
    row.state = answer.ok ? "ok" : "error";
    row.selected = !answer.ok;
  }
  render();
});

load();
