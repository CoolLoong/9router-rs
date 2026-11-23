import assert from "node:assert/strict";
import fs from "node:fs";
import vm from "node:vm";

const script = fs.readFileSync(process.argv[2], "utf8");
const available = { currentVersion: "0.5.119", latestVersion: "0.5.120", hasUpdate: true };
const installed = { currentVersion: "0.5.120", latestVersion: "0.5.120", hasUpdate: false };
const get = "/api/version";
const post = "/api/version/update";
const gate = "/api/settings/require-login";

function setup(canInstall, signedIn, reply) {
  class Node {
    dataset = {};
    listeners = {};
    hidden = false;
    disabled = false;
    textContent = "";
    addEventListener(type, callback) { this.listeners[type] = callback; }
    click() { return this.listeners.click(); }
  }
  const elements = new Map(["native-update", "native-update-check", "native-update-install",
    "native-update-sign-in", "native-update-status"].map(id => [id, new Node()]));
  const select = id => elements.get(id);
  const panel = select("native-update");
  panel.dataset = { canInstall: String(canInstall), signedIn: String(signedIn) };
  const check = select("native-update-check");
  const install = select("native-update-install");
  const signIn = select("native-update-sign-in");
  const status = select("native-update-status");
  install.hidden = true;
  signIn.hidden = true;
  const requests = [];
  let clock = 0;
  vm.runInNewContext(script, {
    window: {
      nineRouterUi: {
        select,
        notice: (node, message, failed = false) => {
          node.textContent = message;
          node.hidden = !message;
          node.dataset.error = String(failed);
        },
      },
      setTimeout: (run, delay) => { clock += delay; queueMicrotask(run); },
    },
    AbortSignal,
    Date: { now: () => clock },
    fetch: async (path, options) => {
      assert.equal(options.cache, "no-store");
      assert.ok(options.signal instanceof AbortSignal);
      requests.push([path, options.method]);
      const result = await reply(path, options.method, requests.length);
      const statusCode = result.status ?? 200;
      return {
        ok: statusCode >= 200 && statusCode < 300,
        status: statusCode,
        json: async () => result.body,
      };
    },
  });
  assert.deepEqual(requests, []);
  return { check, install, status, signIn, requests };
}

{
  const payloads = [
    [get, { body: available }],
    [post, { body: { success: true }}],
    [get, new Error("Connection refused")],
    [get, { body: installed }],
    [gate, { status: 503, body: { message: "Waiting for commit" }}],
    [get, { body: installed }],
    [gate, { body: { requireLogin: true }}],
  ];
  const ui = setup(true, true, (path, method, index) => {
    const [next, response] = payloads[index - 1];
    assert.equal(path, next);
    assert.equal(method, path === post ? "POST" : "GET");
    if (response instanceof Error) throw response;
    return response;
  });
  await ui.install.click();
  assert.deepEqual(ui.requests, []);
  await ui.check.click();
  assert.equal(ui.install.hidden, false);
  assert.match(ui.status.textContent, /v0\.5\.120 is available/);
  const attempt = ui.install.click();
  assert.equal(ui.check.disabled, true);
  await ui.install.click();
  await ui.check.click();
  await attempt;
  assert.equal(ui.status.textContent, "Update complete. Running v0.5.120.");
  assert.equal(ui.install.hidden, true);
  assert.equal(ui.requests.length, payloads.length);
}

{
  const ui = setup(false, true, path => {
    assert.equal(path, get);
    return { body: available };
  });
  await ui.check.click();
  await ui.install.click();
  assert.equal(ui.install.hidden, true);
  assert.equal(ui.signIn.hidden, true);
  assert.match(ui.status.textContent, /Install the preview manually/);
  assert.deepEqual(ui.requests, [[get, "GET"]]);
}

{
  const ui = setup(true, false, () => ({ body: available }));
  await ui.check.click();
  await ui.install.click();
  assert.equal(ui.install.hidden, true);
  assert.equal(ui.signIn.hidden, false);
  assert.deepEqual(ui.requests, [[get, "GET"]]);
}

{
  const ui = setup(true, true, () => ({ body: { currentVersion: "0.5.119", latestVersion: null, hasUpdate: false }}));
  await ui.check.click();
  assert.equal(ui.install.hidden, true);
  assert.match(ui.status.textContent, /No signed release was returned/);
}

{
  const ui = setup(true, true, path => path === get ? { body: available } : {
    status: 409, body: { success: false, message: "Native update requires a persistent installation" },
  });
  await ui.check.click();
  await ui.install.click();
  assert.equal(ui.status.dataset.error, "true");
  assert.equal(ui.status.textContent, "Native update requires a persistent installation");
  assert.equal(ui.install.hidden, true);
  assert.deepEqual(ui.requests, [[get, "GET"], [post, "POST"]]);
}

{
  const ui = setup(true, true, path => ({
    body: path === post ? { success: true } : path === get ? available : { requireLogin: true },
  }));
  await ui.check.click();
  await ui.install.click();
  assert.match(ui.status.textContent, /Running v0\.5\.119.*did not confirm v0\.5\.120/);
  assert.equal(ui.check.disabled, false);
  assert.equal(ui.install.hidden, true);
  assert.equal(ui.requests.filter(([path]) => path === post).length, 1);
  assert.ok(ui.requests.length < 130);
}

console.log("Native dashboard updates passed six signed release cases.");
