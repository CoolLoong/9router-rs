import { encodeEventFrame } from "../../open-sse/utils/awsEventStream.js";
import { escapeUri, signAwsRequest } from "../../open-sse/utils/awsSigv4.js";

const fixture = "http://127.0.0.1:52989";
const claudeModel = "us.anthropic.claude-sonnet-4-5-20250929-v1:0";
const profileArn = "arn:aws:bedrock:us-west-2:123456789000:inference-profile/us.anthropic.claude-sonnet-4-5-20250929-v1:0";
const grokModel = "us.xai.grok-4.6";
const credentials = {
  messages: { accessKeyId: "ASIAFIXTUREBEDROCKKEY", secretAccessKey: "offline-bedrock-secret", sessionToken: "offline-bedrock-session" },
  chat: { accessKeyId: "AKIAFIXTUREGROKKEY", secretAccessKey: "offline-bedrock-grok-secret" },
};
const cases = new Map([
  ["Bedrock Messages JSON fixture", ["messages", profileArn, false]],
  ["Bedrock Chat pivot fixture", ["messages", claudeModel, false]],
  ["Bedrock xAI JSON fixture", ["chat", grokModel, false]],
  ["Bedrock Messages stream fixture", ["messages", claudeModel, true]],
  ["Bedrock xAI stream fixture", ["chat", grokModel, true]],
  ["Bedrock CRC fixture", ["messages", claudeModel, true]],
  ["Bedrock exception fixture", ["chat", grokModel, true]],
  ["Bedrock truncation fixture", ["chat", grokModel, true]],
  ["Bedrock redaction fixture", ["messages", claudeModel, false]],
  ["Bedrock redirect fixture", ["messages", claudeModel, false]],
]);

export function bedrockFixture(url) {
  if (url?.protocol !== "https:" || !/^bedrock-runtime\.[a-z0-9][a-z0-9-]{0,30}\.amazonaws\.com$/.test(url.hostname) ||
      !url.pathname.startsWith("/model/")) return null;
  return fixture + "/bedrock" + url.pathname + url.search;
}

export function isBedrockMock(request) {
  const path = new URL(request.url, fixture).pathname;
  return request.method === "POST" && path.startsWith("/bedrock/model/");
}

function json(response, status, value) {
  return response.writeHead(status, { "content-type": "application/json" }).end(JSON.stringify(value));
}

function signingDate(stamp) {
  const match = /^(\d{4})(\d\d)(\d\d)T(\d\d)(\d\d)(\d\d)Z$/.exec(stamp || "");
  if (!match) return null;
  const instant = new Date(Date.UTC(+match[1], +match[2] - 1, +match[3], +match[4], +match[5], +match[6]));
  return Math.abs(Date.now() - instant.getTime()) <= 30_000 ? instant : null;
}

const usage = { prompt_tokens: 11, completion_tokens: 3, total_tokens: 14 };
const claudeUsage = { input_tokens: 11, output_tokens: 3 };

function message(model, text, output = claudeUsage) {
  return { id: "msg_bedrock_fixture", model, type: "message", role: "assistant",
    content: text ? [{ type: "text", text }] : [], stop_reason: text ? "end_turn" : null, stop_sequence: null, usage: output };
}

function chunk(payload) {
  const bytes = Buffer.from(JSON.stringify(payload)).toString("base64");
  return Buffer.from(encodeEventFrame({ ":message-type": "event", ":event-type": "chunk", ":content-type": "application/json" }, { bytes }));
}

function chatChunk(model, delta, finish, extra = {}) {
  return {
    id: "chatcmpl-bedrock-fixture", created: 1795046400, object: "chat.completion.chunk", model,
    choices: [{ index: 0, delta, finish_reason: finish }], ...extra,
  };
}

export async function handleBedrockMock(request, response) {
  const requested = new URL(request.url, fixture);
  if (requested.pathname === "/bedrock/model/redirect-canary/invoke") {
    return json(response, 418, { error: "signed request incorrectly followed redirect" });
  }
  const fail = message => json(response, 422, { error: { message } });
  let raw = "";
  for await (const bytes of request) raw += bytes;
  let body;
  try { body = JSON.parse(raw); } catch { return fail("unexpected Bedrock fixture body"); }
  const content = body.messages?.[0]?.content;
  const label = Array.isArray(content) ? content[0]?.text : content;
  const selected = cases.get(label);
  if (!selected) return fail("unexpected Bedrock fixture prompt");
  const [wire, model, streaming] = selected;
  const region = wire === "messages" ? "us-west-2" : "us-east-1";
  const host = "bedrock-runtime." + region + ".amazonaws.com";
  const path = "/model/" + escapeUri(model) + "/" + (streaming ? "invoke-with-response-stream" : "invoke");
  const accept = streaming ? "application/vnd.amazon.eventstream" : "application/json";
  const date = signingDate(request.headers["x-amz-date"]);
  const hostValid = [host, "127.0.0.1:52989"].includes(request.headers.host);
  if (requested.pathname !== "/bedrock" + path || requested.search || !date ||
      request.headers["content-type"] !== "application/json" || request.headers.accept !== accept ||
      !hostValid || request.headers["x-amz-security-token"] !== credentials[wire].sessionToken) {
    return fail("unexpected Bedrock fixture URL or headers " + JSON.stringify({
      path: requested.pathname === "/bedrock" + path, query: Boolean(requested.search), date: Boolean(date),
      contentType: request.headers["content-type"] === "application/json", accept: request.headers.accept === accept,
      host: hostValid, session: request.headers["x-amz-security-token"] === credentials[wire].sessionToken,
    }));
  }
  const expected = signAwsRequest({
    method: "POST", url: "https://" + host + path,
    headers: { Accept: accept, "Content-Type": "application/json" }, body: raw, region, service: "bedrock",
    credentials: credentials[wire], date,
  });
  if (expected.Authorization !== request.headers.authorization) return fail("unexpected Bedrock fixture signature");
  if (Object.hasOwn(body, "model") || Object.hasOwn(body, "stream") ||
      (wire === "messages" ? body.anthropic_version !== "bedrock-2023-05-31" : Object.hasOwn(body, "anthropic_version"))) {
    return fail("unexpected Bedrock fixture body version or model");
  }
  if (label === "Bedrock redaction fixture") {
    return fail("echo " + credentials.messages.secretAccessKey + " " + credentials.messages.sessionToken + " " + expected.Authorization);
  }
  if (label === "Bedrock redirect fixture") {
    return response.writeHead(307, { Location: "/bedrock/model/redirect-canary/invoke" }).end();
  }
  if (!streaming) {
    if (wire === "messages") return json(response, 200, message(model, "Bedrock Messages JSON"));
    return json(response, 200, { id: "chatcmpl-bedrock-fixture", created: 1795046400, object: "chat.completion", model,
      choices: [{ index: 0, message: { role: "assistant", content: "Bedrock xAI JSON" }, finish_reason: "stop" }], usage });
  }
  response.writeHead(200, { "content-type": "application/vnd.amazon.eventstream" });
  if (label === "Bedrock exception fixture") {
    return response.end(Buffer.from(encodeEventFrame({
      ":message-type": "exception", ":exception-type": "throttlingException",
    }, { Message: "fixture throttled" })));
  }
  if (label === "Bedrock CRC fixture") {
    const broken = chunk({ type: "message_stop" });
    broken[broken.length - 1] ^= 1;
    return response.end(broken);
  }

  const frames = wire === "messages"
    ? [
      chunk({ type: "message_start", message: message(model, null, { input_tokens: 11 }) }),
      chunk({ type: "content_block_start", index: 0, content_block: { type: "text", text: "" } }),
      chunk({ type: "content_block_delta", index: 0, delta: { type: "text_delta", text: "Bedrock Messages SSE" } }),
      chunk({ type: "content_block_stop", index: 0 }),
      chunk({ type: "message_delta", delta: { stop_reason: "end_turn", stop_sequence: null }, usage: { output_tokens: 3 } }),
      chunk({ type: "message_stop" }),
    ]
    : [
      chunk(chatChunk(model, { role: "assistant" }, null)),
      chunk(chatChunk(model, { content: "Bedrock xAI SSE" }, null)),
      chunk(chatChunk(model, {}, "stop", { usage })),
    ];
  if (label === "Bedrock truncation fixture") frames.pop();
  const metadata = Buffer.from(encodeEventFrame({ ":message-type": "event", ":event-type": "fixture-metadata" }, null));
  const stream = Buffer.concat([frames[0], metadata, ...frames.slice(1)]);
  response.write(stream.subarray(0, 7));
  await new Promise(resolve => setTimeout(resolve, 12));
  response.write(stream.subarray(7, 51));
  return response.end(stream.subarray(51));
}
