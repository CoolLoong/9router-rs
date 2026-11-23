import http2 from "node:http2";
import { appendFileSync } from "node:fs";
import { generateCursorChecksum } from "../../open-sse/utils/cursorChecksum.js";

const destination = "https://agent.api5.cursor.sh";
const local = "http://127.0.0.1:52982";
const resource = "/agent.v1.AgentService/GetUsableModels";
const machines = {
  primary: "01234567-89ab-cdef-0123-456789abcdef",
  secondary: "89abcdef0123456789abcdef0123456789abcdef0123456789abcdef01234567",
  status: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
  empty: "33333333-2222-1111-2222-333333333333",
  malformed: "aaaaaaaa-ffff-0000-ffff-aaaaaaaaaaaa",
  redirect: "11111111-0000-1111-0000-111111111111",
  echo: "abcdefab-cdef-abcd-efab-cdefabcdefab",
};
const token = (name) => `synthetic-cursor-${name}-fixture-only`;

function proofMatches(checksum, machine) {
  const readClock = Date.now;
  const now = readClock();
  try {
    return [now, now - 10_000].some(current => {
      Date.now = () => current;
      return checksum === generateCursorChecksum(machine);
    });
  } finally {
    Date.now = readClock;
  }
}

function varint(n) {
  const out = [];
  while (n > 127) {
    out.push((n & 127) | 128);
    n >>>= 7;
  }
  out.push(n);
  return Buffer.from(out);
}

function field(id, input) {
  if (typeof input === "number") return Buffer.concat([varint(id << 3), varint(input)]);
  const body = Buffer.from(input);
  return Buffer.concat([varint((id << 3) + 2), varint(body.length), body]);
}

function model(...data) {
  return field(1, Buffer.concat(data.map(([id, value]) => field(id, value))));
}

function flags(...data) {
  return field(2, Buffer.concat(data.map(([id, value]) => field(id, value))));
}

function response(name) {
  const rows = {
    primary: [
      model([1, "  fixture-cursor-live  "], [4, " Live Label "], [5, "Short"]),
      model([1, "fixture-cursor-live"], [4, "Duplicate"]),
      model([1, "default"], [5, "Auto (Server Picks)"]),
      model([1, "fixture-cursor-chat"], [3, "Display ID"]),
      model([1, "tool/model-with-slash"], [4, "Slash"]),
      model([1, "fixture-cursor-hidden"], [4, "Hidden"]),
      model([1, "fixture-cursor-whitespace"], [4, "  "], [5, "Ignored"]),
      model([1, "   "], [4, "Skipped blank id"]),
      field(1, 100),
      field(99, "unknown field"),
      flags([1, "default"], [5, 1]),
      flags([1, "fixture-cursor-live"], [2, 1], [5, 1]),
      flags([1, "fixture-cursor-live"], [4, 1]),
      flags([1, "fixture-cursor-chat"], [4, 1], [5, 1]),
      flags([1, "fixture-cursor-hidden"], [35, 1], [5, 1]),
      flags([1, "ignored-flag-without-model"], [5, 1]),
    ],
    secondary: [model([1, "fixture-cursor-second"]), model([1, "fixture-cursor-utf8"], [4, [0xff]])],
    empty: [flags([1, "available-without-model"], [5, 1])],
    malformed: [Buffer.from([10, 127, 10, 3, 112])],
    echo: [model([1, token("echo")]), model([1, "fixture-cursor-clean"], [4, machines.echo])],
  };
  return Buffer.concat(rows[name] || []);
}

if (process.argv[2] === "serve-cursor-models") {
  const requests = process.argv[3];
  const strict = process.argv[4] === "native";
  const app = http2.createServer();
  app.on("stream", (stream, headers) => {
    const name = Object.keys(machines).find(n => headers.authorization === `Bearer ${token(n)}`);
    const checksum = headers["x-cursor-checksum"] || "";
    const valid = name && proofMatches(checksum, machines[name]);
    const forbidden = ["cookie", "x-api-key", "x-9r-cli-token", "connect-accept-encoding",
      "connect-protocol-version", "proxy-authorization"];
    if (strict) forbidden.push("x-client-key", "x-cursor-client-version", "x-cursor-client-commit",
      "x-cursor-client-type", "x-session-id", "x-cursor-config-version");
    const unsafe = forbidden.filter(header => header in headers);
    const ghost = headers["x-ghost-mode"];
    let length = 0;
    stream.on("data", chunk => { length += chunk.length; });
    stream.on("end", () => {
      const admitted = valid && !unsafe.length && !length && headers[":path"] === resource &&
        headers[":method"] === "POST" && headers.accept === "application/proto" &&
        headers["content-type"] === "application/proto" && ghost === (name === "secondary" ? "false" : "true");
      appendFileSync(requests, JSON.stringify({ name, admitted: Boolean(admitted) }) + "\n");
      if (!admitted) {
        stream.respond({":status": 422});
        return stream.end();
      }
      if (name === "status") {
        stream.respond({":status": 401});
        return stream.end("synthetic auth failure");
      }
      if (name === "redirect") {
        stream.respond({":status": 302, location: `${local}/blocked`});
        return stream.end();
      }
      stream.respond({":status": 200, "content-type":"application/proto"});
      stream.end(response(name));
    });
  });
  app.listen(52982, "127.0.0.1");
} else {
  const direct = http2.connect;
  http2.connect = (authority, ...options) => {
    if (String(authority) !== destination) throw new Error("Unexpected HTTP/2 fixture upstream");
    const session = direct(local, ...options);
    const request = session.request.bind(session);
    session.request = (headers, ...settings) => request({
      ...headers, ":scheme":"http", ":authority":"127.0.0.1:52982",
    }, ...settings);
    return session;
  };
}
