import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { once } from "node:events";
import { copyFile, mkdtemp, readFile, rm } from "node:fs/promises";
import { createServer, request } from "node:https";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

const root = await mkdtemp(join(tmpdir(), "deno-relay-"));
let server;
try {
  const code = new URL("../app/src/", import.meta.url);
  await copyFile(new URL("deno_relay_runtime.mjs", code), join(root, "main.mjs"));
  await copyFile(new URL("cloudflare_relay_worker.mjs", code), join(root, "relay.mjs"));
  await copyFile(new URL("pinned_relay_https.mjs", code), join(root, "pinned-https.mjs"));
  const { pinnedHttps } = await import(pathToFileURL(join(root, "main.mjs")));
  const { forward } = await import(pathToFileURL(join(root, "relay.mjs")));
  execFileSync("openssl", [
    "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
    "-subj", "/CN=api.openai.com", "-addext", "subjectAltName=DNS:api.openai.com",
    "-keyout", join(root, "key.pem"), "-out", join(root, "cert.pem"),
  ], { stdio: "ignore" });
  const cert = await readFile(join(root, "cert.pem"));
  const key = await readFile(join(root, "key.pem"));
  const received = [];
  let redirect = false;
  server = createServer({ key, cert }, async (req, res) => {
    let body = "";
    for await (const chunk of req) body += chunk;
    received.push({ body, method: req.method, path: req.url, headers: req.headers });
    if (redirect) return res.writeHead(307, { location: "/other" }).end();
    res.writeHead(200, { "content-type": "text/event-stream" });
    res.write("data: first\n\n");
    res.end("data: last\n\n");
  }).listen(0, "127.0.0.1");
  await once(server, "listening");
  const port = server.address().port;
  const ip = "93.184.216.34";
  let answers = [ip];
  const queries = [];
  const sockets = [];
  const upstream = pinnedHttps(async hostname => {
    queries.push(hostname);
    return answers;
  }, (options, listener) => {
    sockets.push(options.hostname);
    assert.equal(options.hostname, ip);
    assert.equal(options.port, 443);
    assert.equal(options.headers.host, options.servername);
    assert.deepEqual(options.agent.options.proxyEnv, {});
    return request({ ...options, hostname: "127.0.0.1", port, ca: cert }, listener);
  });
  const url = "https://private-relay.fixture-org.deno.net";
  const secret = "D".repeat(43);
  const env = { RELAY_AUTH: secret, RELAY_URL: url };
  const relay = (destination = "https://api.openai.com", supplied = secret) =>
    forward(new Request(url, {
      method: "POST",
      body: "relay request",
      headers: {
        "x-relay-auth": supplied,
        "x-relay-target": destination,
        "x-relay-path": "/v1/chat/completions?fixture=yes",
        "authorization": "Bearer offline-model",
      },
    }), env, upstream);

  assert.equal((await relay("https://api.openai.com", "X".repeat(43))).status, 403);
  assert.equal((await relay("http://api.openai.com")).status, 400);
  assert.deepEqual(queries, []);
  for (const candidate of ["127.0.0.1", "169.254.169.254", "10.0.0.1", "100.64.0.1", "192.168.0.1",
                           "0.0.0.0", "198.18.0.1", "198.51.100.2", "203.0.113.3", "224.0.0.1", "::1"]) {
    answers = [ip, candidate];
    assert.equal((await relay()).status, 502, candidate);
    assert.deepEqual(sockets, []);
  }
  answers = [];
  assert.equal((await relay()).status, 502);
  assert.deepEqual(sockets, []);
  answers = [ip];
  const wrongCertificate = await relay("https://api.anthropic.com");
  assert.equal(wrongCertificate.status, 502);
  assert.deepEqual(received, []);
  const result = await relay();
  assert.equal(result.status, 200);
  assert.equal(await result.text(), "data: first\n\ndata: last\n\n");
  assert.equal(received.length, 1);
  const call = received[0];
  assert.equal(call.path, "/v1/chat/completions?fixture=yes");
  assert.equal(call.headers.authorization, "Bearer offline-model");
  assert.equal(call.headers.host, "api.openai.com");
  assert.ok(!Object.keys(call.headers).some(name => name.startsWith("x-relay-")));
  assert.equal(call.body, "relay request");
  redirect = true;
  const blocked = await relay();
  assert.equal(blocked.status, 502);
  assert.equal(blocked.headers.has("location"), false);
  assert.equal(received.length, 2);
  console.log("Deno relay DNS pin, TLS hostname verification and protected forwarding passed");
} finally {
  if (server) await new Promise(resolve => server.close(resolve));
  await rm(root, { recursive: true, force: true });
}
