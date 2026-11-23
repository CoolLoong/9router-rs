import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { createServer } from "node:http";
import { createRequire } from "node:module";
import { setTimeout as pause } from "node:timers/promises";

const endpoint = "https://api.commandcode.ai/alpha/generate";
const fixture = "http://127.0.0.1:52989/commandcode/alpha/generate";
const catalog = "https://api.commandcode.ai/provider/v1/models?view=client";
const catalogFixture = "http://127.0.0.1:52989/commandcode/provider/v1/models?view=client";
const key = "offline-commandcode-account";
const attempts = new Map();
const usage = { inputTokens: 23, outputTokens: 5, totalTokens: 28, inputTokenDetails: { cacheReadTokens: 6, cacheWriteTokens: 3 } };
const explicit = createHash("sha256").update("fixture-command-session").digest("hex");
const explicitId = `${explicit.slice(0, 8)}-${explicit.slice(8, 12)}-4${explicit.slice(13, 16)}-${(parseInt(explicit[16], 16) & 3) | 8}${explicit.slice(17, 20)}-${explicit.slice(20, 32)}`;
const uuid = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;
const testImage = "data:image/png;base64,iVBORw0KGgoAAA==";
let implicitId;

const remap = fetchImpl => (input, options) => {
  const url = input instanceof Request ? input.url : String(input);
  const target = url === endpoint ? fixture : url === catalog ? catalogFixture : null;
  if (!target) return fetchImpl(input, options);
  return fetchImpl(input instanceof Request ? new Request(target, input) : target, options);
};

globalThis.fetch = remap(globalThis.fetch);
const undici = createRequire(new URL("../../.next/standalone/server.js", import.meta.url))("undici");
undici.fetch = remap(undici.fetch);

function error(response, status, error) {
  response.writeHead(status, { "Content-Type":"application/json" }).end(JSON.stringify({ error }));
}

function validate(request, body, label) {
  assert.equal(request.method, "POST");
  assert.equal(request.headers.authorization, `Bearer ${key}`);
  assert.equal(request.headers.accept, "text/event-stream");
  assert.equal(request.headers["content-type"], "application/json");
  assert.equal(request.headers["user-agent"], "cli");
  assert.equal(request.headers["x-command-code-version"], "1.88.5");
  assert.equal(request.headers["x-cli-environment"], "production");
  assert.equal(request.headers["x-cli-surface"], "cli");
  assert.equal(request.headers["x-taste-learning"], "false");
  assert.equal(request.headers["x-project-slug"], process.cwd().toLowerCase().split(/[^a-z0-9]+/).filter(Boolean).join("-"));
  assert.match(body.threadId, uuid);
  assert.equal(body.threadId, request.headers["x-session-id"]);
  assert.equal(body.permissionMode, "standard");
  assert.deepEqual(body.config, {
    workingDir: process.cwd(),
    date: new Date().toISOString().slice(0, 10),
    environment: "linux",
    structure: [],
    isGitRepo: false,
    currentBranch: "",
    mainBranch: "",
    gitStatus: "",
    recentCommits: [],
  });
  assert.equal(body.memory, null);
  assert.equal(body.taste, null);
  assert.equal(body.skills, null);
  assert.equal(body.model, body.params.model);
  assert.equal(body.params.stream, true);
  if (["json", "default", "tools"].includes(label)) assert.equal(body.threadId, explicitId);
  if (["retry-in-band", "retry-http"].includes(label)) {
    implicitId ||= body.threadId;
    assert.equal(body.threadId, implicitId);
  }
  if (label === "default") {
    assert.equal(body.params.max_tokens, 64000);
    assert.equal(body.params.temperature, 0.3);
  }
  if (label === "tools") {
    assert.equal(body.params.model, "claude-haiku-5-5");
    assert.equal(body.params.system, "first rule\nsecond rule\n\nlast rule");
    assert.equal(body.params.temperature, 0.7);
    assert.equal(body.params.top_p, 0.9);
    assert.equal(body.params.max_tokens, 88);
    assert.equal(body.params.reasoning_effort, "high");
    assert.deepEqual(body.params.tools, [{
      name:"probe", description:"fixture utility", input_schema:{type:"object",properties:{q:{type:"string"}}},
    }]);
    assert.deepEqual(body.params.messages, [
      {role:"assistant",content:[
        {type:"reasoning",text:"internal fixture"},
        {type:"text",text:"checking fixture"},
        {type:"tool-call",toolCallId:"history-1",toolName:"probe",input:{q:"fixture"}},
      ]},
      {role:"tool",content:[
        {type:"tool-result",toolCallId:"history-1",toolName:"probe",output:{type:"error-text",value:"previous fixture failed"}},
      ]},
      {role:"user",content:[
        {type:"text",text:"tools"},
        {type:"image",image:testImage,mimeType:"image/png",mediaType:"image/png"},
      ]},
    ]);
  }
}

async function writeStream(response, events, lateError = false) {
  response.writeHead(200, { "Content-Type":"application/x-ndjson", "Cache-Control":"no-cache" });
  if (lateError) {
    response.write(events[0] + "\n");
    await pause(60);
    response.end(events[1]);
    return;
  }
  const raw = Buffer.from(events.join("\n"));
  const index = raw.indexOf(Buffer.from("文"));
  if (index !== -1) {
    response.write(raw.subarray(0, index + 1));
    await pause(10);
    response.end(raw.subarray(index + 1));
  } else {
    response.end(raw);
  }
}

createServer(async (request, response) => {
  if (request.method === "GET" && request.url === "/commandcode/provider/v1/models?view=client") {
    assert.equal(request.headers.authorization, undefined);
    assert.equal(request.headers["user-agent"], "cli");
    assert.equal(request.headers["x-command-code-version"], "1.88.5");
    return response.writeHead(200, { "Content-Type": "application/json" })
      .end(JSON.stringify({ data: [
        { id: "fixture-vision-model", display: { label: "Fixture Vision", description: "Synthetic catalog" },
          context_length: 100000, maxOutputTokens: 9000, inputModalities: ["image"], reasoningEfforts: ["high"] },
        { id: "fixture-basic-model", name: "Fixture Basic" },
        { id: "fixture-hidden-model", hidden: true },
      ] }));
  }
  if (request.method === "GET" && request.url === "/commandcode/fixture-attempts") {
    response.writeHead(200, { "Content-Type":"application/json" }).end(JSON.stringify(Object.fromEntries(attempts)));
    return;
  }
  if (request.url !== "/commandcode/alpha/generate") {
    error(response, 404, {message:"unknown CommandCode fixture route"});
    return;
  }
  try {
    const packets = [];
    for await (const packet of request) packets.push(packet);
    const body = JSON.parse(Buffer.concat(packets));
    const label = body.params.messages.at(-1).content[0].text;
    validate(request, body, label);
    const count = (attempts.get(label) || 0) + 1;
    attempts.set(label, count);
    const encode = JSON.stringify;
    if (label === "retry-http" && count === 1) {
      error(response, 503, {message:"synthetic upstream HTTP unavailable"});
      return;
    }
    if (label === "retry-in-band" && count === 1) {
      await writeStream(response, [
        encode({type:"start"}),
        "fixture invalid NDJSON line",
        encode({type:"error",error:{message:"synthetic upstream overloaded",statusCode:503}}),
      ]);
      return;
    }
    if (label === "early-limit") {
      await writeStream(response, [encode({type:"error",error:"too many requests in fixture"})]);
      return;
    }
    if (label === "http-error") {
      error(response, 418, {message:"synthetic request rejected",code:422});
      return;
    }
    if (label === "secret") {
      error(response, 422, {message:`synthetic auth value: ${key}`});
      return;
    }
    if (label === "mid-stream-error") {
      await writeStream(response, [
        encode({type:"text-delta",text:"fixture before error"}),
        encode({type:"error",error:"synthetic generator failed mid-stream"}),
      ], true);
      return;
    }
    if (label === "oversize") {
      response.writeHead(200, { "Content-Type":"application/x-ndjson" });
      response.write(encode({type:"text-delta",text:"fixture before error"}) + "\n");
      await pause(30);
      response.end(Buffer.alloc(8 * 1024 * 1024 + 1, "x"));
      return;
    }
    if (label === "tools") {
      await writeStream(response, [
        encode({type:"start"}),
        "data: " + encode({type:"reasoning-delta",text:"fixture thought"}),
        encode({type:"tool-input-start",id:"tool-A",toolName:"probe"}),
        encode({type:"tool-input-delta",id:"tool-A",delta:'{"q":'}),
        encode({type:"tool-input-delta",toolCallId:"tool-A",inputTextDelta:'"fixture"}'}),
        encode({type:"tool-input-end",id:"tool-A"}),
        encode({type:"tool-call",toolCallId:"tool-A",toolName:"probe",input:{q:"fixture"}}),
        encode({type:"tool-call",toolCallId:"tool-B",toolName:"probe",input:{q:"second"}}),
        encode({type:"finish-step",finishReason:"tool-calls",usage}),
        encode({type:"finish",finishReason:"stop"}),
      ]);
      return;
    }
    assert.ok(["json", "default", "retry-in-band", "retry-http", "messages-json"].includes(label), label);
    await writeStream(response, [
      encode({type:"start",model:"ignored-server-model"}),
      "fixture invalid NDJSON line",
      encode({type:"text-delta",text:"fixture 文"}),
      "data: " + encode({type:"text-delta",delta:" completed"}),
      encode({type:"finish",finishReason:"stop",totalUsage:usage}),
    ]);
  } catch (failure) {
    process.stderr.write(`CommandCode fixture assertion: ${failure.stack}\n`);
    if (response.headersSent) response.destroy();
    else error(response, 422, {message:"CommandCode fixture assertion failed"});
  }
}).listen(52989, "127.0.0.1");
