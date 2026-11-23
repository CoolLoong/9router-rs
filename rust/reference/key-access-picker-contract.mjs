import assert from "node:assert/strict";
import vm from "node:vm";
import { getProviderModels } from "../../open-sse/config/providerModels.js";

const [origin, keyId] = process.argv.slice(2);
const cookie = process.env.NINEROUTER_FIXTURE_COOKIE;
const calls = [];
let confirm = false;
const model = "openai/gpt-5.4";
const combo = "picker-fixture";
const alias = "picker-model-alias";
const manual = "openai/fixture-manual-id";
assert(getProviderModels("openai").some(entry => `openai/${entry.id}` === model));

async function localFetch(path, options = {}) {
  assert(path.startsWith("/") && !path.startsWith("//"));
  calls.push({ path, method: options.method || "GET", body: options.body ? JSON.parse(options.body) : null });
  return fetch(origin + path, {
    ...options,
    headers: { ...options.headers, cookie, "x-forwarded-for": "192.0.2.40" },
    redirect: "manual",
  });
}

class Control {
  constructor(tag = "") {
    this.tag = tag;
    this.children = [];
    this.dataset = {};
    this.listeners = new Map();
    this.attributes = new Map();
    this.open = false;
    this.value = "";
    this.disabled = false;
    this.submit = { disabled: false };
  }

  addEventListener(type, callback) { this.listeners.set(type, callback); }
  emit(type) { return this.listeners.get(type)?.({ currentTarget: this, preventDefault() {} }); }
  setAttribute(name, value) { this.attributes.set(name, value); }
  getAttribute(name) { return this.attributes.get(name); }
  append(...children) { this.children.push(...children); }
  replaceChildren(...children) { this.children = children; }
  showModal() { this.open = true; }
  close() { this.open = false; this.emit("close"); }
  querySelector() { return this.submit; }
  querySelectorAll(selector) {
    assert.equal(selector, "input[type=checkbox]");
    return this.children.flatMap(child => child.tag === "input" && child.type === "checkbox" ? [child] : child.querySelectorAll(selector));
  }
  focus() {}
  reset() {}
}

const source = await localFetch("/dashboard/endpoint");
assert.equal(source.status, 200);
const html = await source.text();
const ids = new Map(Array.from(html.matchAll(/\bid="([^"]+)"/g)).map(match => [match[1], new Control()]));
const select = id => {
  assert(ids.has(id), `Missing key control ${id}`);
  return ids.get(id);
};
const code = await localFetch("/__9router-ui/dashboard.js");
assert.equal(code.status, 200);
vm.runInNewContext(await code.text(), {
  fetch: localFetch, URL, URLSearchParams,
  document: {
    body: { dataset: { page: "endpoint" } }, documentElement: { dataset: {} },
    getElementById: select, createElement: tag => new Control(tag), querySelectorAll: () => [],
  },
  window: {
    matchMedia: () => ({ matches: false, addEventListener() {} }),
    location: { origin, replace: () => assert.fail("Unexpected login redirect") }, confirm: () => confirm,
  },
});

async function until(predicate) {
  for (let attempt = 0; attempt < 200; attempt++) {
    if (predicate()) return;
    await new Promise(resolve => setTimeout(resolve, 20));
  }
  throw new Error("Key picker interaction did not finish");
}

const dialog = select("key-access-dialog");
const list = select("key-list");
const picker = select("access-model-picks");
const allow = select("access-allow");
const input = select("access-search");
const form = select("key-access-form");
const keyRow = () => list.children.find(row => row.children[0]?.children[0]?.textContent === "Fixture picker key");
const button = text => keyRow().children[1].children.find(item => item.textContent === text);
const writes = () => calls.filter(call => call.path === `/api/keys/${keyId}` && call.method === "PUT");
const options = () => picker.querySelectorAll("input[type=checkbox]");
const search = text => { input.value = text; input.emit("input"); };
const change = (id, checked = true) => {
  const option = options().find(option => option.value === id);
  assert(option, `Missing access option ${id}`);
  option.checked = checked;
  option.emit("change");
};

await until(() => keyRow());
button("Edit access").emit("click");
await until(() => options().length > 0);
assert(dialog.open);
assert.equal(select("access-list-fields").disabled, true);
assert(picker.children.some(group => group.children[0].textContent === "Combos"));
assert(picker.children.some(group => group.children[0].textContent === "Model aliases"));
assert(calls.some(({ path }) => path === "/api/models/access-options"));
assert(!calls.some(({ path }) => path === "/v1/models"));
select("access-restricted").checked = true;
select("access-restricted").emit("change");
assert(!select("access-list-fields").disabled);

search(alias);
change(alias);
change(alias, false);
assert.equal(allow.value, "");
search("gpt-5.4");
allow.value = model.toUpperCase();
allow.emit("input");
assert(options().find(option => option.value === model).checked);
change(model, false);
assert.equal(allow.value, "");
change(model);
search(combo);
change(combo);
search("disabled-fixture-not-offered");
assert.equal(options().length, 0);
assert.match(select("access-picker-status").textContent, /No matching/);
allow.value += `\n${manual}`;
allow.emit("input");
assert.equal(writes().length, 0);

await form.emit("submit");
assert.equal(writes().length, 0);
assert(dialog.open);
confirm = true;
await form.emit("submit");
assert(!dialog.open);
assert.deepEqual(writes().at(-1).body.access, { restricted: true, allow: [model, combo, manual] });
confirm = false;
await button("Disable").emit("click");
assert.equal(writes().length, 1);
confirm = true;
await button("Disable").emit("click");
assert.deepEqual(writes().at(-1).body, { isActive: false });
await button("Enable").emit("click");
assert.deepEqual(writes().at(-1).body, { isActive: true });
console.log("Node VM verified remote grouped picker choices against pinned model IDs, staged edits, case-insensitive toggles and key confirmation.");
