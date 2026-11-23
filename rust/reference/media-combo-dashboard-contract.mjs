import assert from "node:assert/strict";
import { execFileSync, spawn } from "node:child_process";
import { once } from "node:events";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { runInNewContext } from "node:vm";

const assets = new URL("../app/src/dashboard_assets/", import.meta.url);
const html = readFileSync(new URL("media-combo.html", assets), "utf8");
const code = readFileSync(new URL("integrations.js", assets), "utf8");
const settingsPath = "/api/settings";
const logsPath = "/api/usage/logs";
const key = "offline-media-preview+key";
const settle = () => new Promise((finish) => setImmediate(finish));
const plain = (value) => value === undefined ? value : JSON.parse(JSON.stringify(value));

class Element {
  constructor(attributes = "") {
    this.listeners = new Map();
    this.dataset = {};
    this.value = "";
    this.textContent = "";
    this.disabled = /\bdisabled\b/.test(attributes);
    this.hidden = /\bhidden\b/.test(attributes);
  }
  querySelector(selector) { return selector === "fieldset" ? this.fieldset : this.submit; }
  addEventListener(event, callback) { this.listeners.set(event, callback); }
  removeAttribute(name) { delete this[name]; }
  async fire(event) {
    this.listeners.get(event)?.({ currentTarget: this, preventDefault() {} });
    await settle();
  }
}

function mount(template, script, id, request, fetch) {
  const elements = new Map([...template.matchAll(/<[^>]+\bid="([^"]+)"[^>]*>/g)]
    .map((match) => [match[1], new Element(match[0])]));
  const get = (suffix) => elements.get("media-combo-" + suffix);
  elements.get("media-page").dataset.combo = id;
  get("form").submit = new Element();
  get("form").fieldset = new Element("disabled");
  const events = new Map();
  const blobs = [];
  const revoked = [];
  class FixtureURL extends URL {
    static createObjectURL(blob) {
      blobs.push(blob);
      return "blob:fixture-" + blobs.length;
    }
    static revokeObjectURL(url) { revoked.push(url); }
  }
  runInNewContext(script, {
    document: { body: { dataset: { page: "media-combo" } } },
    window: {
      nineRouterUi: {
        select: (name) => elements.get(name),
        request,
        notice(node, text = "", failed = false) {
          node.textContent = text;
          node.hidden = !text;
          node.failed = failed;
        },
      },
      addEventListener: (event, handler) => events.set(event, handler),
      confirm: () => true,
      location: { replace() { throw new Error("Unexpected sign-in redirect"); }, assign() {} },
    },
    fetch, URL: FixtureURL, URLSearchParams, AbortController,
  });
  return { get, events, blobs, revoked, notice: elements.get("integration-status") };
}

function fixture(kind, members = ["mock/member"]) {
  const id = "demo+combo";
  const calls = [];
  const previews = [];
  const state = {
    combo: { id, kind, name: "offline-combo", models: members },
    strategies: { other: { fallbackStrategy: "smart" } },
    logs: ["offline-combo first", "unrelated"],
    status: 200,
    type: "application/json",
    payload: { ok: true, content: "Offline sample" },
    keys: [{ key: "disabled-secret", isActive: false }, { key }],
  };
  const rig = mount(html, code, id, async (path, method = "GET", body) => {
    calls.push({ path, method, body: plain(body) });
    if (path === "/api/combos/demo%2Bcombo") {
      if (method === "PUT") state.combo = { ...state.combo, ...plain(body) };
      return plain(state.combo);
    }
    if (path === settingsPath) {
      if (method === "PATCH") {
        if (state.rejectToggle) throw new Error("Could not save rotation");
        state.strategies = plain(body.comboStrategies);
      }
      return { comboStrategies: plain(state.strategies) };
    }
    if (path === logsPath) return [...state.logs];
    if (path === "/api/keys") return { keys: state.keys };
    throw new Error("Unexpected dashboard endpoint");
  }, async (path, options) => {
    previews.push({ path, method: options.method, authorization: options.headers.Authorization,
      body: JSON.parse(options.body), signal: options.signal });
    return {
      status: state.status,
      ok: state.status < 300,
      headers: { get: () => state.type },
      json: async () => state.payload,
      blob: async () => ({ bytes: 8, type: state.type }),
    };
  });
  return { ...rig, calls, previews, state };
}

const tts = fixture("tts");
await settle();
assert.equal(tts.get("logs").textContent, "offline-combo first");
assert.equal(tts.get("rotate").disabled, false);
assert.equal(tts.get("rotate").checked, false);
assert.equal(tts.get("run").disabled, false);
assert.equal(tts.previews.length, 0);
assert.equal(tts.get("endpoint").textContent, "POST /api/v1/audio/speech");
assert.equal(tts.get("example").textContent.includes(key), false);
tts.get("rotate").checked = true;
await tts.get("rotate").fire("change");
assert.deepEqual(tts.state.strategies, {
  other: { fallbackStrategy: "smart" },
  "offline-combo": { fallbackStrategy: "round-robin" },
});
tts.state.rejectToggle = true;
tts.get("rotate").checked = false;
await tts.get("rotate").fire("change");
assert.equal(tts.get("rotate").checked, true);
assert.equal(tts.notice.textContent, "Could not save rotation");
tts.state.rejectToggle = false;
tts.get("rotate").checked = false;
await tts.get("rotate").fire("change");
assert.deepEqual(tts.state.strategies, { other: { fallbackStrategy: "smart" } });

tts.state.type = "audio/mpeg; charset=binary";
tts.state.logs = ["unrelated", ...Array.from({ length: 55 }, (_, i) => "offline-combo run " + i)];
await tts.get("run").fire("click");
const [audio] = tts.previews;
assert.deepEqual({ path: audio.path, method: audio.method, authorization: audio.authorization, body: audio.body }, {
  path: "/api/v1/audio/speech", method: "POST", authorization: "Bearer " + key,
  body: { model: "offline-combo", input: "Hello, this is a test.", voice: "alloy" },
});
assert.equal(tts.get("audio").src, "blob:fixture-1");
assert.equal(tts.get("audio").hidden, false);
assert.equal(tts.get("download").download, "speech.mp3");
assert.equal(tts.get("logs").textContent.split("\n").length, 50);
assert.doesNotMatch(tts.get("logs").textContent, /unrelated|run 50/);
tts.state.status = 403;
tts.state.type = "application/json";
tts.state.payload = { error: { message: `Echo ${key} ${encodeURIComponent(key)}` } };
await tts.get("run").fire("click");
assert.deepEqual(tts.revoked, ["blob:fixture-1"]);
assert.equal(tts.get("audio").hidden, true);
assert.equal(tts.get("result-status").textContent, "Echo [redacted] [redacted]");
assert.doesNotMatch(tts.get("result").textContent, /offline-media-preview/);
tts.get("name").value = "renamed-combo";
await tts.get("form").fire("submit");
assert.equal(JSON.parse(tts.get("example").textContent).model, "renamed-combo");
assert.equal(tts.get("logs").textContent, "No usage yet.");

for (const [kind, path, body] of [
  ["webSearch", "/api/v1/search", { query: "What is the latest news about AI?", search_type: "web", max_results: 5 }],
  ["webFetch", "/api/v1/web/fetch", { url: "https://example.com", format: "markdown" }],
]) {
  const web = fixture(kind);
  await settle();
  web.state.keys = [];
  await web.get("run").fire("click");
  assert.equal(web.previews[0].path, path);
  assert.equal(web.previews[0].authorization, undefined);
  assert.deepEqual(web.previews[0].body, { model: "offline-combo", ...body });
  assert.match(web.get("result").textContent, /Offline sample/);
  assert.equal(web.get("back").href, "/dashboard/media-providers/web");
}

const rendered = fixture("image");
await settle();
rendered.state.payload = { data: [{ b64_json: "A".repeat(128) }] };
await rendered.get("run").fire("click");
assert.deepEqual(rendered.previews[0].body, {
  model: "offline-combo", prompt: "A cute cat playing piano", n: 1, size: "1024x1024",
});
assert.match(rendered.get("result").textContent, /128 chars base64/);
assert.match(rendered.get("image").src, /^data:image\/png;base64,/);
rendered.state.payload = { data: [{ url: "https://images.example.com/synthetic" }] };
await rendered.get("run").fire("click");
assert.equal(rendered.get("download").href, "https://images.example.com/synthetic");
assert.equal(rendered.get("image").hidden, true);
rendered.state.payload = { data: [{ url: "https://user:password@images.example.com/synthetic" }] };
await rendered.get("run").fire("click");
assert.equal(rendered.get("download").hidden, true);
rendered.state.type = "image/webp";
await rendered.get("run").fire("click");
assert.equal(rendered.get("download").download, "image.webp");
rendered.events.get("pagehide")();
assert.deepEqual(rendered.revoked, ["blob:fixture-1"]);

const empty = fixture("image", []);
const unknown = fixture("video");
await settle();
assert.equal(empty.get("run").disabled, true);
assert.equal(unknown.get("preview").hidden, true);
await empty.get("run").fire("click");
assert.equal(empty.previews.length, 0);
console.log("Media combo dashboard controls passed Node VM request, result and usage checks.");

async function verifyHttp(binary) {
  const scratch = mkdtempSync(join(tmpdir(), "9router-media-combo-"));
  const dataDir = join(scratch, "data");
  const env = { HOME: scratch, PATH: process.env.PATH, DATA_DIR: dataDir, HOSTNAME: "127.0.0.1",
    MODEL_CATALOG_SYNC: "off", DISABLE_BACKGROUND_TOKEN_REFRESH: "1", NODE_NO_WARNINGS: "1" };
  const upstream = [];
  const mock = createServer(async (request, response) => {
    const parts = [];
    for await (const part of request) parts.push(part);
    upstream.push({ path: request.url, body: JSON.parse(Buffer.concat(parts)), authorization: request.headers.authorization });
    const body = Buffer.from("synthetic TTS bytes");
    response.writeHead(200, { "Content-Type": "audio/mpeg", "Content-Length": body.length }).end(body);
  });
  let child;
  try {
    execFileSync("python3", [fileURLToPath(new URL("./seed.py", import.meta.url)), dataDir], { env });
    mock.listen(0, "127.0.0.1");
    await once(mock, "listening");
    const discovery = createServer();
    discovery.listen(0, "127.0.0.1");
    await once(discovery, "listening");
    env.PORT = String(discovery.address().port);
    const ended = once(discovery, "close");
    discovery.close();
    await ended;
    const fallback = join(scratch, "fallback.log");
    child = spawn(binary, ["--no-browser", "--no-tray", "--skip-update"],
      { cwd: scratch, env: { ...env, PATH: "", RUST_ANYTHING_FALLBACK_LOG: fallback }, stdio: "ignore" });
    const base = "http://127.0.0.1:" + env.PORT;
    const limit = Date.now() + 10000;
    while (true) {
      try {
        if ((await fetch(base + "/api/health")).ok) break;
      } catch {}
      assert.ok(Date.now() < limit && child.exitCode === null, "Standalone media fixture did not boot");
      await new Promise((finish) => setTimeout(finish, 25));
    }
    const call = (path, options) => fetch(base + path, { redirect: "manual", ...options });
    const asJson = (method, payload, headers = {}) => ({
      method, headers: { ...headers, "Content-Type": "application/json" }, body: JSON.stringify(payload),
    });
    const login = await call("/api/auth/login", asJson("POST", { password: "123456" }));
    assert.equal(login.status, 200);
    const cookie = { Cookie: login.headers.get("set-cookie").split(";", 1)[0] };
    const connection = await call("/api/providers", asJson("POST", {
      provider: "selfhosted-tts", name: "Offline speech", apiKey: "offline-provider-secret",
      providerSpecificData: { baseUrl: "http://127.0.0.1:" + mock.address().port },
    }, cookie));
    assert.equal(connection.status, 201);
    const created = await call("/api/combos", asJson("POST", {
      name: "offline-preview-combo", kind: "tts", models: ["selfhosted-tts/kokoro"],
    }, cookie));
    assert.equal(created.status, 201);
    const { id } = await created.json();
    execFileSync("python3", ["-c", `
import sqlite3, sys
with sqlite3.connect(sys.argv[1]) as db:
    db.executemany("INSERT INTO usageHistory (timestamp,provider,model,promptTokens,completionTokens,status) VALUES ('2026-11-20T00:00:00Z','selfhosted-tts',?,0,0,'success')",
                   [('offline-preview-combo',), ('unrelated-model',)])
`, join(dataDir, "db", "data.sqlite")], { env });
    const path = "/dashboard/media-providers/combo/" + id;
    assert.equal((await call(path)).status, 307);
    const page = await call(path, { headers: cookie });
    assert.equal(page.status, 200);
    assert.match(page.headers.get("content-security-policy"), /img-src blob: data:; media-src blob:/);
    const renderedPage = await page.text();
    for (const marker of ["Test Example", "Round Robin", "Usage Logs", 'controls preload="none"', "panel=legacy\" hidden"]) {
      assert.ok(renderedPage.includes(marker), marker);
    }
    assert.doesNotMatch(renderedPage, /offline-provider-secret|offline-chat-key/);
    const main = await call("/dashboard/media-providers/web", { headers: cookie });
    assert.doesNotMatch(main.headers.get("content-security-policy"), /media-src/);
    const script = await (await call("/__9router-ui/integrations.js")).text();
    const rig = mount(renderedPage, script, id, async (route, method = "GET", payload) => {
      const response = await call(route, payload === undefined ? { method, headers: cookie } : asJson(method, payload, cookie));
      assert.ok(response.ok, `${method} ${route} ${response.status}`);
      return response.json();
    }, call);
    async function until(predicate) {
      const deadline = Date.now() + 5000;
      while (!predicate()) {
        assert.ok(Date.now() < deadline, rig.notice.textContent);
        await new Promise((finish) => setTimeout(finish, 10));
      }
    }
    await until(() => !rig.get("rotate").disabled && rig.get("logs").textContent.includes("offline-preview-combo"));
    assert.doesNotMatch(rig.get("logs").textContent, /unrelated-model/);
    rig.get("rotate").checked = true;
    await rig.get("rotate").fire("change");
    await until(() => !rig.get("rotate").disabled);
    const saved = await (await call(settingsPath, { headers: cookie })).json();
    assert.deepEqual(saved.comboStrategies["offline-preview-combo"], { fallbackStrategy: "round-robin" });
    await rig.get("run").fire("click");
    await until(() => rig.blobs.length === 1);
    assert.equal(await rig.blobs[0].text(), "synthetic TTS bytes");
    assert.deepEqual(upstream, [{ path: "/v1/audio/speech",
      body: { model: "kokoro", voice: "af_heart", input: "Hello, this is a test.", response_format: "mp3" },
      authorization: "Bearer offline-provider-secret" }]);
    assert.equal(readFileSync(fallback, "utf8"), "");
    console.log("Mounted media combo dashboard passed authenticated HTTP controls with loopback speech and zero fallback.");
  } finally {
    if (child?.exitCode === null) {
      const stopped = once(child, "exit");
      child.kill("SIGTERM");
      await stopped;
    }
    mock.close();
    rmSync(scratch, { force: true, recursive: true });
  }
}

if (process.argv[2]) await verifyHttp(process.argv[2]);
