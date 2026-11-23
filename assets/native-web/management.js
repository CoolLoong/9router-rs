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

  if (document.body.dataset.page === "connection") connectionEditor();
})();
