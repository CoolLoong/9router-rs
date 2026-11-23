(() => {
  "use strict";

  const { select, request, notice, element, action } = window.nineRouterUi;
  const { provider, alias, modelKind } = select("provider-detail").dataset;
  const status = select("provider-detail-status");
  const connections = select("provider-detail-connections");
  const tests = select("provider-detail-tests");
  const models = select("provider-detail-models");
  const availability = select("provider-detail-availability");
  const filter = select("provider-detail-model-filter");
  const testAll = select("provider-detail-test-all");
  const discovery = select("provider-detail-discovery");
  const source = select("provider-detail-source");
  const discover = discovery.querySelector("button");
  const routing = select("provider-detail-routing");
  const routingFields = select("provider-detail-routing-fields");
  const roundRobin = select("provider-detail-round-robin");
  const sticky = select("provider-detail-sticky-requests");
  const providerApi = "/api/providers";
  const modelApi = "/api/models";
  const disabledApi = modelApi + "/disabled";
  const availabilityApi = modelApi + "/availability";
  const testKinds = new Set(["llm", "embedding", "image", "stt", "systemone"]);
  const modelKinds = new Set([...testKinds, "tts", "video", "music", "imageToText", "webSearch", "webFetch"]);
  const namespaces = [...new Set([provider, alias])];
  const accountPath = (id) => `${providerApi}/${encodeURIComponent(id)}`;
  const accountName = (account) => account.name || account.email || account.id;
  let accounts = [];
  let catalog = [];
  let custom = [];
  let live = [];
  let disabled = {};
  let probeRunning = false;
  let discoveryRunning = false;

  function failed(error) {
    if (error.status === 401) window.location.replace("/login");
    else notice(status, error.message, true);
  }

  async function run(control, work) {
    if (control.disabled) return;
    control.disabled = true;
    notice(status);
    try {
      await work();
    } catch (error) {
      failed(error);
    } finally {
      control.disabled = false;
      syncControls();
    }
  }

  function link(label, path) {
    const target = element("a", label, "button");
    target.href = path;
    return target;
  }

  function item(title, details, buttons = []) {
    const entry = element("article", null, "key-row");
    const meta = element("div", null, "key-meta");
    meta.append(element("h3", title));
    for (const detail of details.filter(Boolean)) meta.append(element("span", detail, "muted"));
    const controls = element("div", null, "key-actions");
    controls.append(...buttons);
    entry.append(meta, controls);
    return entry;
  }

  function showRows(target, rows, empty) {
    target.replaceChildren(...(rows.length ? rows : [element("p", empty, "muted")]));
  }

  function syncControls() {
    const active = accounts.some((account) => account.isActive !== false);
    testAll.disabled = !active || probeRunning;
    source.disabled = discover.disabled = !active || discoveryRunning;
    for (const button of connections.querySelectorAll("[data-account-probe]")) {
      button.disabled = probeRunning;
    }
  }

  async function testConnection(button, id) {
    if (probeRunning) return;
    await run(button, async () => {
      probeRunning = true;
      syncControls();
      try {
        const result = await request(accountPath(id) + "/test", "POST");
        notice(status, result.valid ? (result.error || "Connection test passed.") : (result.error || "Connection test failed."),
          !result.valid);
        await Promise.all([loadAccounts(), loadAvailability()]);
      } finally {
        probeRunning = false;
      }
    });
  }

  function renderAccounts() {
    const rows = accounts.map((account) => {
      const active = account.isActive !== false;
      const path = accountPath(account.id);
      const auth = { oauth: "OAuth", apikey: "API key", cookie: "Session cookie", access_token: "Access token" }[account.authType]
        || account.authType;
      const probe = action("Test", (event) => void testConnection(event.currentTarget, account.id));
      probe.dataset.accountProbe = "true";
      return item(accountName(account), [
        [auth, active ? "Enabled" : "Disabled", `Priority ${account.priority ?? 1}`].filter(Boolean).join(" · "),
        account.defaultModel ? `Default model ${account.defaultModel}` : "",
        account.testStatus ? `Connection status ${account.testStatus}` : "",
      ], [
        link("Edit", `/dashboard/providers/connections/${encodeURIComponent(account.id)}`),
        probe,
        ...(["claude", "codex"].includes(provider) && account.authType === "oauth" ? [
          action("Auto-ping schedule", (event) => void run(event.currentTarget, async () => {
            const { openQuotaAutoPing } = await import("/__9router-ui/quota-auto-ping.js");
            await openQuotaAutoPing(account, accountName(account), status);
          })),
        ] : []),
        action(active ? "Disable" : "Enable", (event) => void run(event.currentTarget, async () => {
          await request(path, "PUT", { isActive: !active });
          await Promise.all([loadAccounts(), loadAvailability()]);
        })),
        action("Delete", (event) => {
          if (!window.confirm(`Delete ${accountName(account)}?`)) return;
          void run(event.currentTarget, async () => {
            await request(path, "DELETE");
            await Promise.all([loadAccounts(), loadAvailability()]);
            notice(status, "Connection deleted.");
          });
        }, "button-danger"),
      ]);
    });
    showRows(connections, rows, "No connections configured.");
    const prior = source.value;
    source.replaceChildren(...accounts.filter((account) => account.isActive !== false).map((account) => {
      const option = element("option", accountName(account));
      option.value = account.id;
      return option;
    }));
    if ([...source.options].some((option) => option.value === prior)) source.value = prior;
    syncControls();
  }

  async function loadAccounts() {
    connections.setAttribute("aria-busy", "true");
    try {
      const result = await request(providerApi);
      accounts = (result.connections ?? []).filter((account) => account.provider === provider);
      renderAccounts();
    } finally {
      connections.setAttribute("aria-busy", "false");
    }
  }

  function applyDisabled() {
    return new Set(namespaces.flatMap((name) => disabled[name] ?? []).filter((id) => typeof id === "string"));
  }

  function modelRows() {
    const known = new Map();
    for (const [entries, discovered] of [[live, true], [catalog, false], [custom, false]]) {
      for (const entry of entries) {
        const id = entry?.model ?? entry?.id ?? (typeof entry === "string" ? entry : "");
        if (typeof id !== "string" || !id) continue;
        const name = typeof entry?.name === "string" && entry.name ? entry.name : id;
        const prior = known.get(id);
        const declared = entry?.kind || entry?.type;
        const kind = modelKinds.has(declared) ? declared : modelKind;
        known.set(id, { id, name, kind, discovered: discovered && (prior?.discovered ?? true) });
      }
    }
    const unavailable = applyDisabled();
    for (const id of unavailable) {
      if (!known.has(id)) known.set(id, { id, name: id, kind: modelKind });
    }
    const term = filter.value.trim().toLowerCase();
    const rows = [];
    for (const entry of [...known.values()].sort((a, b) => a.name.localeCompare(b.name))) {
      const { id, name, kind, discovered } = entry;
      if (term && ![id, name].some((value) => String(value).toLowerCase().includes(term))) continue;
      const hidden = unavailable.has(id);
      const row = element("tr");
      const title = element("th");
      title.scope = "row";
      title.append(element("span", name), document.createElement("br"), element("code", `${alias}/${id}`));
      const operations = element("td");
      const buttons = element("div", null, "key-actions");
      if (testKinds.has(kind)) {
        const probe = action("Test", (event) => void run(event.currentTarget, async () => {
          const result = await request(modelApi + "/test", "POST", { model: `${alias}/${id}`, kind });
          notice(status, result.ok ? "Model test passed." : (result.error || "Model test failed."), !result.ok);
          await loadAvailability();
        }));
        probe.disabled = hidden;
        buttons.append(probe);
      }
      buttons.append(action(hidden ? "Enable" : "Disable", (event) => void run(event.currentTarget, async () => {
        if (hidden) {
          for (const namespace of namespaces.filter((key) => (disabled[key] ?? []).includes(id))) {
            await request(disabledApi + "?" + new URLSearchParams({ providerAlias: namespace, id }), "DELETE");
          }
        } else {
          await request(disabledApi, "POST", { providerAlias: alias, ids: [id] });
        }
        await loadModels();
      })));
      if (discovered) {
        buttons.append(action("Add custom model", (event) => void run(event.currentTarget, async () => {
          await request(modelApi + "/custom", "POST", { providerAlias: alias, id, name, type: kind });
          await loadModels();
          notice(status, "Custom model added.");
        })));
      }
      operations.append(buttons);
      row.append(title, element("td", hidden ? "Disabled" : (discovered ? "Discovered" : "Enabled")), operations);
      rows.push(row);
    }
    if (!rows.length) {
      const row = element("tr");
      const cell = element("td", term ? "No matching models." : "No models in the catalog.", "muted");
      cell.colSpan = 3;
      row.append(cell);
      rows.push(row);
    }
    models.replaceChildren(...rows);
  }

  async function loadModels() {
    models.setAttribute("aria-busy", "true");
    try {
      const [list, additions, settings] = await Promise.all([
        request(modelApi), request(modelApi + "/custom"), request(disabledApi),
      ]);
      catalog = (list.models ?? []).filter((model) => namespaces.some((namespace) =>
        model.provider === namespace || model.routedModel?.startsWith(namespace + "/")));
      custom = (additions.models ?? []).filter((model) => namespaces.includes(model.providerAlias));
      disabled = settings.disabled ?? {};
      modelRows();
    } finally {
      models.setAttribute("aria-busy", "false");
    }
  }

  async function loadAvailability() {
    availability.setAttribute("aria-busy", "true");
    try {
      const data = await request(availabilityApi);
      const rows = (data.models ?? []).filter((model) => model.provider === provider).map((model) => {
        const title = model.model === "__all" ? "All models" : model.model;
        const until = model.until ? new Date(model.until) : null;
        const info = [model.connectionName || model.connectionId, model.status, model.lastError];
        if (until && Number.isFinite(until.getTime())) info.push(`Until ${until.toLocaleString()}`);
        const buttons = model.status === "cooldown" ? [action("Clear cooldown", (event) =>
          void run(event.currentTarget, async () => {
            await request(availabilityApi, "POST", { action: "clearCooldown", provider, model: model.model });
            await Promise.all([loadAccounts(), loadAvailability()]);
          }))] : [];
        return item(title, info, buttons);
      });
      showRows(availability, rows, "No unavailable models.");
    } finally {
      availability.setAttribute("aria-busy", "false");
    }
  }

  async function loadRouting() {
    const { providerStrategies } = await request("/api/settings");
    const current = providerStrategies?.[provider] ?? {};
    roundRobin.checked = current.fallbackStrategy === "round-robin";
    const requests = Number(current.stickyRoundRobinLimit);
    sticky.value = Number.isInteger(requests) && requests >= 1 ? requests : 1;
    sticky.disabled = !roundRobin.checked;
    routingFields.disabled = false;
  }

  testAll.addEventListener("click", (event) => {
    if (probeRunning) return;
    void run(event.currentTarget, async () => {
      probeRunning = true;
      syncControls();
      tests.replaceChildren();
      try {
        const result = await request(providerApi + "/test-batch", "POST", { mode: "provider", providerId: provider });
        showRows(tests, (result.results ?? []).map((row) => item(row.connectionName || row.connectionId,
          [row.valid ? "Passed" : "Failed", row.error])), "No active connections to test.");
        const summary = result.summary;
        if (summary) notice(status, `${summary.passed} of ${summary.total} connection tests passed.`, summary.failed > 0);
        await Promise.all([loadAccounts(), loadAvailability()]);
      } finally {
        probeRunning = false;
      }
    });
  });

  discovery.addEventListener("submit", (event) => {
    event.preventDefault();
    if (!source.value || discoveryRunning) return;
    void run(discover, async () => {
      discoveryRunning = true;
      syncControls();
      try {
        const data = await request(accountPath(source.value) + "/models");
        live = (Array.isArray(data.models) ? data.models : []).map(entry => {
          if (provider !== "gemini" || typeof entry?.name !== "string" || !entry.name.startsWith("models/")) {
            return entry;
          }
          const id = entry.name.slice("models/".length);
          const methods = Array.isArray(entry.supportedGenerationMethods) ? entry.supportedGenerationMethods : [];
          const kind = methods.includes("embedContent") && !methods.includes("generateContent") ? "embedding" : entry.kind;
          return { ...entry, id, name: entry.displayName || id, kind };
        });
        modelRows();
        notice(status, data.warning || `${live.length} models discovered.`);
      } finally {
        discoveryRunning = false;
      }
    });
  });

  roundRobin.addEventListener("change", () => { sticky.disabled = !roundRobin.checked; });
  routing.addEventListener("submit", (event) => {
    event.preventDefault();
    const override = roundRobin.checked ? {
      fallbackStrategy: "round-robin", stickyRoundRobinLimit: sticky.valueAsNumber,
    } : null;
    void run(routing.querySelector("button"), async () => {
      const { providerStrategies } = await request("/api/settings");
      const updated = { ...providerStrategies };
      if (override) updated[provider] = override;
      else delete updated[provider];
      await request("/api/settings", "PATCH", { providerStrategies: updated });
      notice(status, "Connection routing saved.");
    });
  });

  const reload = () => Promise.all([loadAccounts(), loadModels(), loadAvailability(), loadRouting()]);
  document.addEventListener("provider-accounts-updated", () => void loadAccounts().catch(failed));
  select("provider-detail-reload").addEventListener("click", (event) => void run(event.currentTarget, reload));
  filter.addEventListener("input", modelRows);
  reload().catch(failed);
})();
