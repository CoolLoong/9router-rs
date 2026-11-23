import assert from "node:assert/strict";
import { copyFile, mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

const folder = await mkdtemp(join(tmpdir(), "netlify-lambda-"));
try {
  const code = new URL("../app/src/", import.meta.url);
  await Promise.all([
    copyFile(new URL("netlify_relay_function.cjs", code), join(folder, "relay.js")),
    copyFile(new URL("cloudflare_relay_worker.mjs", code), join(folder, "relay.mjs")),
    copyFile(new URL("pinned_relay_https.mjs", code), join(folder, "pinned-https.mjs")),
  ]);
  const lambda = await import(pathToFileURL(join(folder, "relay.js")));
  assert.equal(typeof lambda.handler, "function");
  const url = "https://managed-fixture.netlify.app/.netlify/functions/relay";
  const secret = "N".repeat(43);
  const calls = [];
  let mode = "stream";
  const invoke = lambda.createHandler({ RELAY_AUTH: secret, RELAY_URL: url }, async (target, init) => {
    calls.push([target, init]);
    assert.equal(String(target), "https://api.openai.com/v1/chat/completions?fixture=true");
    assert.equal(init.headers.get("authorization"), "Bearer offline-provider");
    assert.ok(!Array.from(init.headers.keys()).some(name => name.startsWith("x-relay-")));
    assert.equal(init.redirect, "manual");
    const decoded = await new Response(init.body).text();
    assert.equal(decoded, "sample request");
    if (mode === "redirect") return new Response("ignored", { status: 302, headers: { location: url } });
    if (mode === "large") return new Response(new Uint8Array(4 * 1024 * 1024 + 1));
    return new Response("data: first\n\ndata: last\n\n", {
      headers: { "content-type": "text/event-stream", "x-relay-auth": "discard", "x-fixture": "saved" },
    });
  });
  const event = {
    httpMethod: "POST",
    rawUrl: url,
    headers: {
      "X-Relay-Auth": secret,
      "x-relay-target": "https://api.openai.com",
      "x-relay-path": "/v1/chat/completions?fixture=true",
      "Authorization": "Bearer offline-provider",
    },
    isBase64Encoded: true,
    body: Buffer.from("sample request").toString("base64"),
  };
  assert.equal((await invoke({ ...event, rawUrl: url + "/not-authorized" })).statusCode, 403);
  assert.equal((await invoke({ ...event, rawUrl: url.replace("managed-fixture", "other-app") })).statusCode, 403);
  assert.equal((await invoke({ ...event, headers: { ...event.headers, "X-Relay-Auth": "incorrect" } })).statusCode, 403);
  assert.equal((await invoke({ ...event, headers: { ...event.headers, "x-relay-target": "https://127.0.0.1" } })).statusCode, 400);
  assert.equal((await invoke({ ...event, body: Buffer.alloc(4 * 1024 * 1024 + 1).toString("base64") })).statusCode, 413);
  assert.deepEqual(calls, []);
  const finished = await invoke(event);
  assert.equal(finished.statusCode, 200);
  assert.equal(finished.isBase64Encoded, true);
  assert.equal(finished.headers["content-type"], "text/event-stream");
  assert.equal(finished.headers["x-fixture"], "saved");
  assert.equal(finished.headers["x-relay-auth"], undefined);
  assert.equal(Buffer.from(finished.body, "base64").toString(), "data: first\n\ndata: last\n\n");
  mode = "redirect";
  const blocked = await invoke(event);
  assert.equal(blocked.statusCode, 502);
  assert.equal(blocked.headers.location, undefined);
  mode = "large";
  const capped = await invoke(event);
  assert.equal(capped.statusCode, 502);
  assert.deepEqual(JSON.parse(capped.body), { error: "Relay response is too large" });
  assert.equal(calls.length, 3);
  console.log("Netlify Lambda event authentication, allowlisted route, bounded SSE and redirect handling passed");
} finally {
  await rm(folder, { recursive: true, force: true });
}
