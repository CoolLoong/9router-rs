import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { runInNewContext } from "node:vm";

const script = readFileSync(new URL("../app/src/dashboard_assets/integrations.js", import.meta.url), "utf8");
const endpoint = "/api/cli-tools/cowork-settings";
const flush = () => new Promise(done => setImmediate(done));

class Element {
  constructor() {
    this.dataset = {};
    this.children = [];
    this.handlers = new Map();
    this.value = "";
    this.checked = false;
    this.hidden = false;
    this.disabled = false;
  }
  addEventListener(name, handler) { this.handlers.set(name, handler); }
  querySelector() { return this.submit; }
  append(...nodes) { this.children.push(...nodes); }
  async fire(type) {
    this.handlers.get(type)?.({ currentTarget: this, preventDefault() {} });
    await flush();
  }
}

async function fixture(offered) {
  const ids = ["integration-status", "cli-page", "cli-base-url", "cli-model", "cli-key", "cli-preview",
    "cli-tool-status", "cli-config-form", "cli-host-actions", "cli-extra-models", "cli-copy", "cli-refresh",
    "cli-restore", "cli-model-options", "cowork-connector-selection", "cowork-existing-connectors",
    "cowork-browser-preset", "cowork-browser-preset-choice", "cowork-catalog", "cowork-load-catalog", "cowork-add-connector"];
  const elements = Object.fromEntries(ids.map(id => [id, new Element()]));
  const get = id => elements[id];
  get("cli-page").dataset.tool = "cowork";
  get("cli-config-form").submit = new Element();
  get("cowork-browser-preset").disabled = true;
  get("cowork-browser-preset-choice").hidden = true;
  let saved = ["browsermcp"];
  const reads = [];
  const mutations = [];
  runInNewContext(script, {
    URLSearchParams,
    document: { body: { dataset: { page: "cli-detail" } }, querySelectorAll: () => [] },
    window: {
      open: () => { throw new Error("Unexpected browser invocation"); },
      location: { origin: "http://127.0.0.1:20128" },
      confirm: () => true,
      nineRouterUi: {
        select: get,
        notice: (target, text) => { target.textContent = text; },
        element: () => new Element(),
        request: async (path, method = "GET", body) => {
          if (method === "GET") {
            reads.push(path);
            if (path === "/api/keys") return { keys: [{ id: "fake", key: "synthetic-cowork-key" }] };
            if (path === "/api/models/alias") return { aliases: {} };
            if (path === "/__9router-ui/cli-status?tool=cowork") return { cowork: { installed: true } };
            assert.equal(path, endpoint);
            return { localStdioPlugins: offered ? [{ name: "browsermcp", title: "Browser MCP" }] : [],
              cowork: { baseUrl: "http://127.0.0.1:20128/v1", models: ["fixture/model"],
                plugins: [{ name: "Catalog fixture" }], customPlugins: [], localPlugins: saved } };
          }
          assert.equal(path, endpoint);
          const payload = body && JSON.parse(JSON.stringify(body));
          mutations.push({ method, payload });
          if (method === "DELETE") saved = [];
          else if (payload.localPlugins) saved = payload.localPlugins;
          return { success: true };
        },
      },
    },
  });
  await flush();
  return { get, reads, mutations, submit: () => get("cli-config-form").fire("submit") };
}

const local = await fixture(true);
const choice = local.get("cowork-browser-preset-choice");
const preset = local.get("cowork-browser-preset");
const current = local.get("cowork-existing-connectors");
assert.equal(choice.hidden, false);
assert.equal(preset.disabled, false);
assert.equal(preset.checked, true);
assert.match(current.textContent, /Catalog fixture, Browser MCP/);
assert.equal(local.mutations.length, 0);
preset.checked = false;
await local.submit();
assert.deepEqual(local.mutations[0], { method: "POST", payload: {
  baseUrl: "http://127.0.0.1:20128/v1", model: "fixture/model", models: ["fixture/model"],
  localPlugins: [], apiKey: "synthetic-cowork-key",
} });
assert.equal(preset.checked, false);
assert.doesNotMatch(current.textContent, /Browser MCP/);
preset.checked = true;
local.get("cowork-connector-selection").value = '[{"name":"Fixture URL","url":"https://fixture.invalid/mcp","custom":true}]';
await local.submit();
assert.deepEqual(local.mutations[1].payload.localPlugins, ["browsermcp"]);
assert.deepEqual(local.mutations[1].payload.customPlugins, [{ name: "Fixture URL", url: "https://fixture.invalid/mcp", custom: true }]);
assert.equal(preset.checked, true);
await local.get("cli-restore").fire("click");
assert.equal(local.mutations[2].method, "DELETE");
assert.equal(local.reads.at(-1), endpoint);
assert.equal(preset.checked, false);

const remote = await fixture(false);
const unavailable = remote.get("cowork-browser-preset");
assert.equal(remote.get("cowork-browser-preset-choice").hidden, true);
assert.equal(unavailable.disabled, true);
assert.equal(unavailable.checked, false);
unavailable.checked = true;
await remote.submit();
assert.ok(!Object.hasOwn(remote.mutations[0].payload, "localPlugins"));
console.log("Cowork preset VM passed saved display, explicit selection, retained URL options, reset and the hidden remote state.");
