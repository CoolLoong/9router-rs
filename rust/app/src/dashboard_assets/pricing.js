(() => {
  "use strict";

  const { element, notice, request, select } = window.nineRouterUi;
  const provider = select("pricing-provider");
  const filter = select("pricing-filter");
  const output = select("pricing-status");
  const rows = select("pricing-rates");
  const save = select("pricing-save");
  const reset = select("pricing-reset");
  const endpoint = "/api/pricing";
  const fields = [
    ["input", "Input"], ["output", "Output"], ["cached", "Cached"],
    ["reasoning", "Reasoning"], ["cache_creation", "Cache creation"],
  ];
  const pending = new Map();
  let catalog = {};
  let busy = false;

  function buttons() {
    save.disabled = busy || pending.size === 0;
    reset.disabled = busy || Object.keys(catalog).length === 0;
    provider.disabled = busy;
    filter.disabled = busy;
    for (const input of rows.querySelectorAll("input")) input.disabled = busy;
  }

  function render() {
    const name = provider.value;
    const models = Object.entries(catalog[name] ?? {});
    const term = filter.value.trim().toLowerCase();
    rows.replaceChildren();
    for (const [model, rates] of models.sort(([a], [b]) => a.localeCompare(b))) {
      if (term && !model.toLowerCase().includes(term)) continue;
      const row = element("tr");
      const label = element("th", model);
      label.scope = "row";
      row.append(label);
      for (const [field, description] of fields) {
        const input = element("input");
        input.type = "number";
        input.min = "0";
        input.step = "any";
        input.setAttribute("aria-label", `${description} rate for ${model}`);
        const edited = pending.get(name)?.get(model);
        input.value = (edited ?? rates)?.[field] ?? "";
        input.placeholder = "—";
        input.addEventListener("change", () => {
          if (!input.validity.valid || (input.value !== "" && !Number.isFinite(input.valueAsNumber))) {
            input.reportValidity();
            return;
          }
          const models = pending.get(name) ?? new Map();
          const draft = { ...(models.get(model) ?? rates) };
          if (input.value === "") delete draft[field];
          else draft[field] = input.valueAsNumber;
          if (fields.every(([key]) => draft[key] === rates?.[key])) models.delete(model);
          else models.set(model, draft);
          if (models.size) pending.set(name, models);
          else pending.delete(name);
          buttons();
        });
        const cell = element("td");
        cell.append(input);
        row.append(cell);
      }
      rows.append(row);
    }
    if (!rows.children.length) {
      const row = element("tr");
      const cell = element("td", "No matching models", "muted");
      cell.colSpan = fields.length + 1;
      row.append(cell);
      rows.append(row);
    }
    buttons();
  }

  function applyCatalog(value) {
    catalog = value;
    const names = Object.keys(catalog).sort();
    const previous = provider.value;
    provider.replaceChildren(...names.map(name => {
      const option = element("option", name);
      option.value = name;
      return option;
    }));
    if (names.includes(previous)) provider.value = previous;
    const modelCount = Object.values(catalog).reduce((count, values) => count + Object.keys(values ?? {}).length, 0);
    select("pricing-count").textContent = `${names.length} providers · ${modelCount} models`;
    render();
  }

  async function mutate(action, success) {
    if (busy) return;
    busy = true;
    notice(output);
    buttons();
    try {
      await action();
      pending.clear();
      applyCatalog(await request(endpoint));
      notice(output, success);
    } catch (error) {
      notice(output, error.message, true);
    } finally {
      busy = false;
      buttons();
    }
  }

  provider.addEventListener("change", render);
  filter.addEventListener("input", render);
  select("pricing-form").addEventListener("submit", event => {
    event.preventDefault();
    if (!pending.size) return;
    const data = Object.fromEntries([...pending].map(([name, models]) => [name, Object.fromEntries(models)]));
    void mutate(() => request(endpoint, "PATCH", data), "Pricing saved.");
  });
  reset.addEventListener("click", () => {
    if (!window.confirm("Reset every pricing override to its default rate?")) return;
    void mutate(() => request(endpoint, "DELETE"), "Default rates restored.");
  });
  notice(output, "Loading pricing…");
  request(endpoint).then(applyCatalog).then(() => notice(output))
    .catch(error => notice(output, error.message, true));
})();
