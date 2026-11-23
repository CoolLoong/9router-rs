import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";

const origin = "https://opencode.ai";
const fixture = "http://127.0.0.1:52989";
const paths = new Set([
  "/inference/go/openai/v1/chat/completions",
  "/inference/go/anthropic/v1/messages",
  "/inference/go/openai/v1/responses",
  "/zen/v1/chat/completions",
  "/zen/v1/messages",
  "/zen/v1/responses",
]);

export function opencodeChatFixture(url) {
  return url?.origin === origin && paths.has(url.pathname) ? fixture + "/opencode" + url.pathname + url.search : null;
}

export function isOpenCodeChatMock(request) {
  const path = new URL(request.url, fixture).pathname;
  return request.method === "POST" && path.startsWith("/opencode/") && paths.has(path.slice("/opencode".length));
}

function json(response, status, value) {
  return response.writeHead(status, { "content-type": "application/json" }).end(JSON.stringify(value));
}

function mode() {
  try { return readFileSync(resolve(process.env.DATA_DIR || ".", "opencode-chat-implementation"), "utf8").trim(); }
  catch { return "reference"; }
}

const usage = { prompt_tokens: 6, completion_tokens: 2, total_tokens: 8 };
const responsesUsage = { input_tokens: 6, output_tokens: 2, total_tokens: 8 };
const claudeUsage = { input_tokens: 6, output_tokens: 2 };

function eventStream(response) {
  response.writeHead(200, { "content-type": "text/event-stream", "cache-control": "no-cache" });
  return (type, data) => response.write((type ? "event: " + type + "\n" : "") + "data: " + JSON.stringify(data) + "\n\n");
}

function respondChat(response, body, content, stream) {
  const base = { id: "chatcmpl-opencode-fixture", created: 1795046400, model: body.model };
  if (!stream) {
    return json(response, 200, { ...base, object: "chat.completion",
      choices: [{ index: 0, message: { role: "assistant", content }, finish_reason: "stop" }], usage });
  }
  const emit = eventStream(response);
  const chunk = (delta, finish, extra = {}) => emit(null, { ...base, object: "chat.completion.chunk",
    choices: [{ index: 0, delta, finish_reason: finish }], ...extra });
  chunk({ role: "assistant" }, null);
  chunk({ content }, null);
  chunk({}, "stop", { usage });
  return response.end("data: [DONE]\n\n");
}

function respondMessages(response, body, text, stream) {
  const message = { id: "msg_opencode_fixture", model: body.model, type: "message", role: "assistant", stop_sequence: null };
  if (!stream) return json(response, 200, { ...message, content: [{ type: "text", text }], stop_reason: "end_turn", usage: claudeUsage });
  const emit = eventStream(response);
  emit("message_start", { type: "message_start", message: { ...message, content: [], stop_reason: null, usage: { input_tokens: 6 } } });
  emit("content_block_start", { type: "content_block_start", index: 0, content_block: { type: "text", text: "" } });
  emit("content_block_delta", { type: "content_block_delta", index: 0, delta: { type: "text_delta", text } });
  emit("content_block_stop", { type: "content_block_stop", index: 0 });
  emit("message_delta", { type: "message_delta", delta: { stop_reason: "end_turn", stop_sequence: null }, usage: { output_tokens: 2 } });
  emit("message_stop", { type: "message_stop" });
  return response.end();
}

function respondResponses(response, model, text) {
  const item = { id: "msg_opencode_resp", type: "message", role: "assistant",
    content: [{ type: "output_text", text, annotations: [] }] };
  const base = { id: "resp_opencode_fixture", created_at: 1795046400, object: "response", model };
  const emit = eventStream(response);
  let seq = 0;
  const event = (type, data) => emit(type, { type, ...data, sequence_number: ++seq });
  event("response.created", { response: { ...base, status: "in_progress", output: [] } });
  event("response.output_text.delta", { output_index: 0, item_id: item.id, delta: text });
  event("response.output_item.done", { output_index: 0, item });
  event("response.completed", { response: { ...base, status: "completed", output: [item], usage: responsesUsage } });
  return response.end("data: [DONE]\n\n");
}

const cases = new Map([
  ["Go Chat fixture", { lane: "go", wire: "chat", model: "kimi-k3.1-pro", stream: false, text: "Go Chat" }],
  ["Go Messages fixture", { lane: "go", wire: "messages", model: "minimax-m3", stream: false, text: "Go Messages" }],
  ["Go Responses fixture", { lane: "go", wire: "responses", model: "grok-4.7", stream: true, text: "Go Responses" }],
  ["Go Responses pivot fixture", { lane: "go", wire: "responses", model: "grok-4.7", stream: true, text: "Go Responses pivot" }],
  ["Zen Chat fixture", { lane: "zen", wire: "chat", model: "gemini-3.6-flash", stream: true, text: "Zen Chat" }],
  ["Zen Messages fixture", { lane: "zen", wire: "messages", model: "claude-opus-4-6", stream: true, text: "Zen Messages" }],
  ["Zen Responses fixture", { lane: "zen", wire: "responses", model: "gpt-5.6-sol", stream: true, text: "Zen Responses" }],
]);

export async function handleOpenCodeChatMock(request, response) {
  const path = new URL(request.url, fixture).pathname;
  const lane = path.startsWith("/opencode/zen/") ? "zen" : "go";
  const suffix = path.split("/").at(-1);
  const wire = suffix === "completions" ? "chat" : suffix;
  const key = "offline-opencode-chat-" + lane;
  const fail = message => json(response, 422, { error: { message } });
  if (request.headers["content-type"] !== "application/json") return fail("unexpected fixture Content-Type");
  if (wire === "messages") {
    if (request.headers["x-api-key"] !== key || request.headers["anthropic-version"] !== "2023-06-01" ||
        request.headers.authorization) return fail("unexpected fixture Messages credentials");
  } else if (request.headers.authorization !== "Bearer " + key || request.headers["x-api-key"]) {
    return fail("unexpected fixture Bearer credentials");
  }
  let raw = "";
  for await (const chunk of request) raw += chunk;
  let body;
  try { body = JSON.parse(raw); } catch { return fail("expected fixture JSON"); }
  const content = wire === "responses" ? body.input?.[0]?.content : body.messages?.[0]?.content;
  const label = Array.isArray(content) ? content[0]?.text : (content ?? (typeof body.input === "string" ? body.input : null));
  const check = cases.get(label);
  if (!check || [check.lane, check.wire, check.model, check.stream].join() !== [lane, wire, body.model, body.stream].join()) {
    return fail("unexpected fixture model, wire or flags: " + [label, lane, wire, body.model, body.stream].join(","));
  }
  if (lane === "go") {
    const provided = request.headers["x-opencode-session"];
    const hash = createHash("sha256").update("opencode-go\0generic\0fixture-client-session").digest("hex").slice(0, 32);
    const expected = label === "Go Chat fixture" ? "ses_" + hash : "fixture-go-session";
    if (provided !== expected) return fail("unexpected fixture Go session");
  }
  if (lane === "zen" && mode() === "rust" &&
      (body.tools?.length || request.headers["user-agent"]?.startsWith("opencode/") || request.headers["x-opencode-request"])) {
    return fail("native paid request must carry only its own tools and session identity");
  }
  if (lane === "go" && wire === "responses" &&
      (body.store !== false || body.max_output_tokens !== 42 || "max_tokens" in body || !Array.isArray(body.input))) {
    return fail("unexpected fixture Responses normalization");
  }
  if (wire === "chat") return respondChat(response, body, check.text, check.stream);
  if (wire === "messages") return respondMessages(response, body, check.text, check.stream);
  return respondResponses(response, body.model, check.text);
}
