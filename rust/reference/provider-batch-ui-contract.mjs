import assert from "node:assert/strict";
import vm from "node:vm";

const [origin] = process.argv.slice(2);
const cookie = process.env.NINEROUTER_FIXTURE_COOKIE;
const calls = [];
const path = "/api/providers/test-batch";
let failTest = false;

async function request(route, method = "GET", body) {
  assert(route.startsWith("/"));
  calls.push({ route, method, body });
  if (route === path && failTest) throw new Error("offline-probe-key must never be displayed");
  const response = await fetch(`${origin}${route}`, {
    method, headers: { cookie, ...(body ? { "content-type": "application/json" } : {}) },
    ...(body ? { body: JSON.stringify(body) } : {}), redirect: "manual",
  });
  const data = await response.json();
  if (!response.ok) {
    const error = new Error(data.error);
    error.status = response.status;
    throw error;
  }
  return data;
}

class Control {
  constructor(text = "") {
    this.textContent = text || "";
    this.value = "";
    this.children = [];
    this.options = [];
    this.listeners = new Map();
    this.attributes = new Map();
    this.open = false;
  }
  addEventListener(action, callback) { this.listeners.set(action, callback); }
  emit(action) { return this.listeners.get(action)?.({ currentTarget: this, preventDefault() {} }); }
  setAttribute(name, value) { this.attributes.set(name, value); }
  append(...children) { this.children.push(...children); }
  replaceChildren(...children) { this.children = children; }
  showModal() { this.open = true; }
  close() { this.open = false; }
}

const page = await fetch(`${origin}/dashboard/providers`, { headers: { cookie } });
assert.equal(page.status, 200);
const html = await page.text();
const fields = new Map(Array.from(html.matchAll(/\bid="([^"]+)"/g)).map(match => [match[1], new Control()]));
const select = id => {
  assert(fields.has(id), `Missing provider control ${id}`);
  return fields.get(id);
};
const script = await fetch(`${origin}/__9router-ui/management.js`);
assert.equal(script.status, 200);
vm.runInNewContext(await script.text(), {
  URLSearchParams,
  document: { body: { dataset: { page: "providers" } } },
  window: {
    location: { replace: () => assert.fail("Unexpected login redirect") },
    nineRouterUi: {
      request, select,
      notice(control, message = "", failed = false) { control.textContent = message; control.failed = failed; },
      element(tag, text) { return new Control(text); },
      action(text) { return new Control(text); },
    },
  },
});

async function until(predicate) {
  for (let attempt = 0; attempt < 150; attempt++) {
    if (predicate()) return;
    await new Promise(resolve => setTimeout(resolve, 20));
  }
  throw new Error("Provider batch interaction did not complete");
}

const batch = () => calls.filter(call => call.route === path);
const form = select("provider-batch-form");
const group = select("provider-batch-group");
const button = select("provider-batch-run");
const dialog = select("provider-batch-dialog");
const summary = select("provider-batch-summary");
const list = select("provider-batch-results");
const status = select("provider-batch-status");
const connectionList = select("provider-list");

await until(() => connectionList.children.length > 0);
assert.equal(batch().length, 0);
group.value = "apikey";
const action = form.emit("submit");
assert(button.disabled && group.disabled);
assert.equal(form.attributes.get("aria-busy"), "true");
await form.emit("submit");
assert.equal(batch().length, 1);
await action;
assert.equal(JSON.stringify(batch()[0].body), JSON.stringify({ mode: "apikey", providerId: null }));
assert(dialog.open && !button.disabled && !group.disabled);
assert.equal(form.attributes.get("aria-busy"), "false");
assert.equal(summary.textContent, "2 tested · 1 passed · 1 failed");
assert.deepEqual(list.children.map(row => row.children[0].children[0].textContent), ["Healthy fixture", "Failed fixture"]);
assert.match(list.children[0].children[1].textContent, /^Passed · \d+ ms$/);
assert.match(list.children[1].children[1].textContent, /^Failed · \d+ ms$/);
assert.equal(list.children[0].attributes.get("role"), "listitem");
assert(connectionList.children.some(row => row.children[0].children[1].textContent.includes("Test failed")));
assert(!JSON.stringify(list.children).includes("offline-probe-key"));
select("provider-batch-close").emit("click");
assert(!dialog.open);

group.value = "oauth";
await form.emit("submit");
assert(dialog.open && select("provider-batch-results-title").textContent === "OAuth connection test results");
assert.equal(summary.textContent, "No active connections found for this group.");
assert.equal(list.children.length, 0);
select("provider-batch-close").emit("click");

group.value = "free";
await form.emit("submit");
assert.equal(summary.textContent, "1 tested · 0 passed · 1 failed");
assert.equal(list.children[0].children[0].children[0].textContent, "Expired free fixture");
assert.deepEqual(batch().map(call => call.body.mode), ["apikey", "oauth", "free"]);
select("provider-batch-close").emit("click");

failTest = true;
await form.emit("submit");
assert(!dialog.open && !button.disabled && !group.disabled);
assert(status.failed && status.textContent === "Could not test provider connections.");
console.log("Provider overview VM exercised explicit requests, single-flight, all three groups, result ordering, empty/failure states and refreshed health.");
