import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { runInNewContext } from "node:vm";

const script = readFileSync(0, "utf8");
const local = "http://127.0.0.1:20128";
const token = "offline-kimchi-test-bearer";
const state = "kimchi_random-test-value";

function run(url, rejectHistory = false) {
  const location = new URL(url, local);
  const actions = [];
  location.replace = (path) => {
    actions.push(["replace", path]);
    location.href = new URL(path, local).href;
  };
  const window = {
    location,
    history: { replaceState(_state, _title, path) {
      if (rejectHistory) throw new Error("Synthetic history restriction");
      actions.push(["history", path]);
      location.href = new URL(path, local).href;
    } },
    opener: { postMessage(data, origin) { actions.push(["window", origin, data.data]); } },
  };
  const status = {textContent: ""};
  class BroadcastChannel {
    constructor(name) { assert.equal(name, "oauth_callback"); }
    postMessage(data) { actions.push(["channel", local, data]); }
    close() { actions.push(["close"]); }
  }
  runInNewContext(script, { window, BroadcastChannel, URLSearchParams,
    document: { getElementById() { return status; } } });
  return {actions, location, status: status.textContent};
}

for (const original of [
  `/callback?token=${token}&state=${state}`,
  `/callback?code=${token}&state=${state}`,
  `/callback#access_token=${token}&state=${state}`,
  `/callback?state=${state}#access_token=${token}`,
]) {
  const {actions, location, status} = run(original);
  assert.deepEqual(actions[0], ["history", "/callback"]);
  assert.equal(location.href, `${local}/callback`);
  for (const item of actions.slice(1, 3)) {
    assert.equal(item[1], local);
    assert.equal(item[2].code, token);
    assert.equal(item[2].state, state);
    assert.ok(!Object.hasOwn(item[2], "fullUrl"));
  }
  assert.ok(!status.includes(token));
}
const malformed = run(`/callback?token=${token}`);
assert.deepEqual(malformed.actions, [["history", "/callback"]]);
const manual = run(`/callback?token=${token}&state=${state}`, true);
assert.deepEqual(manual.actions.at(-1), ["replace", "/callback"]);
const legacy = run("/callback?code=offline-ordinary-code&state=ordinary-state");
assert.equal(legacy.actions[0][0], "window");
assert.equal(legacy.actions[0][2].fullUrl, legacy.location.href);
assert.ok(legacy.location.search.includes("code="));
console.log("Kimchi callback removed bearer credentials from the URL before same-origin delivery");
