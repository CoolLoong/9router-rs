(() => {
  "use strict";

  const { select, request, element, notice } = window.nineRouterUi;
  const statusEndpoint = "/api/cli-tools/antigravity-mitm";
  const aliasEndpoint = `${statusEndpoint}/alias`;
  const output = select("mitm-notice");
  const client = select("mitm-tool");
  const mappings = select("mitm-mappings");
  const saveMappings = select("mitm-mappings-save");
  const routerUrl = select("mitm-router-url");
  const saveUrl = select("mitm-router-save");
  const refreshButton = select("mitm-refresh");
  let dnsStatus = {};
  let aliasRequest = 0;

  async function run(button, operation) {
    if (button?.disabled) return;
    if (button) button.disabled = true;
    notice(output);
    try {
      await operation();
    } catch (error) {
      if (error.status === 401) window.location.replace("/login");
      else notice(output, error.message, true);
    } finally {
      if (button) button.disabled = false;
      saveMappings.disabled = !dnsStatus[client.value] || !mappings.children.length;
    }
  }

  async function loadMappings() {
    const selected = client.selectedOptions[0];
    const tool = selected.value;
    const version = ++aliasRequest;
    saveMappings.disabled = true;
    mappings.replaceChildren();
    const enabled = dnsStatus[tool] === true;
    select("mitm-dns").textContent = enabled ? "Local DNS mapping is active." : "Enable local DNS mapping for this client before editing.";
    const { aliases } = await request(`${aliasEndpoint}?tool=${encodeURIComponent(tool)}`);
    if (aliasRequest !== version) return;
    const rows = new Map(JSON.parse(selected.dataset.models).map(model => [model.alias, model.name]));
    for (const name of Object.keys(aliases ?? {})) {
      if (!rows.has(name)) rows.set(name, name);
    }
    for (const [alias, name] of rows) {
      const label = element("label", name, "key-row");
      const input = element("input");
      input.type = "text";
      input.value = Object.hasOwn(aliases ?? {}, alias) ? aliases[alias] : "";
      input.spellcheck = false;
      input.placeholder = "provider/model";
      input.disabled = !enabled;
      input.dataset.alias = alias;
      label.append(input);
      mappings.append(label);
    }
    saveMappings.disabled = !enabled;
  }

  async function loadStatus() {
    const status = await request(statusEndpoint);
    dnsStatus = status.dnsStatus ?? {};
    select("mitm-service").textContent = status.running ? `Running (PID ${status.pid})` : "Stopped";
    select("mitm-certificate").textContent = status.certTrusted ? "Certificate trusted" : status.certExists ? "Certificate present" : "No local certificate";
    routerUrl.value = status.mitmRouterBaseUrl ?? "";
    routerUrl.disabled = false;
    saveUrl.disabled = false;
    client.disabled = false;
    await loadMappings();
  }

  refreshButton.addEventListener("click", () => run(refreshButton, loadStatus));
  client.addEventListener("change", () => run(null, loadMappings));
  select("mitm-router-form").addEventListener("submit", (event) => {
    event.preventDefault();
    run(saveUrl, async () => {
      const value = routerUrl.value.trim().replace(/\/+$/, "") || "http://localhost:20128";
      if (!["http:", "https:"].includes(new URL(value).protocol)) {
        throw new Error("MITM router URL must use http or https");
      }
      await request("/api/settings", "PATCH", { mitmRouterBaseUrl: value });
      routerUrl.value = value;
      notice(output, "Router URL saved.");
    });
  });
  select("mitm-mappings-form").addEventListener("submit", (event) => {
    event.preventDefault();
    run(saveMappings, async () => {
      const values = Object.fromEntries([...mappings.querySelectorAll("input")].map(input => [input.dataset.alias, input.value]));
      await request(aliasEndpoint, "PUT", { tool: client.value, mappings: values });
      notice(output, "Model mappings saved.");
    });
  });
  run(refreshButton, loadStatus);
})();
