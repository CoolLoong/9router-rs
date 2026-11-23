import assert from "node:assert/strict";
import vm from "node:vm";

const [origin, cookie, endpoint] = process.argv.slice(2);
const nodesUrl = "/api/provider-nodes";
const providers = "/dashboard/providers";
const probeKey = "offline-compatible-probe";
const savedKey = "offline-compatible-saved";
const calls = [];
const navigations = [];
let confirmDelete = false;

async function send(path, method = "GET", body) {
  assert(path.startsWith("/") && !path.startsWith("//"));
  calls.push({ path, method, body });
  return fetch(origin + path, { method, redirect: "manual", headers: {
    cookie, ...(body === undefined ? {} : { "Content-Type": "application/json" }),
  }, ...(body === undefined ? {} : { body: JSON.stringify(body) }) });
}

async function request(path, method, body) {
  const response = await send(path, method, body);
  const data = await response.json();
  if (!response.ok) {
    const error = new Error(data.error || `HTTP ${response.status}`);
    error.status = response.status;
    throw error;
  }
  return data;
}

class Control {
  constructor(tag, text = "") {
    this.tag = tag;
    this.textContent = text || "";
    this.value = "";
    this.children = [];
    this.options = [];
    this.dataset = {};
    this.disabled = false;
    this.listeners = new Map();
  }
  addEventListener(type, callback) { this.listeners.set(type, callback); }
  emit(type) { this.listeners.get(type)?.({ currentTarget: this, preventDefault() {} }); }
  setAttribute() {}
  append(...children) { this.children.push(...children); }
  replaceChildren(...children) { this.children = children; }
}

async function page(path, script, kind) {
  const response = await send(path);
  assert.equal(response.status, 200);
  const html = await response.text();
  const asset = `/__9router-ui/${script}`;
  assert(html.includes(asset) && !html.includes(probeKey) && !html.includes(savedKey));
  const fields = new Map(Array.from(html.matchAll(/\bid="([^"]+)"/g))
    .map(([, id]) => [id, new Control(id)]));
  if (kind === "provider-node") {
    fields.get("node-form").dataset.nodeId = html.match(/data-node-id="([^"]*)"/)[1];
    fields.get("node-fields").disabled = fields.get("node-check-fields").disabled = true;
  }
  const inspect = id => {
    assert(fields.has(id), `Missing control ${id}`);
    return fields.get(id);
  };
  const javascript = await send(asset);
  assert.equal(javascript.status, 200);
  assert(javascript.headers.get("content-type").startsWith("text/javascript"));
  const context = {
    URL, URLSearchParams,
    document: { body: { dataset: { page: kind } } },
    window: {
      confirm: () => confirmDelete,
      location: { search: new URL(origin + path).search,
        assign: path => navigations.push(path), replace: path => navigations.push(path) },
      nineRouterUi: {
        select: inspect, request,
        notice(control, text = "", failed = false) { control.textContent = text; control.failed = failed; },
        element(tag, text) { return new Control(tag, text); },
        action(text) { return new Control("button", text); },
      },
    },
  };
  vm.runInNewContext(await javascript.text(), context, { filename: script });
  return inspect;
}

async function until(condition) {
  for (let attempt = 0; attempt < 200; attempt++) {
    if (condition()) return;
    await new Promise(resolve => setTimeout(resolve, 15));
  }
  throw new Error("Compatible endpoint control did not complete");
}

let forms = await page(`${providers}/nodes/new?type=anthropic-compatible`, "provider-node.js", "provider-node");
await until(() => !forms("node-fields").disabled);
assert.equal(forms("node-type").value, "anthropic-compatible");
assert(forms("node-api-fields").hidden && forms("node-api").disabled);
forms("node-type").value = "openai-compatible";
forms("node-type").emit("change");
assert(!forms("node-api-fields").hidden);
forms("node-api").value = "responses";
forms("node-url").value = `${endpoint}/missing/v1`;
forms("node-check-key").value = probeKey;
forms("node-check-model").value = "fixture-model";
forms("node-check-form").emit("submit");
await until(() => !forms("node-check-fields").disabled);
assert.equal(forms("node-check-status").textContent, "Responses API check passed.");
assert.equal(calls.at(-1).body.apiKey, probeKey);
assert.equal(calls.at(-1).body.apiType, "responses");

forms("node-name").value = "Fixture compatible";
forms("node-prefix").value = "fixture-compatible";
forms("node-form").emit("submit");
await until(() => navigations.length === 1);
const id = decodeURIComponent(navigations[0].split("/").at(-1));
assert(!("apiKey" in calls.at(-1).body));
assert.equal(forms("node-check-key").value, "");
const detail = await send(navigations[0]);
assert.equal(detail.status, 200);
const detailHtml = await detail.text();
assert(detailHtml.includes(`${providers}/nodes/${id}`));
assert(detailHtml.includes(`${providers}/new?provider=${id}`));
assert(!detailHtml.includes("?panel=legacy"));

const list = await page(providers, "management.js", "providers");
await until(() => list("provider-node-list").children.length > 0);
const entry = list("provider-node-list").children.find(row => row.children[0].children[0].textContent === "Fixture compatible");
assert(entry);
assert.equal(entry.children[1].children[1].href, `${providers}/nodes/${id}`);

const created = await request("/api/providers", "POST", { provider: id, name: "Fixture Key", apiKey: savedKey });
assert.equal(created.connection.provider, id);
forms = await page(`${providers}/nodes/${id}`, "provider-node.js", "provider-node");
await until(() => !forms("node-fields").disabled);
assert.equal(forms("node-api").value, "responses");
assert.equal(forms("node-check-key").value, "");
assert(forms("node-type").disabled);
assert.equal(forms("node-add-key").href, `${providers}/new?provider=${id}`);
forms("node-name").value = "Updated endpoint";
forms("node-prefix").value = "updated-fixture";
forms("node-api").value = "chat";
forms("node-url").value = `${endpoint}/native/v1`;
forms("node-form").emit("submit");
await until(() => navigations.length === 2);
const { connection } = await request(`/api/providers/${created.connection.id}`);
assert.equal(connection.providerSpecificData.baseUrl, `${endpoint}/native/v1`);
assert.equal(connection.providerSpecificData.nodeName, "Updated endpoint");
assert.equal(connection.providerSpecificData.prefix, "updated-fixture");
assert.equal(connection.providerSpecificData.apiType, "chat");
assert(!JSON.stringify(connection).includes(savedKey));

const before = calls.length;
forms("node-delete").emit("click");
assert.equal(calls.length, before);
confirmDelete = true;
forms("node-delete").emit("click");
await until(() => navigations.length === 3);
assert.equal(navigations.at(-1), `${providers}#provider-nodes`);
assert.equal(calls.at(-1).method, "DELETE");
assert.equal((await send(`/api/providers/${created.connection.id}`)).status, 404);
assert.equal((await send(`${providers}/nodes/${id}`)).status, 404);

const embedding = "/dashboard/media-providers/embedding";
const embeddingList = await send(embedding);
assert.equal(embeddingList.status, 200);
assert((await embeddingList.text()).includes(`${providers}/nodes/new?type=custom-embedding`));
forms = await page(`${providers}/nodes/new?type=custom-embedding`, "provider-node.js", "provider-node");
await until(() => !forms("node-fields").disabled);
assert.equal(forms("node-type").value, "custom-embedding");
assert(forms("node-api-fields").hidden && forms("node-check-model").required);
forms("node-url").value = `${endpoint}/embeds/v1`;
forms("node-check-key").value = probeKey;
const beforeMissing = calls.length;
forms("node-check-form").emit("submit");
await until(() => !forms("node-check-fields").disabled);
assert.equal(calls.length, beforeMissing);
assert.equal(forms("node-check-status").textContent, "Enter an embedding model ID.");
forms("node-check-model").value = "fixture-embedding";
forms("node-check-form").emit("submit");
await until(() => !forms("node-check-fields").disabled);
assert.equal(forms("node-check-status").textContent, "Embedding check passed (3 dimensions).");

forms("node-name").value = "Fixture embedding";
forms("node-prefix").value = "fixture-embedding";
forms("node-url").value += "/embeddings";
forms("node-form").emit("submit");
await until(() => navigations.length === 4);
const embedId = decodeURIComponent(navigations.at(-1).split("/").at(-1));
assert.equal(navigations.at(-1), `${embedding}/${embedId}`);
assert(!("apiKey" in calls.at(-1).body));
const saved = (await request(nodesUrl)).nodes.find(node => node.id === embedId);
assert.equal(saved.baseUrl, `${endpoint}/embeds/v1`);
assert(!("apiType" in saved));
const mediaDetail = await send(navigations.at(-1));
assert.equal(mediaDetail.status, 200);
assert((await mediaDetail.text()).includes(`href="${providers}/nodes/${embedId}"`));

const listWithEmbedding = await page(providers, "management.js", "providers");
await until(() => listWithEmbedding("provider-node-list").children.length > 0);
const embedEntry = listWithEmbedding("provider-node-list").children
  .find(row => row.children[0].children[0].textContent === "Fixture embedding");
assert.equal(embedEntry.children[1].children[0].href, `${embedding}/${embedId}`);
assert.equal(embedEntry.children[1].children[1].href, `${providers}/nodes/${embedId}`);
const keys = [];
for (const name of ["Embeddings first key", "Embeddings second key"]) {
  const result = await request("/api/providers", "POST", { provider: embedId, name, apiKey: savedKey });
  keys.push(result.connection.id);
}
assert.notEqual(keys[0], keys[1]);

forms = await page(`${providers}/nodes/${embedId}`, "provider-node.js", "provider-node");
await until(() => !forms("node-fields").disabled);
assert.equal(forms("node-type").value, "custom-embedding");
assert.equal(forms("node-check-key").value, "");
forms("node-name").value = "Updated embedding";
forms("node-prefix").value = "updated-embeddings";
forms("node-form").emit("submit");
await until(() => navigations.length === 5);
for (const key of keys) {
  const { connection } = await request(`/api/providers/${key}`);
  assert.equal(connection.providerSpecificData.prefix, "updated-embeddings");
  assert.equal(connection.providerSpecificData.nodeName, "Updated embedding");
  assert(!JSON.stringify(connection).includes(savedKey));
}
forms("node-delete").emit("click");
await until(() => navigations.length === 6);
assert.equal(navigations.at(-1), embedding);
for (const key of keys) assert.equal((await send(`/api/providers/${key}`)).status, 404);
console.log("Compatible endpoint VM verified check, create, saved-key connection, edit propagation and confirmed deletion.");
