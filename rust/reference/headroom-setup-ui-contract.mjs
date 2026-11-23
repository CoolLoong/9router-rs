import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { runInNewContext } from "node:vm";

const asset = (name) => readFileSync(new URL(`../app/src/dashboard_assets/${name}`, import.meta.url), "utf8");
const html = asset("token-saver.html");
assert.match(html, /href="\/api\/headroom\/proxy\/dashboard"/);
assert.doesNotMatch(html, /panel=legacy/);

const elements = new Map();
const select = (id) => {
  if (!elements.has(id)) elements.set(id, {
    hidden: true,
    addEventListener(type, callback) { this[type] = callback; },
  });
  return elements.get(id);
};
const start = select("start");
const stop = select("stop");
start.dataset = { headroom: "start" };
stop.dataset = { headroom: "stop" };
const controls = {
  '[data-headroom="start"]': start,
  '[data-headroom="stop"]': stop,
};
select("headroom-install-command").textContent =
  html.match(/id="headroom-install-command"[^>]*>([^<]*)</)[1];

const operations = [];
const clipboard = [];
let state = { localUrl: true, installed: false, running: false, python: "/fixture/python3" };
const request = async (path, method) => {
  if (path === "/api/settings") return {};
  if (path === "/api/headroom/status") return state;
  assert.equal(method, "POST");
  operations.push(path);
  if (path.endsWith("/start")) state = { ...state, running: true, managedPid: 21 };
  if (path.endsWith("/stop")) state = { ...state, running: false, managedPid: null };
  return {};
};
runInNewContext(asset("optimization.js"), {
  document: {
    body: { dataset: { page: "token-saver" } },
    querySelectorAll: (selector) => selector === "[data-headroom]" ? [start, stop] : [],
    querySelector: (selector) => controls[selector],
  },
  navigator: { clipboard: { async writeText(value) { clipboard.push(value); } } },
  window: { nineRouterUi: { select, request, notice(output, text) { output.textContent = text; } } },
});
await new Promise(resolve => setImmediate(resolve));

const visible = (...ids) => ids.map(id => !select(id).hidden);
const check = () => select("headroom-status").click({ currentTarget: select("headroom-status") });
assert.deepEqual(visible("headroom-install", "headroom-dashboard", "start", "stop"), [true, false, false, false]);
await select("headroom-install-copy").click({ currentTarget: select("headroom-install-copy") });
assert.deepEqual(clipboard, ['pip install "headroom-ai[proxy]"']);
assert.deepEqual(operations, []);

state = { ...state, python: null };
await check();
assert.match(select("headroom-setup-hint").textContent, /Python 3\.10/);
assert.deepEqual(visible("headroom-install", "headroom-setup-hint"), [false, true]);

state = { ...state, localUrl: false, running: false };
await check();
assert.equal(select("headroom-result").textContent, "External");
assert.deepEqual(visible("start", "stop", "headroom-install"), [false, false, false]);

state = { ...state, localUrl: true, installed: true, canStart: true };
await check();
assert.deepEqual(visible("start", "stop", "headroom-dashboard"), [true, false, false]);
await start.click();
assert.deepEqual(visible("start", "stop", "headroom-dashboard"), [false, true, true]);
await stop.click();
assert.deepEqual(operations, ["/api/headroom/start", "/api/headroom/stop"]);
assert.deepEqual(visible("start", "stop", "headroom-dashboard"), [true, false, false]);

state = { ...state, localUrl: false, running: true, managedPid: 45 };
await check();
assert.deepEqual(visible("start", "stop", "headroom-dashboard"), [false, false, true]);
console.log("Headroom setup rendered install, external, managed and running states without opening a browser.");
