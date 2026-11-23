(() => {
  "use strict";

  const { select, request, notice, element, cliEndpoints } = window.nineRouterUi;
  const endpoint = "/api/cli-tools/hermes-settings";
  const roles = [
    ["delegation", "Delegation (subagents)"], ["vision", "Vision"],
    ["compression", "Compression"], ["title_generation", "Title generation"],
    ["approval", "Approval"], ["skills_hub", "Skills Hub"], ["mcp", "MCP"],
    ["review", "Review (/review subagent)"], ["memory_query_rewrite", "Memory query rewrite"],
    ["background_review", "Background review"], ["curator", "Curator"], ["monitor", "Monitor"],
    ["tts_audio_tags", "TTS audio tags"], ["triage_specifier", "Triage specifier"],
    ["kanban_decomposer", "Kanban decomposer"], ["profile_describer", "Profile describer"],
    ["goal_judge", "Goal judge"],
  ];
  const output = select("hermes-notice");
  const profile = select("hermes-profile");
  const base = select("hermes-endpoint");
  const key = select("hermes-key");
  const model = select("hermes-model");
  const audio = select("hermes-audio");
  const transcription = select("hermes-stt");
  const report = select("hermes-bulk-results");
  const refresh = select("hermes-refresh");
  const bulk = select("hermes-bulk");
  const reset = select("hermes-reset");
  const roleFields = new Map();
  const keys = new Map();
  let profiles = [];
  let current = {};
  let busy = false;

  function routerUrl(value) {
    try {
      const url = new URL(value);
      if (!["http:", "https:"].includes(url.protocol) || url.username || url.password || url.search || url.hash) return "";
      const path = url.pathname.replace(/\/+$/, "");
      return `${url.origin}${path.endsWith("/v1") ? path : `${path}/v1`}`;
    } catch { return ""; }
  }

  const local = new URL(window.location.origin);
  if (local.hostname === "localhost") local.hostname = "127.0.0.1";
  const localUrl = routerUrl(local);
  base.value = localUrl;
  const presets = cliEndpoints(base, output, { local: localUrl, normalize: routerUrl });
  const knownEndpoints = presets.known;
  for (const [id, label] of roles) {
    const field = element("div");
    const caption = element("label", label);
    const input = element("input");
    input.id = `hermes-role-${id}`;
    input.setAttribute("list", "hermes-chat-models");
    input.setAttribute("autocomplete", "off");
    caption.htmlFor = input.id;
    roleFields.set(id, input);
    field.append(caption, input);
    select("hermes-roles").append(field);
  }

  function failed(error) {
    if (error.status === 401) window.location.replace("/login");
    else notice(output, error.message, true);
  }

  function controls() {
    profile.disabled = busy || profiles.length === 0;
    refresh.disabled = busy;
    select("hermes-fields").disabled = busy || !current.installed;
    bulk.disabled = profiles.length < 2;
    const settings = current.settings ?? {};
    const marked = [settings.model, settings.delegation, ...Object.values(settings.auxiliary ?? {})]
      .some(config => config?.provider === "custom" && knownEndpoints.has(routerUrl(config.base_url)));
    reset.disabled = !marked && !current.has9Router && !current.has9RouterStt;
  }

  async function operate(operation) {
    if (busy) return;
    busy = true;
    controls();
    notice(output);
    report.hidden = true;
    try { await operation(); }
    catch (error) { failed(error); }
    finally { busy = false; controls(); }
  }

  function option(value, label = "") {
    const node = element("option", label);
    node.value = value;
    return node;
  }

  function selections() {
    return [["default", model], ...roleFields]
      .filter(([, input]) => input.value.trim())
      .map(([role, input]) => ({ role, model: input.value.trim() }));
  }

  function updatePreview() {
    const directory = profile.value === "default" ? "~/.hermes" : `~/.hermes/profiles/${profile.value}`;
    select("hermes-yaml-path").textContent = `${directory}/config.yaml`;
    select("hermes-env-path").textContent = `${directory}/.env`;
    select("hermes-audio-fields").hidden = !audio.checked;
    transcription.disabled = !audio.checked;
    transcription.required = audio.checked;
    const url = JSON.stringify(routerUrl(base.value) || localUrl);
    const selected = new Map(selections().map(({ role, model }) => [role, JSON.stringify(model)]));
    function provider(name, id, indent = "  ") {
      return [
        `${indent}provider: custom`, `${indent}${name}: ${id}`,
        `${indent}base_url: ${url}`, indent + "api_key: ${OPENAI_API_KEY}",
      ].join("\n");
    }
    const text = ["model:", provider("default", selected.get("default") || '"provider/model"')];
    if (selected.has("delegation")) text.push("delegation:", provider("model", selected.get("delegation")));
    const auxiliary = [...selected].filter(([role]) => role !== "default" && role !== "delegation");
    if (auxiliary.length) {
      text.push("auxiliary:");
      for (const [role, id] of auxiliary) text.push(`  ${role}:`, provider("model", id, "    "));
    }
    if (audio.checked && transcription.value.trim()) {
      text.push("stt:", "  enabled: true", "  provider: openai", "  openai:",
        `    model: ${JSON.stringify(transcription.value.trim())}`,
        `    base_url: ${url}`, "    api_key: ${OPENAI_API_KEY}");
    }
    select("hermes-yaml").textContent = text.join("\n") + "\n";
  }

  async function load() {
    const active = profile.value;
    current = {};
    profiles = (await request("/api/cli-tools/hermes-profiles")).profiles ?? [];
    profile.replaceChildren(...profiles.map(item => {
      const label = item.displayName || (item.isDefault ? "Default" : item.name);
      const state = item.has9Router || knownEndpoints.has(routerUrl(item.baseUrl)) ?
        "9Router" : item.baseUrl ? "Other endpoint" : "Unconfigured";
      return option(item.name, `${label} · ${state}`);
    }));
    profile.value = profiles.some(item => item.name === active) ? active : "default";
    try {
      current = await request(`${endpoint}?${new URLSearchParams({ profile: profile.value })}`);
    } catch (error) {
      if (error.status !== 404 || profile.value === "default") throw error;
      profile.value = "default";
      return load();
    }
    const settings = current.settings ?? {};
    const selectedProfile = profiles.find(item => item.name === profile.value);
    select("hermes-command").textContent = selectedProfile?.command || "hermes";
    select("hermes-status").textContent = !current.installed ? "Hermes Agent is not installed." :
      [current.has9Router || knownEndpoints.has(routerUrl(settings.model?.base_url)) ?
        "9Router configured" : settings.model?.base_url ? "Other endpoint" : "No router endpoint",
        current.has9RouterStt ? "9Router audio transcription" : "", current.configPath].filter(Boolean).join(" · ");
    base.value = settings.model?.base_url || settings.stt?.base_url || localUrl;
    presets.sync();
    model.value = settings.model?.default || "";
    for (const [role, input] of roleFields) {
      const config = role === "delegation" ? settings.delegation : settings.auxiliary?.[role];
      input.value = config?.model || "";
    }
    audio.checked = current.has9RouterStt === true;
    transcription.value = audio.checked ? settings.stt?.model || "" : "";
    updatePreview();
  }

  async function choices() {
    const [keyResult, network, aliases, chat, stt] = (await Promise.allSettled([
      request("/api/keys"), request("/api/tunnel/status"), request("/api/models/alias"),
      request("/v1/models"), request("/v1/models/stt"),
    ])).map(result => result.value ?? {});
    for (const item of keyResult.keys ?? []) {
      if (!item.id || !item.key || item.isActive === false) continue;
      key.append(option(item.id, item.name || "Router key"));
      keys.set(item.id, item.key);
    }
    presets.network(network);
    const chatIds = [...Object.keys(aliases.aliases ?? {}), ...(chat.data ?? []).map(item => item.id)];
    const sttIds = (stt.data ?? []).map(item => item.id);
    for (const [id, values] of [["hermes-chat-models", chatIds], ["hermes-stt-models", sttIds]]) {
      select(id).replaceChildren(...[...new Set(values.filter(Boolean))].sort().map(value => option(value)));
    }
    controls();
  }

  const form = select("hermes-form");
  form.addEventListener("input", updatePreview);
  form.addEventListener("submit", event => {
    event.preventDefault();
    operate(async () => {
      const baseUrl = routerUrl(base.value);
      const selected = selections();
      if (!baseUrl) throw new Error("Enter a valid HTTP or HTTPS router URL.");
      if (!model.value.trim() && !audio.checked && !current.has9RouterStt) throw new Error("Choose a default or transcription model.");
      const result = await request(endpoint, "POST", {
        profile: profile.value, baseUrl, apiKey: keys.get(key.value), selections: selected,
        audio: { enabled: audio.checked, model: transcription.value.trim() },
      });
      presets.remember(baseUrl);
      await load();
      notice(output, result.message);
    });
  });
  bulk.addEventListener("click", () => operate(async () => {
    const baseUrl = routerUrl(base.value);
    if (!knownEndpoints.has(baseUrl)) throw new Error("Choose a known 9Router endpoint before updating all profiles.");
    if (!model.value.trim()) throw new Error("Choose a default model for profiles without a model.");
    if (!window.confirm(`Apply ${baseUrl} to the Hermes profiles that use 9Router?`)) return;
    const result = await request(endpoint, "POST", {
      applyToAll: true, baseUrl, apiKey: keys.get(key.value), model: model.value.trim(),
    });
    await load();
    report.replaceChildren(...(result.results ?? []).map(item =>
      element("li", `${item.profile} · ${item.reason || item.status}`)));
    report.hidden = !report.childElementCount;
    notice(output, result.message, !result.success);
  }));
  reset.addEventListener("click", () => {
    if (!window.confirm(`Remove 9Router settings from the ${profile.value} Hermes profile?`)) return;
    operate(async () => {
      const result = await request(endpoint, "DELETE", { profile: profile.value });
      await load();
      notice(output, result.message);
    });
  });
  profile.addEventListener("change", () => operate(load));
  refresh.addEventListener("click", () => operate(load));
  function copy(id, text) {
    select(id).addEventListener("click", async () => {
      try {
        await navigator.clipboard.writeText(text());
        notice(output, "Copied.");
      } catch (error) { failed(error); }
    });
  }
  copy("hermes-copy-command", () => select("hermes-command").textContent);
  copy("hermes-copy-yaml", () => select("hermes-yaml").textContent);
  copy("hermes-copy-env", () => `OPENAI_API_KEY=${keys.get(key.value) ?? "<9Router API key>"}\n`);
  choices().catch(failed);
  updatePreview();
  operate(load);
})();
