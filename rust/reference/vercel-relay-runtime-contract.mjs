import assert from "node:assert/strict";
import { copyFile, mkdir, mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

const folder = await mkdtemp(join(tmpdir(), "vercel-relay-"));
try {
  await mkdir(join(folder, "api"));
  await mkdir(join(folder, "lib"));
  const source = new URL("../app/src/", import.meta.url);
  await Promise.all([
    copyFile(new URL("vercel_relay_function.mjs", source), join(folder, "api/relay.mjs")),
    copyFile(new URL("cloudflare_relay_worker.mjs", source), join(folder, "lib/relay.mjs")),
    copyFile(new URL("pinned_relay_https.mjs", source), join(folder, "lib/pinned-https.mjs")),
  ]);
  const runtime = await import(pathToFileURL(join(folder, "api/relay.mjs")));
  assert.equal(typeof runtime.default.fetch, "function");
  const secret = "V".repeat(43);
  const origin = "managed-fixture.vercel.app";
  const url = "https://" + origin + "/api/relay";
  const bindings = { VERCEL_ENV: "production", VERCEL_PROJECT_PRODUCTION_URL: origin, RELAY_AUTH: secret };
  const calls = [];
  let finish;
  let redirect = false;
  const send = async (destination, request) => {
    calls.push([destination, request]);
    assert.equal(String(destination), "https://api.openai.com/v1/chat/completions?fixture=true");
    assert.equal(request.redirect, "manual");
    assert.equal(request.headers.get("authorization"), "Bearer offline-provider");
    assert.equal(request.headers.get("x-vercel-oidc-token"), null);
    assert.equal(request.headers.get("x-relay-auth"), null);
    assert.equal(await new Response(request.body).text(), "test streaming");
    if (redirect) return new Response("ignored", { status: 301, headers: { location: url } });
    const encoder = new TextEncoder();
    const stream = new ReadableStream({
      start(controller) {
        controller.enqueue(encoder.encode("data: first\n\n"));
        finish = () => {
          controller.enqueue(encoder.encode("data: final\n\n"));
          controller.close();
        };
      },
    });
    return new Response(stream, { headers: { "content-type": "text/event-stream",
      "x-vercel-debug": "discard", "x-relay-auth": "discard" } });
  };
  const handler = runtime.createHandler(bindings, send);
  const request = (address = url, token = secret) => new Request(address, {
    method: "POST",
    body: "test streaming",
    headers: {
      authorization: "Bearer offline-provider",
      "x-relay-target": "https://api.openai.com",
      "x-relay-path": "/v1/chat/completions?fixture=true",
      "x-relay-auth": token,
      "x-vercel-oidc-token": "offline-instance-only",
    },
  });
  assert.equal((await handler(request(url, "wrong"))).status, 403);
  assert.equal((await handler(request(url + "?wrong=1"))).status, 403);
  assert.equal((await handler(request("https://some-preview.vercel.app/api/relay"))).status, 403);
  const preview = runtime.createHandler({ ...bindings, VERCEL_ENV: "preview" }, send);
  assert.equal((await preview(request())).status, 403);
  assert.equal((await handler(new Request(url))).status, 403);
  const challenge = await handler(new Request(url, { headers: { "x-relay-auth": secret } }));
  assert.equal(challenge.status, 400);
  assert.deepEqual(await challenge.json(), { error: "A public HTTPS destination is required" });
  assert.deepEqual(calls, []);

  const streamed = await handler(request());
  assert.equal(streamed.status, 200);
  assert.equal(streamed.headers.get("content-type"), "text/event-stream");
  assert.equal(streamed.headers.get("x-vercel-debug"), null);
  assert.equal(streamed.headers.get("x-relay-auth"), null);
  const reader = streamed.body.getReader();
  const decoder = new TextDecoder();
  assert.equal(decoder.decode((await reader.read()).value), "data: first\n\n");
  finish();
  assert.equal(decoder.decode((await reader.read()).value), "data: final\n\n");
  assert.equal((await reader.read()).done, true);
  redirect = true;
  const blocked = await handler(request());
  assert.equal(blocked.status, 502);
  assert.equal(blocked.headers.get("location"), null);
  console.log("Vercel function production-only authentication, streaming and platform header isolation passed");
} finally {
  await rm(folder, { recursive: true, force: true });
}
