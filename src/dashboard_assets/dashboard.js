(() => {
  "use strict";

  const select = (id) => document.getElementById(id);
  const themeMedia = window.matchMedia("(prefers-color-scheme: dark)");

  function theme() {
    try {
      const selected = JSON.parse(localStorage.getItem("theme"))?.state?.theme;
      if (selected === "light" || selected === "dark") return selected;
    } catch {}
    return "system";
  }

  function applyTheme(value) {
    document.documentElement.dataset.theme = value === "system" ? (themeMedia.matches ? "dark" : "light") : value;
  }

  function setTheme(value) {
    try {
      localStorage.setItem("theme", JSON.stringify({ state: { theme: value }, version: 0 }));
    } catch {}
    applyTheme(value);
  }

  applyTheme(theme());
  themeMedia.addEventListener("change", () => applyTheme(theme()));

  async function request(path, method = "GET", body) {
    const response = await fetch(path, {
      method,
      cache: "no-store",
      ...(body === undefined ? {} : {
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(body),
      }),
    });
    const data = await response.json().catch(() => ({}));
    if (!response.ok) {
      const error = new Error(data.error || data.message || `Request failed (${response.status}).`);
      error.status = response.status;
      error.retryAfter = data.retryAfter;
      throw error;
    }
    return data;
  }

  function notice(output, message = "", failed = false) {
    output.textContent = message;
    output.hidden = !message;
    output.dataset.error = failed ? "true" : "false";
  }

  function login() {
    if (document.body.dataset.ready === "true") {
      window.location.replace("/dashboard");
      return;
    }

    const form = select("login-form");
    const password = select("password");
    const submit = select("login-submit");
    const output = select("login-status");

    form.addEventListener("submit", async (event) => {
      event.preventDefault();
      if (submit.disabled || form.hidden || form.parentElement.hidden) return;
      submit.disabled = true;
      notice(output);
      try {
        await request("/api/auth/login", "POST", { password: password.value });
        password.value = "";
        window.location.replace("/dashboard");
      } catch (error) {
        notice(output, error.message, true);
        const delay = Number(error.retryAfter);
        if (Number.isFinite(delay) && delay > 0) {
          window.setTimeout(() => { submit.disabled = false; }, delay * 1000);
        } else {
          submit.disabled = false;
          password.focus();
          password.select();
        }
      }
    });
  }

  function element(tag, text, className) {
    const node = document.createElement(tag);
    if (text != null) node.textContent = text;
    if (className) node.className = className;
    return node;
  }

  function action(text, handler, className = "") {
    const button = element("button", text, `button ${className}`);
    button.type = "button";
    button.addEventListener("click", handler);
    return button;
  }

  function cliBaseUrl(value) {
    try {
      const url = new URL(value);
      if (!["http:", "https:"].includes(url.protocol) || url.username || url.password || url.search || url.hash) return "";
      return url.origin + url.pathname.replace(/\/+$/, "");
    } catch { return ""; }
  }

  function cliEndpoints(input, output, { local = window.location.origin, normalize = cliBaseUrl } = {}) {
    const storageKey = "9router.cliToolEndpointPresets";
    const eventName = "9router:endpoint-presets-changed";
    const known = new Set();
    const builtins = new Map();
    const choices = element("select");
    choices.setAttribute("aria-label", "Endpoint presets");
    const save = action("Save endpoint", () => {
      const url = normalize(input.value);
      if (!url) return;
      const name = window.prompt("Endpoint name", new URL(url).host)?.trim();
      if (name && write(url, name)) notice(output, "Endpoint preset saved.");
      else if (name) notice(output, "Could not save the endpoint preset.", true);
    });
    const remove = action("Delete preset", () => {
      const url = normalize(input.value);
      if (persist(read().filter((item) => item.baseUrl !== url))) {
        notice(output, "Endpoint preset deleted.");
      } else {
        notice(output, "Could not delete the endpoint preset.", true);
      }
    });
    const row = element("div", null, "key-actions");
    row.append(choices, save, remove);
    input.after(row);

    function read() {
      try {
        const rows = JSON.parse(localStorage.getItem(storageKey) || "[]");
        return Array.isArray(rows) ? rows.flatMap((row) => {
          const name = typeof row?.name === "string" ? row.name.trim() : "";
          const url = normalize(row?.baseUrl);
          return name && url ? [{ name, baseUrl: url }] : [];
        }) : [];
      } catch { return []; }
    }

    function persist(rows) {
      try {
        localStorage.setItem(storageKey, JSON.stringify(rows));
        window.dispatchEvent(new CustomEvent(eventName));
        return true;
      } catch { return false; }
    }

    function write(url, name) {
      const rows = read().filter((item) => item.name !== name && item.baseUrl !== url);
      rows.push({ name, baseUrl: url });
      rows.sort((a, b) => a.name.localeCompare(b.name));
      return persist(rows);
    }

    function builtin(url) {
      const stem = url.replace(/\/v1$/, "");
      return [...builtins.keys()].some((value) => value.replace(/\/v1$/, "") === stem);
    }

    function sync() {
      const saved = read();
      const entries = new Map(builtins);
      for (const { name, baseUrl } of saved) entries.set(baseUrl, name);
      known.clear();
      const placeholder = element("option", "Custom URL");
      placeholder.value = "";
      choices.replaceChildren(placeholder);
      for (const [url, name] of entries) {
        known.add(url);
        const option = element("option", `${name} · ${url}`);
        option.value = url;
        choices.append(option);
      }
      const active = normalize(input.value);
      choices.value = known.has(active) ? active : "";
      save.disabled = !active || builtin(active);
      remove.disabled = !saved.some((item) => item.baseUrl === active);
    }

    function network(status) {
      builtins.clear();
      for (const [value, name] of [
        [local, "This gateway"],
        [status?.tunnel?.publicUrl, "Cloudflare Tunnel"],
        [status?.tunnel?.tunnelUrl, "Cloudflare Tunnel"],
        [status?.tailscale?.tunnelUrl, "Tailscale"],
      ]) {
        const url = normalize(value);
        if (url && !builtins.has(url)) builtins.set(url, name);
      }
      sync();
    }

    input.addEventListener("input", sync);
    choices.addEventListener("change", () => {
      if (!choices.value) {
        input.focus();
        return;
      }
      input.value = choices.value;
      input.dispatchEvent(new Event("input", { bubbles: true }));
    });
    window.addEventListener(eventName, sync);
    window.addEventListener("storage", (event) => {
      if (event.key === storageKey || event.key === null) sync();
    });
    network();
    return {
      known, network, sync,
      remember(value = input.value) {
        const url = normalize(value);
        if (url && !known.has(url) && !builtin(url)) write(url, new URL(url).host);
      },
    };
  }

  function sessionActions() {
    const button = select("sign-out");
    button.addEventListener("click", async () => {
      button.disabled = true;
      try {
        await request("/api/auth/logout", "POST");
        window.location.replace("/login");
      } catch (error) {
        notice(select("session-status"), error.message, true);
        button.disabled = false;
      }
    });
  }

  function endpoint() {
    const status = select("workspace-status");
    const list = select("key-list");
    const url = select("server-endpoint");
    const required = select("require-key");
    const createDialog = select("create-key-dialog");
    const createForm = select("create-key-form");
    const createError = select("create-key-error");
    const createdDialog = select("created-key-dialog");
    const createdValue = select("created-key-value");
    const createdStatus = select("created-key-status");
    const accessDialog = select("key-access-dialog");
    const accessForm = select("key-access-form");
    const restricted = select("access-restricted");
    const allow = select("access-allow");
    const search = select("access-search");
    const picks = select("access-model-picks");
    const pickerStatus = select("access-picker-status");
    const accessError = select("key-access-error");
    let accessId;
    let wasRestricted;
    let catalogChoices = [];
    let generation = 0;

    function failure(output, error) {
      if (error.status === 401) {
        window.location.replace("/login");
        return;
      }
      notice(output, error.message, true);
    }

    async function copy(value, output) {
      try {
        await navigator.clipboard.writeText(value);
        notice(output, "Copied.");
      } catch {
        notice(output, "Clipboard unavailable. Select the value to copy it.", true);
      }
    }

    url.textContent = new URL("/v1", window.location.origin).href;
    select("copy-endpoint").addEventListener("click", () => copy(url.textContent, status));
    select("copy-created-key").addEventListener("click", () => copy(createdValue.textContent, createdStatus));
    required.addEventListener("change", async () => {
      required.disabled = true;
      const checked = required.checked;
      try {
        await request("/api/settings", "PATCH", { requireApiKey: checked });
        notice(status, checked ? "API keys are required." : "API keys are optional.");
      } catch (error) {
        required.checked = !checked;
        failure(status, error);
      } finally {
        required.disabled = false;
      }
    });

    function row(key) {
      const active = key.isActive !== false;
      const value = key.key ?? "";
      const mask = value.length > 12 ? `${value.slice(0, 8)}…${value.slice(-4)}` : "••••••••";
      const code = element("code", mask);
      const access = key.access?.restricted ? "Restricted" : "All models";
      const meta = element("div", null, "key-meta");
      meta.append(
        element("h3", key.name || "Untitled API key"),
        code,
        element("span", `${active ? "Active" : "Disabled"} · ${access}`, "key-state"),
      );

      const actions = element("div", null, "key-actions");
      const reveal = action("Show", () => {
        const open = reveal.getAttribute("aria-pressed") !== "true";
        code.textContent = open ? value : mask;
        reveal.textContent = open ? "Hide" : "Show";
        reveal.setAttribute("aria-pressed", String(open));
      });
      reveal.setAttribute("aria-pressed", "false");
      const copyButton = action("Copy", async () => {
        await copy(value, status);
        if (status.dataset.error === "true") {
          code.textContent = value;
          reveal.textContent = "Hide";
          reveal.setAttribute("aria-pressed", "true");
        }
      });

      async function update(event, method, body, success) {
        const button = event.currentTarget;
        button.disabled = true;
        try {
          await request(keyPath(key.id), method, body);
          await reload();
          notice(status, success);
        } catch (error) {
          failure(status, error);
        } finally {
          button.disabled = false;
        }
      }

      actions.append(
        reveal,
        copyButton,
        action("Edit access", () => openAccess(key)),
        action(active ? "Disable" : "Enable", (event) => {
          if (!active || window.confirm(`Pause API key "${key.name || "Untitled API key"}"? Requests using it will be rejected until it is resumed.`)) {
            return update(event, "PUT", { isActive: !active }, active ? "API key disabled." : "API key enabled.");
          }
        }),
        action("Delete", (event) => {
          if (window.confirm("Delete this API key?")) {
            return update(event, "DELETE", undefined, "API key deleted.");
          }
        }, "button-danger"),
      );
      const entry = element("article", null, "key-row");
      entry.append(meta, actions);
      return entry;
    }

    async function reload(provisionDefault = false) {
      list.setAttribute("aria-busy", "true");
      try {
        let { keys } = await request("/api/keys");
        if (provisionDefault && !keys.length) {
          try {
            await request("/api/keys", "POST", { name: "Default Key" });
            ({ keys } = await request("/api/keys"));
          } catch (error) {
            failure(status, error);
          }
        }
        list.replaceChildren(...(keys.length ? keys.map(row) : [element("p", "No API keys yet.", "muted")]));
      } finally {
        list.setAttribute("aria-busy", "false");
      }
    }

    const keyPath = (id) => `/api/keys/${encodeURIComponent(id)}`;
    select("new-key").addEventListener("click", () => {
      createForm.reset();
      notice(createError);
      createDialog.showModal();
      select("key-name").focus();
    });
    createForm.addEventListener("submit", async (event) => {
      event.preventDefault();
      const submit = createForm.querySelector("button[type=submit]");
      if (submit.disabled) return;
      const name = select("key-name").value.trim();
      if (!name) {
        notice(createError, "Enter a name for the API key.", true);
        return;
      }
      submit.disabled = true;
      notice(createError);
      try {
        const created = await request("/api/keys", "POST", { name });
        createDialog.close();
        createdValue.textContent = created.key;
        notice(createdStatus);
        if (!createdDialog.open) createdDialog.showModal();
        await reload().catch((error) => failure(status, error));
      } catch (error) {
        failure(createError, error);
      } finally {
        submit.disabled = false;
      }
    });

    for (const button of document.querySelectorAll("[data-close]")) {
      button.addEventListener("click", () => select(button.dataset.close).close());
    }
    createdDialog.addEventListener("close", () => { createdValue.textContent = ""; });

    const editable = select("access-list-fields");
    const lines = () => allow.value.split(/\r?\n/).map((id) => id.trim()).filter(Boolean);
    restricted.addEventListener("change", () => { editable.disabled = !restricted.checked; });

    function syncChoices() {
      const selected = new Set(lines().map(id => id.toLowerCase()));
      for (const option of picks.querySelectorAll("input[type=checkbox]")) {
        option.checked = selected.has(option.value.toLowerCase());
      }
    }

    function renderChoices() {
      const query = search.value.trim().toLowerCase();
      const matches = catalogChoices.filter(({ id, group }) =>
        !query || id.toLowerCase().includes(query) || group.toLowerCase().includes(query));
      const groups = new Map();
      for (const { id, group } of matches.slice(0, 60)) {
        if (!groups.has(group)) {
          const section = element("fieldset");
          section.append(element("legend", group));
          groups.set(group, section);
        }
        const label = element("label", null, "check-row");
        const option = element("input");
        option.type = "checkbox";
        option.value = id;
        option.addEventListener("change", () => {
          const next = lines().filter(entry => entry.toLowerCase() !== id.toLowerCase());
          if (option.checked) next.push(id);
          allow.value = next.join("\n");
        });
        label.append(option, element("code", id));
        groups.get(group).append(label);
      }
      picks.replaceChildren(...groups.values());
      syncChoices();
      pickerStatus.textContent = matches.length > 60 ? "Showing the first 60 matches. Refine your search for more."
        : matches.length ? `${matches.length} choices` : "No matching combos or models.";
    }

    async function loadOptions() {
      const current = ++generation;
      pickerStatus.textContent = "Loading combos and models…";
      const results = await Promise.allSettled([
        request("/api/combos"),
        request("/api/models/alias"),
        request("/api/models/custom"),
        request("/api/models/access-options"),
        request("/api/models/disabled"),
      ]);
      if (current !== generation) return;
      const [comboResult, aliasResult, customResult, storedResult, disabledResult] = results.map(result => result.value ?? {});
      const disabled = disabledResult.disabled ?? {};
      const available = [
        ...(comboResult.combos ?? []).map(combo => ({ id: combo.name, group: "Combos" })),
        ...Object.keys(aliasResult.aliases ?? {}).map(id => ({ id, group: "Model aliases" })),
        ...(customResult.models ?? []).filter(model => model?.providerAlias && model.id &&
          !disabled[model.providerAlias]?.includes(model.id))
          .map(model => ({ id: `${model.providerAlias}/${model.id}`, group: `Models · ${model.providerAlias}` })),
        ...(storedResult.models ?? []).map(model => ({ id: model.id, group: `Models · ${model.provider}` })),
      ];
      const seen = new Set();
      catalogChoices = available.filter(({ id }) => {
        if (typeof id !== "string" || !id || seen.has(id.toLowerCase())) return false;
        seen.add(id.toLowerCase());
        return true;
      });
      renderChoices();
      if (results.every(result => result.status === "rejected")) pickerStatus.textContent = "Could not load model choices. You can enter IDs above.";
    }

    function openAccess(key) {
      accessId = key.id;
      wasRestricted = restricted.checked = key.access?.restricted === true;
      editable.disabled = !restricted.checked;
      allow.value = (key.access?.allow ?? []).join("\n");
      search.value = "";
      catalogChoices = [];
      picks.replaceChildren();
      notice(accessError);
      accessDialog.showModal();
      void loadOptions();
    }
    search.addEventListener("input", renderChoices);
    allow.addEventListener("input", syncChoices);
    accessDialog.addEventListener("close", () => { generation++; });

    accessForm.addEventListener("submit", async (event) => {
      event.preventDefault();
      const submit = accessForm.querySelector("button[type=submit]");
      if (submit.disabled || !accessId) return;
      if (restricted.checked && !wasRestricted &&
          !window.confirm("Restrict this API key? It will be allowed to call only the selected combos and models.")) return;
      submit.disabled = true;
      notice(accessError);
      try {
        await request(keyPath(accessId), "PUT", { access: {
          restricted: restricted.checked,
          allow: restricted.checked ? lines() : [],
        } });
        accessDialog.close();
        await reload().catch((error) => failure(status, error));
      } catch (error) {
        failure(accessError, error);
      } finally {
        submit.disabled = false;
      }
    });

    reload(true).catch((error) => {
      list.replaceChildren();
      failure(status, error);
    });
  }

  window.nineRouterUi = Object.freeze({ select, request, notice, element, action, theme, setTheme, cliEndpoints });
  if (document.body.dataset.page === "login") login();
  else sessionActions();
  if (document.body.dataset.page === "endpoint") endpoint();
})();
