import assert from "node:assert/strict";
import worker from "../app/src/cloudflare_relay_worker.mjs";

const deploy = "https://managed.fixture-account.workers.dev";
const secret = "T".repeat(43);
const bindings = { RELAY_AUTH: secret, RELAY_URL: deploy };
const required = {
  "x-relay-auth": secret,
  "x-relay-target": "https://api.openai.com",
  "x-relay-path": "/v1/chat/completions?mode=offline",
};
const calls = [];
let redirect = false;
globalThis.fetch = async (target, request) => {
  calls.push([target, request]);
  assert.equal(String(target), "https://api.openai.com/v1/chat/completions?mode=offline");
  assert.equal(request.redirect, "manual");
  assert.equal(request.cache, "no-store");
  if (redirect) {
    return new Response("redirect fixture", { status: 307, headers: { location: "https://example.org/" } });
  }
  return new Response("upstream fixture", {
    status: 201,
    headers: { "x-relay-target": "ignored", "x-relay-auth": "ignored", "x-upstream": "preserved" },
  });
};

const missing = { ...required };
delete missing["x-relay-auth"];
for (const [url, headers, status] of [
  [deploy, missing, 403],
  [deploy, { ...required, "x-relay-auth": "X".repeat(43) }, 403],
  [deploy + "/other", required, 403],
  [deploy + "/?other=true", required, 403],
  ["https://wrong.fixture-account.workers.dev", required, 403],
  [deploy, { ...required, "x-relay-target": "http://api.openai.com" }, 400],
  [deploy, { ...required, "x-relay-target": "https://127.1" }, 400],
  [deploy, { ...required, "x-relay-target": "https://[::1]" }, 400],
  [deploy, { ...required, "x-relay-target": "https://service.internal" }, 400],
  [deploy, { ...required, "x-relay-target": "https://user@api.openai.com" }, 400],
  [deploy, { ...required, "x-relay-target": "https://api.openai.com:8443" }, 400],
  [deploy, { ...required, "x-relay-path": "//example.org/escaped" }, 400],
  [deploy, { ...required, "x-relay-path": "/\\example.org/escaped" }, 400],
  [deploy, { ...required, "x-relay-target": deploy }, 400],
]) {
  const response = await worker.fetch(new Request(url, { headers }), bindings);
  assert.equal(response.status, status, url);
  assert.ok(!(await response.text()).includes(secret));
}
assert.deepEqual(calls, []);

const result = await worker.fetch(new Request(deploy, {
  method: "POST",
  headers: {
    ...required,
    "authorization": "Bearer offline-provider",
    "host": "not-an-upstream",
    "connection": "x-connection-data",
    "x-connection-data": "drop",
    "x-forwarded-for": "192.0.2.10",
    "x-relay-another": "drop",
    "proxy-authorization": "drop",
  },
  body: "sent safely",
}), bindings);
assert.equal(result.status, 201);
assert.equal(await result.text(), "upstream fixture");
assert.equal(result.headers.get("x-upstream"), "preserved");
assert.equal(result.headers.get("cache-control"), "no-store");
assert.ok(!result.headers.has("x-relay-auth") && !result.headers.has("x-relay-target"));
assert.equal(calls.length, 1);
const [, forwarded] = calls[0];
assert.equal(forwarded.method, "POST");
assert.equal(forwarded.headers.get("authorization"), "Bearer offline-provider");
assert.equal(await new Response(forwarded.body).text(), "sent safely");
for (const name of Object.keys(required).concat([
  "host", "connection", "x-connection-data", "x-forwarded-for", "proxy-authorization", "x-relay-another",
])) {
  assert.equal(forwarded.headers.has(name), false, name);
}

redirect = true;
const blocked = await worker.fetch(new Request(deploy, { headers: required }), bindings);
assert.equal(blocked.status, 502);
assert.equal(blocked.headers.has("location"), false);
assert.equal(calls.length, 2);
console.log("Cloudflare Worker authorization, public HTTPS target guards and credential-safe forwarding passed");
