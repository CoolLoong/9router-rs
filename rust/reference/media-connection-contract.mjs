import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { runInNewContext } from "node:vm";

const integration = readFileSync(new URL("../app/src/dashboard_assets/integrations.js", import.meta.url), "utf8");
const flush = () => new Promise((done) => setImmediate(done));

class Element {
  constructor(text = "") {
    this.textContent = text;
    this.children = [];
    this.handlers = new Map();
    this.dataset = {};
    this.files = [];
    this.disabled = false;
    this.value = "";
  }
  set value(value) {
    this.currentValue = value;
    if (value === "") this.files = [];
  }
  get value() { return this.currentValue; }
  append(...children) { this.children.push(...children); }
  replaceChildren(...children) { this.children = children; }
  querySelector() { return this.submit; }
  addEventListener(type, handler) { this.handlers.set(type, handler); }
  removeAttribute(name) { delete this[name]; }
  focus() { this.focused = true; }
  async fire(type) {
    this.handlers.get(type)?.({ currentTarget: this, preventDefault() {} });
    await flush();
  }
  find(text) {
    if (this.textContent === text) return this;
    return this.children.map((child) => child.find(text)).find(Boolean);
  }
}

function fixture(provider, scheme, kind, oauth = false) {
  const byId = new Map([
    "integration-status", "media-page", "media-create-connection", "media-models", "media-connections",
  ].map((id) => [id, new Element()]));
  if (scheme) {
    for (const suffix of ["form", "name", "key", "url", "account", "file", "location", "cancel", "settings"])
      byId.set("media-credential-" + suffix, new Element());
    byId.get("media-credential-form").dataset.scheme = scheme;
    byId.get("media-credential-form").submit = new Element();
  }
  if (oauth) byId.set("provider-account", new Element());
  Object.assign(byId.get("media-page").dataset, { provider, mediaKind: kind, alias: provider, endpoint: "" });
  const records = [];
  const state = { connections: [], fail: false };
  const events = new Map();
  runInNewContext(integration, {
    URL, URLSearchParams,
    document: { body: { dataset: { page: "media-detail" } }, addEventListener: (type, handler) => events.set(type, handler) },
    window: { nineRouterUi: {
      select: (id) => byId.get(id),
      notice: (node, text, error) => { node.textContent = text || ""; node.error = !!error; },
      element: (_tag, text) => new Element(text),
      action: (text, handler) => { const button = new Element(text); button.addEventListener("click", handler); return button; },
      request: async (path, method = "GET", payload) => {
        payload = payload && JSON.parse(JSON.stringify(payload));
        records.push({ path, method, payload });
        if (method === "GET") return { connections: state.connections };
        if (state.fail) throw new Error("Upstream echoed synthetic-media-key");
        const connection = { provider, id: "saved-media-connection", authType: "apikey", ...payload };
        state.connections = [connection];
        return { connection };
      },
    } },
  });
  return { records, state, events, field: (name) => byId.get("media-credential-" + name),
    rows: byId.get("media-connections"), status: byId.get("integration-status"),
    submit: () => byId.get("media-credential-form").fire("submit") };
}

const cloudflare = fixture("cloudflare-ai", "cloudflare", "systemone");
cloudflare.field("name").value = "Cloudflare fixture";
cloudflare.field("key").value = " synthetic-media-key ";
cloudflare.field("account").value = "account_fixture";
await cloudflare.submit();
assert.deepEqual(cloudflare.records[1], { path: "/api/providers", method: "POST", payload: {
  provider: "cloudflare-ai", name: "Cloudflare fixture", apiKey: "synthetic-media-key", providerSpecificData: { accountId: "account_fixture" },
} });
assert.equal(cloudflare.field("key").value, "");
await cloudflare.rows.find("Edit").fire("click");
assert.equal(cloudflare.field("account").value, "account_fixture");
assert.equal(cloudflare.field("key").required, false);
assert.equal(cloudflare.field("key").value, "");
assert.equal(cloudflare.field("settings").href, "/dashboard/providers/connections/saved-media-connection");
cloudflare.field("account").value = "next-account";
await cloudflare.submit();
assert.deepEqual(cloudflare.records[3], { path: "/api/providers/saved-media-connection", method: "PUT", payload: {
  name: "Cloudflare fixture", providerSpecificData: { accountId: "next-account" },
} });

const speech = fixture("selfhosted-stt", "selfhosted", "stt");
speech.field("name").value = "Speech fixture";
speech.field("key").value = "synthetic-media-key";
speech.field("url").value = "https://username:password@fixture.invalid/stt";
await speech.submit();
assert.equal(speech.records.length, 1);
assert.match(speech.status.textContent, /HTTP or HTTPS/);
speech.field("key").value = "synthetic-media-key";
speech.field("url").value = "http://localhost:9000/v1/audio/transcriptions";
speech.state.fail = true;
await speech.submit();
assert.deepEqual(speech.records[1].payload.providerSpecificData, { baseUrl: "http://localhost:9000/v1/audio/transcriptions" });
assert.equal(speech.records[1].payload.apiKey, "synthetic-media-key");
assert.equal(speech.status.textContent, "Could not save the connection.");
assert.equal(speech.field("key").value, "");

for (const [kind, baseUrl] of [["tts", "http://localhost:8880"], ["embedding", "http://localhost:9000/v1"]]) {
  const endpoint = fixture("selfhosted-" + kind, "selfhosted", kind);
  endpoint.field("name").value = "Endpoint fixture";
  endpoint.field("key").value = "synthetic-media-key";
  endpoint.field("url").value = baseUrl;
  await endpoint.submit();
  assert.deepEqual(endpoint.records[1].payload.providerSpecificData, { baseUrl });
}

const vertex = fixture("vertex", "vertex", "video");
vertex.field("name").value = "Vertex fixture";
vertex.field("file").files = [{ text: async () => '{"type":"authorized_user"}' }];
await vertex.submit();
assert.equal(vertex.records.length, 1);
assert.match(vertex.status.textContent, /service account file/);
assert.equal(vertex.field("file").files.length, 0);
const service = { type: "service_account", project_id: "synthetic-vertex-project", client_email: "fixture@example.invalid", private_key: "synthetic-media-key" };
vertex.field("file").files = [{ text: async () => JSON.stringify(service) }];
vertex.field("location").value = "us-central1";
await vertex.submit();
assert.deepEqual(vertex.records[1].payload, { provider: "vertex", name: "Vertex fixture", apiKey: JSON.stringify(service),
  providerSpecificData: { location: "us-central1" } });
assert.equal(vertex.field("file").files.length, 0);
await vertex.rows.find("Edit").fire("click");
assert.equal(vertex.field("location").value, "us-central1");
vertex.field("location").value = "";
await vertex.submit();
assert.deepEqual(vertex.records[3].payload, { name: "Vertex fixture", providerSpecificData: { location: null } });

const codex = fixture("codex", null, "stt", true);
await flush();
codex.state.connections = [{ provider: "codex", id: "oauth-fixture", name: "OAuth saved", authType: "oauth" }];
codex.events.get("provider-accounts-updated")();
await flush();
assert.equal(codex.records.length, 2);
assert.ok(codex.rows.find("OAuth saved"));
assert.equal(codex.rows.find("Edit").href, "/dashboard/providers/connections/oauth-fixture");
console.log("Media VM covered credential creation, edit payloads, malformed input, secret clearing, and OAuth account refresh.");
