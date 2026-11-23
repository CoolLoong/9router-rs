(() => {
  "use strict";

  const { select, request, notice } = window.nineRouterUi;
  const nodes = "/api/provider-nodes";
  const providers = "/dashboard/providers";
  const form = select("node-form");
  const fields = select("node-fields");
  const checkFields = select("node-check-fields");
  const status = select("node-status");
  const checkStatus = select("node-check-status");
  const type = select("node-type");
  const api = select("node-api");
  const url = select("node-url");
  const key = select("node-check-key");
  const model = select("node-check-model");
  const id = form.dataset.nodeId;
  const endpoint = id ? `${nodes}/${encodeURIComponent(id)}` : nodes;
  const defaults = {
    "openai-compatible": "https://api.openai.com/v1",
    "anthropic-compatible": "https://api.anthropic.com/v1",
    "custom-embedding": "https://api.openai.com/v1",
  };
  let busy = false;

  function updateProtocol() {
    const openai = type.value === "openai-compatible";
    const embedding = type.value === "custom-embedding";
    api.disabled = !openai;
    select("node-api-fields").hidden = !openai;
    url.placeholder = defaults[type.value];
    model.required = embedding;
    select("node-check-model-label").textContent = embedding ? "Embedding model ID" : "Model ID for inference fallback (optional)";
  }

  function validBase(raw) {
    try {
      const parsed = new URL(raw);
      return ["http:", "https:"].includes(parsed.protocol) && !parsed.username && !parsed.password &&
        !raw.includes("?") && !raw.includes("#");
    } catch {
      return false;
    }
  }

  function transport() {
    const baseUrl = url.value.trim();
    if (!validBase(baseUrl)) throw new Error("Enter an HTTP or HTTPS base URL without credentials, a query or a fragment.");
    return {
      type: type.value,
      baseUrl,
      ...(type.value === "openai-compatible" ? { apiType: api.value } : {}),
    };
  }

  async function task(output, work) {
    if (busy || fields.disabled) return;
    busy = fields.disabled = checkFields.disabled = true;
    notice(output);
    try {
      await work();
    } catch (error) {
      if (error.status === 401 && error.message === "Unauthorized") window.location.replace("/login");
      else notice(output, error.message, true);
    } finally {
      busy = fields.disabled = checkFields.disabled = false;
    }
  }

  async function load() {
    if (id) {
      const result = await request(nodes);
      const node = result.nodes.find(node => node.id === id);
      if (!node || !Object.hasOwn(defaults, node.type)) throw new Error("Compatible endpoint not found.");
      type.value = node.type;
      type.disabled = true;
      api.value = node.apiType === "responses" ? "responses" : "chat";
      select("node-name").value = node.name || "";
      select("node-prefix").value = node.prefix || "";
      const existing = node.baseUrl || defaults[node.type];
      if (validBase(existing)) url.value = existing;
      const connect = select("node-add-key");
      connect.href = `${providers}/new?provider=${encodeURIComponent(id)}`;
    } else {
      const requested = new URLSearchParams(window.location.search).get("type");
      type.value = Object.hasOwn(defaults, requested) ? requested : "openai-compatible";
      url.value = defaults[type.value];
    }
    updateProtocol();
    fields.disabled = checkFields.disabled = false;
  }

  type.addEventListener("change", () => {
    if (!url.value.trim() || Object.values(defaults).includes(url.value)) url.value = defaults[type.value];
    updateProtocol();
  });
  form.addEventListener("submit", event => {
    event.preventDefault();
    void task(status, async () => {
      const name = select("node-name").value.trim();
      const prefix = select("node-prefix").value.trim();
      if (!name || !prefix) throw new Error("Enter a name and a model prefix.");
      const { node } = await request(endpoint, id ? "PUT" : "POST", { ...transport(), name, prefix });
      key.value = "";
      const directory = node.type === "custom-embedding" ? "/dashboard/media-providers/embedding" : providers;
      window.location.assign(`${directory}/${encodeURIComponent(node.id)}`);
    });
  });
  select("node-check-form").addEventListener("submit", event => {
    event.preventDefault();
    void task(checkStatus, async () => {
      const apiKey = key.value.trim();
      if (!apiKey) throw new Error("Enter a test API key.");
      const modelId = model.value.trim();
      if (type.value === "custom-embedding" && !modelId) throw new Error("Enter an embedding model ID.");
      const result = await request(`${nodes}/validate`, "POST", { ...transport(), apiKey, ...(modelId ? { modelId } : {}) });
      if (!result.valid) throw new Error(result.error || "Endpoint check failed.");
      if (result.method === "embeddings") {
        notice(checkStatus, Number.isInteger(result.dimensions)
          ? `Embedding check passed (${result.dimensions} dimensions).` : "Embedding check passed.");
      } else {
        notice(checkStatus, result.method === "responses" ? "Responses API check passed."
          : result.method === "chat" ? "Chat Completions check passed." : "Model list check passed.");
      }
    });
  });
  select("node-delete").addEventListener("click", () => {
    if (!id || !window.confirm("Delete this endpoint and its saved connections and custom models?")) return;
    void task(status, async () => {
      await request(endpoint, "DELETE");
      key.value = "";
      window.location.assign(type.value === "custom-embedding" ? "/dashboard/media-providers/embedding" : `${providers}#provider-nodes`);
    });
  });

  void load().catch(error => notice(status, error.message, true));
})();
