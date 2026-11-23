(() => {
  "use strict";

  const { select, request, notice, element, action } = window.nineRouterUi;
  const status = select("management-status");
  const field = (id) => select(id).value.trim();
  const providerApi = "/api/providers";
  const providerPath = "/dashboard/providers";
  const endpoint = (id) => `${providerApi}/${encodeURIComponent(id)}`;
  const modelsPath = (alias) => `/dashboard/models?${new URLSearchParams({ provider: alias })}`;

  function failure(error) {
    if (error.status === 401 && error.message === "Unauthorized") window.location.replace("/login");
    else notice(status, error.message, true);
  }

  async function task(control, work) {
    if (control.disabled) return;
    control.disabled = true;
    notice(status);
    try {
      await work();
    } catch (error) {
      failure(error);
    } finally {
      control.disabled = false;
    }
  }

  function onForm(id, handler) {
    const form = select(id);
    const button = form.querySelector('[type="submit"]');
    form.addEventListener("submit", (event) => {
      event.preventDefault();
      void task(button, handler);
    });
  }

  function link(text, href, className = "button") {
    const value = element("a", text, className);
    value.href = href;
    return value;
  }

  function providerList() {
    const lookup = new Map([...select("provider-directory").options]
      .map((option) => [option.value, { alias: option.dataset.alias, name: option.textContent }]));
    const list = select("provider-list");
    const search = select("provider-search");
    let connections = [];

    function render() {
      const term = search.value.trim().toLowerCase();
      const entries = connections.filter((connection) => [
        connection.name,
        connection.provider,
        lookup.get(connection.provider)?.name,
      ].some((value) => String(value || "").toLowerCase().includes(term)));

      list.replaceChildren(...(entries.length ? entries.map((connection) => {
        const provider = connection.provider;
        const providerEntry = lookup.get(provider);
        const meta = element("div", null, "key-meta");
        const active = connection.isActive !== false;
        const details = connection.providerSpecificData ?? {};
        const providerName = details.nodeName || providerEntry?.name || provider;
        const auth = { oauth: "OAuth", apikey: "API key", cookie: "Cookie" }[connection.authType] || "Connection";
        const health = active && ({ active: "Test passed", error: "Test failed", unavailable: "Unavailable" }[connection.testStatus]);
        meta.append(
          element("h3", connection.name || providerName),
          element("span", [providerName, auth, active ? "Active" : "Disabled", health].filter(Boolean).join(" · "), "muted"),
          element("span", `Priority ${connection.priority ?? 1}${details.proxyPoolId ? " · Proxy pool" : ""}`, "key-state"),
        );
        const actions = element("div", null, "key-actions");
        const id = connection.id;
        actions.append(
          link("Edit", `${providerPath}/connections/${encodeURIComponent(id)}`),
          link("Models", modelsPath(providerEntry?.alias || provider)),
          action(active ? "Disable" : "Enable", (event) => task(event.currentTarget, async () => {
            await request(endpoint(id), "PUT", { isActive: !active });
            await reload();
          })),
          action("Delete", (event) => {
            if (window.confirm("Delete this connection?")) {
              void task(event.currentTarget, async () => {
                await request(endpoint(id), "DELETE");
                await reload();
                notice(status, "Connection deleted.");
              });
            }
          }, "button-danger"),
        );
        const row = element("article", null, "key-row");
        row.append(meta, actions);
        return row;
      }) : [element("p", term ? "No matching connections." : "No connections configured.", "muted")]));
    }

    async function reload() {
      list.setAttribute("aria-busy", "true");
      try {
        const result = await request(providerApi);
        connections = result.connections ?? [];
        render();
      } finally {
        list.setAttribute("aria-busy", "false");
      }
    }

    async function loadCompatibleNodes() {
      const list = select("provider-node-list");
      try {
        const { nodes } = await request("/api/provider-nodes");
        const rows = (nodes ?? []).filter(node => ["openai-compatible", "anthropic-compatible", "custom-embedding"].includes(node.type))
          .map(node => {
            const id = encodeURIComponent(node.id);
            const meta = element("div", null, "key-meta");
            const embedding = node.type === "custom-embedding";
            const protocol = embedding ? "Embedding" : node.type === "anthropic-compatible" ? "Anthropic"
              : node.apiType === "responses" ? "Responses" : "Chat Completions";
            meta.append(element("h3", node.name || node.id), element("span", `${protocol} · ${node.prefix || node.id}`, "muted"));
            const actions = element("div", null, "key-actions");
            const directory = embedding ? "/dashboard/media-providers/embedding" : providerPath;
            actions.append(link("Connections", `${directory}/${id}`), link("Edit endpoint", `${providerPath}/nodes/${id}`));
            const row = element("article", null, "key-row");
            row.append(meta, actions);
            return row;
          });
        list.replaceChildren(...(rows.length ? rows : [element("p", "No compatible endpoints configured.", "muted")]));
      } finally {
        list.setAttribute("aria-busy", "false");
      }
    }

    search.addEventListener("input", render);
    providerBatchTest(reload);
    reload().catch(failure);
    loadCompatibleNodes().catch(failure);
  }

  function providerBatchTest(reload) {
    const form = select("provider-batch-form");
    const group = select("provider-batch-group");
    const button = select("provider-batch-run");
    const status = select("provider-batch-status");
    const dialog = select("provider-batch-dialog");
    const summary = select("provider-batch-summary");
    const results = select("provider-batch-results");
    const labels = { apikey: "API key", oauth: "OAuth", free: "Free tier" };

    select("provider-batch-close").addEventListener("click", () => dialog.close());
    form.addEventListener("submit", async event => {
      event.preventDefault();
      const mode = group.value;
      if (button.disabled || !Object.hasOwn(labels, mode)) return;
      group.disabled = button.disabled = true;
      form.setAttribute("aria-busy", "true");
      notice(status, `Testing active ${labels[mode]} connections…`);
      try {
        const response = await request(`${providerApi}/test-batch`, "POST", { mode, providerId: null });
        const total = response.summary;
        const rows = Array.isArray(response.results) ? response.results : [];
        select("provider-batch-results-title").textContent = `${labels[mode]} connection test results`;
        summary.textContent = total && total.total > 0
          ? `${total.total} tested · ${total.passed} passed · ${total.failed} failed`
          : "No active connections found for this group.";
        results.replaceChildren(...rows.map(row => {
          const item = element("div", null, "key-row");
          item.setAttribute("role", "listitem");
          const details = element("div", null, "key-meta");
          details.append(element("strong", row.connectionName || row.connectionId || "Connection"),
            element("span", row.provider, "muted"));
          const latency = Number.isFinite(row.latencyMs) ? ` · ${row.latencyMs} ms` : "";
          item.append(details, element("span", `${row.valid === true ? "Passed" : "Failed"}${latency}`, "key-state"));
          return item;
        }));
        notice(status);
        dialog.showModal();
        await reload().catch(failure);
      } catch (error) {
        if (error.status === 401 && error.message === "Unauthorized") window.location.replace("/login");
        else notice(status, "Could not test provider connections.", true);
      } finally {
        group.disabled = button.disabled = false;
        form.setAttribute("aria-busy", "false");
      }
    });
  }

  function suggestedModels(provider, options) {
    if (!options?.catalogUrl || !options.catalogType || !options.alias) return;
    select("connection-suggestions").hidden = false;
    const button = select("connection-suggestions-load");
    const output = select("connection-suggestions-status");
    const list = select("connection-suggestions-list");
    const alias = options.alias;
    const params = new URLSearchParams({ url: options.catalogUrl, type: options.catalogType });
    const modelApi = "/api/models";
    button.addEventListener("click", event => task(event.currentTarget, async () => {
      const [suggestions, existing, named, disabled] = await Promise.all([
        request(`${providerApi}/suggested-models?${params}`),
        request(modelApi), request(`${modelApi}/alias`), request(`${modelApi}/disabled`),
      ]);
      const added = new Set(Object.values(named.aliases ?? {}));
      for (const model of existing.models ?? []) {
        added.add(model.fullModel);
        added.add(model.routedModel);
      }
      const hidden = new Set([disabled.disabled?.[alias], disabled.disabled?.[provider]]
        .flatMap(ids => Array.isArray(ids) ? ids : []));
      const rows = [];
      for (const model of suggestions.data ?? []) {
        const id = model?.id;
        if (typeof id !== "string" || !id.trim() || hidden.has(id)) continue;
        const full = `${alias}/${id}`;
        if (added.has(full) || added.has(`${provider}/${id}`)) continue;
        added.add(full);
        const row = element("article", null, "key-row");
        const detail = element("div", null, "key-meta");
        const context = Number(model.contextLength);
        detail.append(element("strong", typeof model.name === "string" && model.name ? model.name : id),
          element("code", id));
        if (Number.isFinite(context) && context > 0) {
          detail.append(element("span", `${new Intl.NumberFormat().format(context)} context tokens`, "muted"));
        }
        row.append(detail, action("Add model", event => task(event.currentTarget, async () => {
          await request(`${modelApi}/custom`, "POST", { providerAlias: alias, id, type: "llm" });
          row.remove();
          if (!list.childElementCount) output.textContent = "No new models in this catalog.";
          notice(status, `Added ${full}.`);
        })));
        rows.push(row);
      }
      list.replaceChildren(...rows);
      output.textContent = rows.length ? "" : "No new models in this catalog.";
    }));
  }

  function connectionEditor() {
    const form = select("connection-form");
    const id = form.dataset.connectionId;
    const provider = select("connection-provider");
    const pool = select("connection-pool");
    const secret = select("connection-key");
    const credential = select("connection-secret-fields");
    const host = select("connection-host-fields");
    const poolStatus = select("connection-pool-status");
    const supported = new Set([...provider.options].map((option) => option.value));
    const directory = new Map([...(id ? select("provider-directory").options : provider.options)]
      .map((option) => [option.value, { ...option.dataset, name: option.textContent }]));
    const headersEditor = select("connection-headers");
    const headersButton = select("connection-headers-form").querySelector('[type="submit"]');
    let existing;

    function controls() {
      const providerId = existing?.provider || provider.value;
      const special = existing && (existing.authType !== "apikey"
        || !supported.has(providerId) && !/^(openai-compatible-|anthropic-compatible-|custom-embedding-)/.test(providerId));
      const ollama = providerId === "ollama-local";
      credential.hidden = Boolean(special);
      secret.required = !id && !ollama;
      host.hidden = !ollama;
    }

    function setKeepPool(binding) {
      const saved = binding && [...pool.options].some((option) => option.value === binding);
      if (binding && !saved) {
        const current = element("option", "Current pool");
        current.value = binding;
        pool.append(current);
      }
      pool.value = binding || "";
    }

    function showExisting(connection) {
      existing = connection;
      const providerName = directory.get(connection.provider);
      const details = connection.providerSpecificData ?? {};
      select("connection-provider-name").textContent = details.nodeName || providerName?.name || connection.provider;
      select("connection-name").value = connection.name || "";
      select("connection-priority").value = Number(connection.priority) || 1;
      select("connection-default-model").value = connection.defaultModel || "";
      select("connection-active").checked = connection.isActive !== false;
      select("connection-host").value = connection.provider === "ollama-local" ? details.baseUrl || "" : "";
      select("connection-models").href = modelsPath(providerName?.alias || connection.provider);
      select("connection-advanced").href = `${providerPath}/${encodeURIComponent(connection.provider)}`;
      setKeepPool(details.proxyPoolId);
      controls();
    }

    async function loadPools() {
      try {
        const { proxyPools } = await request("/api/proxy-pools");
        for (const entry of proxyPools ?? []) {
          const item = element("option", `${entry.name || entry.id}${entry.isActive === false ? " (inactive)" : ""}`);
          item.value = entry.id;
          pool.append(item);
        }
        pool.disabled = false;
      } catch {
        poolStatus.textContent = "Proxy pools could not be loaded.";
        poolStatus.hidden = false;
      }
    }

    async function loadNodes() {
      try {
        const { nodes } = await request("/api/provider-nodes");
        for (const entry of nodes ?? []) {
          const item = element("option", entry.name || entry.id);
          item.value = entry.id;
          provider.append(item);
        }
        const requested = new URLSearchParams(window.location.search).get("provider");
        if (requested) provider.value = requested;
      } catch (error) {
        failure(error);
      }
    }

    async function initialize() {
      await loadPools();
      if (id) {
        const { connection } = await request(endpoint(id));
        showExisting(connection);
        suggestedModels(connection.provider, directory.get(connection.provider));
      } else {
        provider.required = true;
        await loadNodes();
        controls();
      }
      select("connection-fields").disabled = false;
      if (id) {
        const data = await request(`${endpoint(existing.provider)}/overrides`);
        headersEditor.value = JSON.stringify(data.headers, null, 2);
        select("connection-builtin-headers").textContent = JSON.stringify(data.builtinHeaders, null, 2);
        headersEditor.disabled = headersButton.disabled = false;
      }
    }

    onForm("connection-headers-form", async () => {
      let headers;
      try { headers = JSON.parse(headersEditor.value || "{}"); }
      catch { throw new Error("Enter a JSON object with header names and values."); }
      const saved = await request(`${endpoint(existing.provider)}/overrides`, "PUT", { headers });
      headersEditor.value = JSON.stringify(saved.headers, null, 2);
      notice(status, "Provider headers saved.");
    });

    provider.addEventListener("change", () => {
      const info = directory.get(provider.value);
      if (!field("connection-name") && info) select("connection-name").value = info.name;
      controls();
    });

    onForm("connection-form", async () => {
      const name = field("connection-name");
      if (!name) throw new Error("Enter a connection name.");
      const body = {
        name,
        priority: select("connection-priority").valueAsNumber,
        defaultModel: field("connection-default-model"),
      };
      if (!pool.disabled && (!id || pool.value !== (existing?.providerSpecificData?.proxyPoolId || ""))) {
        body.proxyPoolId = pool.value || null;
      }
      if (id) body.isActive = select("connection-active").checked;
      else body.provider = provider.value;
      if (secret.value && !credential.hidden) body.apiKey = secret.value;
      if (!host.hidden && field("connection-host")) {
        body.providerSpecificData = { baseUrl: field("connection-host") };
      }
      const { connection } = await request(id ? endpoint(id) : providerApi, id ? "PUT" : "POST", body);
      secret.value = "";
      if (!id) {
        window.location.assign(`${providerPath}/connections/${encodeURIComponent(connection.id)}`);
        return;
      }
      showExisting(connection);
      notice(status, "Connection saved.");
    });

    initialize().catch(failure);
  }

  if (document.body.dataset.page === "providers") providerList();
  if (document.body.dataset.page === "connection") connectionEditor();
})();
