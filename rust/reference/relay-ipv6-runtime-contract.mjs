import assert from "node:assert/strict";
import { createSocket } from "node:dgram";
import { setServers } from "node:dns/promises";
import { once } from "node:events";
import { cp, mkdir, readFile } from "node:fs/promises";
import { createServer } from "node:https";
import { syncBuiltinESMExports } from "node:module";
import { dirname, join } from "node:path";
import { createSecureContext } from "node:tls";
import { pathToFileURL } from "node:url";

const folder = process.argv[2];
const ipv4 = "93.184.216.34";
const ipv6 = "2606:4700:ffff::17";
const suffix = ".ipv6-fixture.dev";
const providerKey = "Bearer offline-ipv6-provider";
const answers = new Map();
const queries = [];
const handshakes = [];
const requests = [];

function binaryIp(address, type) {
  if (type === 1) return Buffer.from(address.split(".").map(Number));
  const [upper, lower] = address.split("::");
  const head = upper ? upper.split(":") : [];
  const tail = lower ? lower.split(":") : [];
  const words = lower === undefined ? head : [...head, ...Array(8 - head.length - tail.length).fill("0"), ...tail];
  const buffer = Buffer.alloc(16);
  words.forEach((word, i) => buffer.writeUInt16BE(Number.parseInt(word, 16), i * 2));
  return buffer;
}

function dnsResponse(query) {
  const labels = [];
  let end = 12;
  while (query[end] !== 0) {
    const length = query[end++];
    labels.push(query.toString("ascii", end, end + length));
    end += length;
  }
  end++;
  const name = labels.join(".");
  const type = query.readUInt16BE(end);
  assert.ok(type === 1 || type === 28);
  assert.equal(query.readUInt16BE(end + 2), 1);
  queries.push([name, type]);
  const records = answers.get(name)?.[type] ?? [];
  const rcode = typeof records === "number" ? records : 0;
  const items = Array.isArray(records) ? records : [];
  const header = Buffer.alloc(12);
  query.copy(header, 0, 0, 2);
  header.writeUInt16BE(0x8180 | rcode, 2);
  header.writeUInt16BE(1, 4);
  header.writeUInt16BE(items.length, 6);
  const body = items.map(address => {
    const data = binaryIp(address, type);
    const record = Buffer.alloc(12);
    record.writeUInt16BE(0xc00c, 0);
    record.writeUInt16BE(type, 2);
    record.writeUInt16BE(1, 4);
    record.writeUInt16BE(data.length, 10);
    return Buffer.concat([record, data]);
  });
  return Buffer.concat([header, query.subarray(12, end + 4), ...body]);
}

function requestFor(relayUrl, secret, host, path = "/v1/chat/completions?public=yes") {
  return new Request(relayUrl, {
    method: "POST",
    headers: {
      "x-relay-auth": secret,
      "x-relay-target": "https://" + host,
      "x-relay-path": path,
      "authorization": providerKey,
    },
    body: "isolated relay request",
  });
}

async function deployFixtures() {
  const source = new URL("../app/src/", import.meta.url);
  const files = {
    "deno/main.mjs": "deno_relay_runtime.mjs",
    "deno/relay.mjs": "cloudflare_relay_worker.mjs",
    "deno/pinned-https.mjs": "pinned_relay_https.mjs",
    "vercel/api/relay.mjs": "vercel_relay_function.mjs",
    "vercel/lib/relay.mjs": "cloudflare_relay_worker.mjs",
    "vercel/lib/pinned-https.mjs": "pinned_relay_https.mjs",
    "netlify/relay.cjs": "netlify_relay_function.cjs",
    "netlify/relay.mjs": "cloudflare_relay_worker.mjs",
    "netlify/pinned-https.mjs": "pinned_relay_https.mjs",
  };
  for (const [target, path] of Object.entries(files)) {
    const file = join(folder, target);
    await mkdir(dirname(file), { recursive: true });
    await cp(new URL(path, source), file);
  }
  const deno = await import(pathToFileURL(join(folder, "deno/main.mjs")));
  const vercel = await import(pathToFileURL(join(folder, "vercel/api/relay.mjs")));
  const netlify = await import(pathToFileURL(join(folder, "netlify/relay.cjs")));
  const secret = "D".repeat(43);
  const denoUrl = "https://managed-ipv6.fixture-org.deno.net";
  const netlifyUrl = "https://managed-ipv6.netlify.app/.netlify/functions/relay";
  const vercelUrl = "https://managed-ipv6.vercel.app/api/relay";
  const denoHandler = deno.createHandler({ RELAY_URL: denoUrl, RELAY_AUTH: secret });
  const netlifyHandler = netlify.createHandler({ RELAY_URL: netlifyUrl, RELAY_AUTH: secret });
  const vercelHandler = vercel.createHandler({ VERCEL_ENV: "production",
    VERCEL_PROJECT_PRODUCTION_URL: "managed-ipv6.vercel.app", RELAY_AUTH: secret });
  return {
    deno: (host, path) => denoHandler(requestFor(denoUrl, secret, host, path)),
    vercel: (host, path) => vercelHandler(requestFor(vercelUrl, secret, host, path)),
    netlify: async (host, path) => {
      const req = requestFor(netlifyUrl, secret, host, path);
      const result = await netlifyHandler({ rawUrl: req.url, httpMethod: req.method,
        body: await req.text(), headers: Object.fromEntries(req.headers) });
      const body = result.isBase64Encoded ? Buffer.from(result.body, "base64") : result.body;
      return new Response(body, { status: result.statusCode, headers: result.headers });
    },
  };
}

const resolver = createSocket("udp4");
const servers = [];
try {
  resolver.on("message", (message, peer) => {
    resolver.send(dnsResponse(message), peer.port, peer.address);
  });
  resolver.bind(0, "127.0.0.1");
  await once(resolver, "listening");
  setServers(["127.0.0.1:" + resolver.address().port]);
  syncBuiltinESMExports();
  const cert = await readFile(join(folder, "cert.pem"));
  const key = await readFile(join(folder, "key.pem"));
  const context = createSecureContext({ key, cert });
  for (const address of [ipv4, ipv6]) {
    const server = createServer({ key, cert, SNICallback(name, confirm) {
      handshakes.push([address, name]);
      confirm(null, context);
    }}, async (req, res) => {
      const body = [];
      for await (const chunk of req) body.push(chunk);
      requests.push([address, req.headers.host, req.url]);
      assert.equal(req.socket.localAddress, address);
      assert.equal(req.method, "POST");
      assert.equal(req.headers.host, req.socket.servername);
      assert.equal(req.headers.authorization, providerKey);
      assert.equal(req.headers["x-relay-auth"], undefined);
      assert.equal(Buffer.concat(body).toString(), "isolated relay request");
      if (req.url === "/redirect") return res.writeHead(307, { location: "https://wrong.fixture-alt.dev/next" }).end();
      res.setHeader("content-type", "text/event-stream");
      res.end("data: verified\n\n");
    });
    servers.push(server);
    server.listen(443, address);
    await once(server, "listening");
  }
  const handlers = await deployFixtures();

  async function check(handler, host, status, path) {
    const before = queries.length;
    const response = await handler(host, path);
    assert.equal(response.status, status, JSON.stringify({host, queries: queries.slice(before), handshakes}));
    const types = new Set(queries.slice(before).filter(([name]) => name === host).map(([, type]) => type));
    assert.deepEqual(types, new Set([1, 28]), host);
    return response;
  }

  for (const [label, handler] of Object.entries(handlers)) {
    const host = label + suffix;
    answers.set(host, { 28: [ipv6] });
    const response = await check(handler, host, 200);
    assert.equal(await response.text(), "data: verified\n\n");
    assert.deepEqual(handshakes.at(-1), [ipv6, host]);
    assert.deepEqual(requests.at(-1), [ipv6, host, "/v1/chat/completions?public=yes"]);
  }
  for (const [label, records] of [
    ["a-only", { 1: [ipv4] }],
    ["dual-stack", { 1: [ipv4], 28: [ipv6] }],
  ]) {
    const host = label + suffix;
    answers.set(host, records);
    assert.equal(await (await check(handlers.deno, host, 200)).text(), "data: verified\n\n");
    assert.deepEqual(requests.at(-1), [ipv4, host, "/v1/chat/completions?public=yes"]);
  }

  const unsafe = [
    "::1", "::", "::ffff:7f00:1", "::ffff:5db8:d822", "fc11::1", "fd00::1", "fe80::1",
    "fec0::1", "ff02::1", "64:ff9b::7f00:1", "64:ff9b:1::1", "100::1", "100:0:0:1::1",
    "2001::1", "2001:2::1", "2001:db8::1", "2002:7f00:1::1", "2620:4f:8000::1", "3ffe::1",
    "3fff::1", "5f00::1",
  ];
  const beforeBlocked = requests.length;
  const beforeTls = handshakes.length;
  for (const [index, candidate] of unsafe.entries()) {
    const host = "blocked-" + index + suffix;
    answers.set(host, { 1: [ipv4], 28: [ipv6, candidate] });
    assert.deepEqual(await (await check(handlers.deno, host, 502)).json(), { error: "Relay upstream request failed" });
  }
  for (const [label, records] of [
    ["unsafe-a", { 1: ["127.0.0.1"], 28: [ipv6] }],
    ["unsafe-a-after-good", { 1: [ipv4, "192.168.0.1"], 28: [ipv6] }],
    ["a-nxdomain", { 1: 3, 28: [ipv6] }],
    ["aaaa-failure", { 1: [ipv4], 28: 2 }],
    ["no-records", {}],
  ]) {
    const host = label + suffix;
    answers.set(host, records);
    assert.deepEqual(await (await check(handlers.deno, host, 502)).json(), { error: "Relay upstream request failed" });
  }
  assert.equal(requests.length, beforeBlocked);
  assert.equal(handshakes.length, beforeTls);

  const wrongName = "wrong.fixture-alt.dev";
  answers.set(wrongName, { 28: [ipv6] });
  await check(handlers.vercel, wrongName, 502);
  assert.deepEqual(handshakes.at(-1), [ipv6, wrongName]);
  assert.equal(requests.length, beforeBlocked);

  const redirectName = "redirect" + suffix;
  answers.set(redirectName, { 28: [ipv6] });
  const response = await check(handlers.netlify, redirectName, 502, "/redirect");
  assert.equal(response.headers.get("location"), null);
  assert.deepEqual(await response.json(), { error: "Relay upstream returned a redirect" });
  assert.equal(queries.filter(([name]) => name === wrongName).length, 2);
  assert.equal(requests.length, beforeBlocked + 1);
  console.log("Deno, Netlify and Vercel relay logic sends public IPv6 over pinned TLS; mixed private DNS and redirects are refused.");
} finally {
  await Promise.all(servers.map(server => new Promise(resolve => server.close(resolve))));
  resolver.close();
}
