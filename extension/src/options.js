// The options page: every option of OPTIONS, saved as it changes, with
// a search that keeps the options whose label or help has every word.
"use strict";

const list = document.getElementById("options");
const rows = [];

(async () => {
  const values = await getOptions();
  for (const option of OPTIONS) {
    const row = document.createElement("div");
    row.className = "option";
    const id = `option-${option.key}`;
    const label = document.createElement("label");
    label.htmlFor = id;
    const input = document.createElement("input");
    input.id = id;
    if (option.type === "bool") {
      input.type = "checkbox";
      input.checked = values[option.key];
      label.append(input, " ", option.label);
      row.append(label);
    } else {
      input.type = option.type === "number" ? "number" : "text";
      if (option.type === "number") input.min = "0";
      input.value = values[option.key];
      label.textContent = option.label;
      row.append(label, input);
    }
    const help = document.createElement("p");
    help.className = "help";
    help.textContent = option.help;
    row.append(help);
    input.addEventListener("change", () => {
      const value =
        option.type === "bool" ? input.checked : option.type === "number" ? Math.max(0, Number(input.value) || 0) : input.value.trim();
      api.storage.local.set({ [option.key]: value });
    });
    rows.push({ row, text: `${option.label} ${option.help} ${option.key}`.toLowerCase() });
    list.append(row);
  }
})();

document.getElementById("search").addEventListener("input", (event) => {
  const words = event.target.value.toLowerCase().split(/\s+/).filter(Boolean);
  let shown = 0;
  for (const { row, text } of rows) {
    const match = words.every((word) => text.includes(word));
    row.hidden = !match;
    shown += match ? 1 : 0;
  }
  document.getElementById("nothing").hidden = shown > 0;
});
