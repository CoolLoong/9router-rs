import { createServer } from "node:http";
import { createHmac } from "node:crypto";
import { createRequire } from "node:module";
import { Resolver } from "node:dns";
import { handleEmbeddingMock, isEmbeddingMock } from "./mock-embeddings.mjs";
import { handleGeminiChatMock, isGeminiChatMock } from "./mock-gemini-chat.mjs";
import { handleSpeechMock, isSpeechMock } from "./mock-speech.mjs";
import { handleTranscriptionMock, isTranscriptionMock } from "./mock-transcriptions.mjs";
import { staticChatFixture, handleStaticChat, isStaticChat } from "./mock-static-chat.mjs";
import { imageGenerationFixture, handleImageGenerationMock, isImageGenerationMock } from "./mock-image-generation.mjs";
import "./mock-proxy-transport.mjs";
import "./mock-gemini-live-stt.mjs";
import { videoFixture, isVideoMock, handleVideoMock } from "./mock-videos.mjs";
import { webGatewayFixture, isWebGatewayMock, handleWebGatewayMock } from "./mock-web-gateway.mjs";
import { isCodexMock, handleCodexMock } from "./mock-codex.mjs";
import { systemOneFixture, isSystemOneMock, handleSystemOneMock } from "./mock-systemone.mjs";
import { isGeminiCliMock, handleGeminiCliMock } from "./mock-gemini-cli.mjs";
import { isGeminiNativeTtsMock, handleGeminiNativeTtsMock } from "./mock-gemini-native-tts.mjs";
import { githubFixture, isGithubMock, handleGithubMock } from "./mock-github.mjs";
import { voiceFixture, isVoiceMock, handleVoiceMock } from "./mock-voices.mjs";
import { vertexFixture, isVertexMock, handleVertexMock } from "./mock-vertex.mjs";
import { isOllamaChatMock, handleOllamaChatMock } from "./mock-ollama-chat.mjs";
import { opencodeChatFixture, isOpenCodeChatMock, handleOpenCodeChatMock } from "./mock-opencode-chat.mjs";
import { clineFixture, isClineMock, handleClineMock } from "./mock-cline.mjs";
import { bedrockFixture, isBedrockMock, handleBedrockMock } from "./mock-bedrock.mjs";
import { isVertexVideoMock, handleVertexVideoMock } from "./mock-vertex-video.mjs";
import { catalogSyncFixture, isCatalogSyncMock, handleCatalogSyncMock } from "./mock-catalog-sync.mjs";
import { coworkRegistryFixture, isCoworkDiscoveryMock, handleCoworkDiscoveryMock } from "./mock-cowork-discovery.mjs";
import { isKiroModelsMock, handleKiroModelsMock } from "./mock-kiro-models.mjs";
import { isKiroChatMock, handleKiroChatMock } from "./mock-kiro-chat.mjs";
import { localImageFixture, isLocalImageMock, handleLocalImageMock } from "./mock-image-local.mjs";
import { speechProviderFixture, isSpeechProviderMock, handleSpeechProviderMock } from "./mock-speech-providers.mjs";
import { verifyChatVision, verifyResponsesVision } from "./mock-chat-vision.mjs";
import { nousKiloFixture, isNousKiloMock, handleNousKiloMock } from "./mock-nous-kilo.mjs";
import { oauthProbeFixture, isOAuthProbeMock, handleOAuthProbeMock } from "./mock-oauth-probes.mjs";
import { kimiChatFixture, isKimiChatMock, handleKimiChatMock } from "./mock-kimi-chat.mjs";
import { museChatFixture, isMuseChatMock, handleMuseChatMock } from "./mock-muse-chat.mjs";
import { xiaomiChatFixture, isXiaomiChatMock, handleXiaomiChatMock } from "./mock-xiaomi-chat.mjs";
import { CAVEMAN_PROMPTS } from "../../open-sse/rtk/cavemanPrompts.js";
import { PONYTAIL_PROMPTS } from "../../open-sse/rtk/ponytailPrompt.js";

const customPromptFixture = "  Fixture custom system instruction  ";
const injectedFixture = [CAVEMAN_PROMPTS.lite, PONYTAIL_PROMPTS.lite, customPromptFixture];
const rawRtkText = "rtk repeated auth-free line\n".repeat(40);
const compactRtkText = "rtk repeated auth-free line\n\n  ... (39 duplicate lines)";
const headroomRaw = "Fixture headroom uncompressed text with useful details";
const headroomCompact = "Fixture headroom compact";
const resolve4 = Resolver.prototype.resolve4;
Resolver.prototype.resolve4 = function(host, ...options) {
  if (host !== "cloudcode-pa.googleapis.com") return resolve4.call(this, host, ...options);
  const callback = options.at(-1);
  queueMicrotask(() => callback(new Error("isolated Cloud Code Assist fixture")));
};

const routeGeminiFixture = fetchUpstream => (input, init) => {
  const originalUrl = input instanceof Request ? input.url : input;
  const url = URL.canParse(originalUrl) ? new URL(originalUrl) : null;
  const accountFixture = nousKiloFixture(url) || oauthProbeFixture(url) || kimiChatFixture(url) || museChatFixture(url) || xiaomiChatFixture(url);
  if (accountFixture) return fetchUpstream(input instanceof Request ? new Request(accountFixture, input) : accountFixture, init);
  const cowork = coworkRegistryFixture(url);
  if (cowork) return fetchUpstream(input instanceof Request ? new Request(cowork, input) : cowork, init);
  if (url?.origin === "https://api.kilo.ai" && url.pathname === "/api/gateway/models") {
    const catalog = `http://127.0.0.1:52989/catalog-suggestions/${url.host}${url.pathname}`;
    return fetchUpstream(input instanceof Request ? new Request(catalog, input) : catalog, init);
  }
  if (url?.origin === "https://cloud.zed.dev" && url.pathname === "/client/users/me") {
    const identity = "http://127.0.0.1:52989/zed-fixture/client/users/me";
    return fetchUpstream(input instanceof Request ? new Request(identity, input) : identity, init);
  }
  if (url?.origin === "https://platform.iflow.cn" && url.pathname === "/api/openapi/apikey") {
    const authorization = "http://127.0.0.1:52989/iflow-fixture/api/openapi/apikey";
    return fetchUpstream(input instanceof Request ? new Request(authorization, input) : authorization, init);
  }
  if (url?.origin === "https://apis.iflow.cn" && url.pathname === "/v1/chat/completions") {
    const inference = "http://127.0.0.1:52989/iflow-fixture/v1/chat/completions";
    return fetchUpstream(input instanceof Request ? new Request(inference, input) : inference, init);
  }
  const provider = url?.origin === "https://generativelanguage.googleapis.com" && url.pathname.startsWith("/v1beta/models/")
    ? "gemini"
    : url?.origin === "https://api.openai.com" && url.pathname === "/v1/audio/speech" ? "openai"
    : url?.origin === "https://api.anthropic.com" && url.pathname === "/v1/messages" ? "anthropic"
    : url?.origin === "https://cloudcode-pa.googleapis.com" && url.pathname.startsWith("/v1internal:") ? "gemini-cli"
    : url?.origin === "https://chatgpt.com" && ["/backend-api/codex/responses", "/backend-api/codex/responses/compact"].includes(url.pathname) ? "codex" : null;
  if (provider) {
    const fixture = `http://127.0.0.1:52989/${provider}${url.pathname}${url.search}`;
    return fetchUpstream(input instanceof Request ? new Request(fixture, input) : fixture, init);
  }
  const transcribe = url?.origin === "https://api.elevenlabs.io" && url.pathname === "/v1/speech-to-text"
    ? "elevenlabs" : url?.origin === "https://api.assemblyai.com" && url.pathname.startsWith("/v2/") ? "assemblyai" : null;
  if (transcribe) {
    const fixture = `http://127.0.0.1:52989/${transcribe}${url.pathname}${url.search}`;
    return fetchUpstream(input instanceof Request ? new Request(fixture, input) : fixture, init);
  }
  const localImage = url && localImageFixture(url);
  if (localImage) {
    return fetchUpstream(input instanceof Request ? new Request(localImage, input) : localImage, init);
  }
  const imageGeneration = imageGenerationFixture(url);
  if (imageGeneration && init?.body?.[Symbol.toStringTag] === "FormData") {
    const form = new FormData();
    for (const [name, value] of init.body.entries()) form.append(name, value);
    const prepared = new Request(imageGeneration, { ...init, body: form });
    return prepared.arrayBuffer().then(bytes => fetchUpstream(imageGeneration, {
      ...init, headers: prepared.headers, body: new Uint8Array(bytes),
    }));
  }
  const staticChat = url && (staticChatFixture(url) || imageGeneration || videoFixture(url) || webGatewayFixture(url) || systemOneFixture(url) || voiceFixture(url) || githubFixture(url) || vertexFixture(url) || opencodeChatFixture(url) || clineFixture(url) || bedrockFixture(url) || speechProviderFixture(url) || catalogSyncFixture(url));
  if (staticChat) {
    return fetchUpstream(input instanceof Request ? new Request(staticChat, input) : staticChat, init);
  }
  return fetchUpstream(input, init);
};
globalThis.fetch = routeGeminiFixture(globalThis.fetch);
const standalone = createRequire(new URL("../../.next/standalone/server.js", import.meta.url));
const undici = standalone("undici");
undici.fetch = routeGeminiFixture(undici.fetch);

const models = [
  { id: "fixture-live/gpt-5.6-sol" },
  { name: "vendor/model" },
  { model: "mock-embed" },
  { id: "mock-image" },
  { id: "fixture-live/gpt-5.6-sol" },
  { id: "" },
];

const usage = { prompt_tokens: 4, completion_tokens: 2, total_tokens: 6 };

async function chat(request, response) {
  const account = request.headers.authorization === "Bearer offline-models-provider" ? "primary"
    : request.headers.authorization === "Bearer offline-backup-provider" ? "backup" : null;
  if (!account) {
    response.writeHead(401, { "Content-Type": "application/json" }).end('{"error":{"message":"invalid fixture provider key"}}');
    return;
  }

  let buffer = "";
  for await (const bytes of request) buffer += bytes;
  let body;
  try { body = JSON.parse(buffer); } catch {
    response.writeHead(400, { "Content-Type": "application/json" }).end('{"error":{"message":"invalid chat JSON"}}');
    return;
  }
  if (!verifyChatVision(body)) {
    response.writeHead(422, { "Content-Type": "application/json" }).end('{"error":{"message":"unexpected fixture chat vision blocks"}}');
    return;
  }
  if (body.model === "fixture-rate-limit" || (body.model === "fixture-rotate" && account === "primary")) {
    response.writeHead(429, { "Content-Type": "application/json", "Retry-After": "1" })
      .end('{"error":{"message":"fixture upstream is rate limited","type":"rate_limit_exceeded","code":"rate_limit_exceeded"}}');
    return;
  }
  if (!["gpt-5.6-sol", "fixture-rotate", "fixture-round", "fixture-tools", "fixture-redirect", "fixture-sse-deferred", "fixture-sse-abort", "fixture-sse-cached",
      "fixture-combo-empty", "fixture-combo-filter", "fixture-combo-a", "fixture-combo-b",
      "fixture-proxied", "fixture-relayed", "fixture-proxy-strict", "fixture-observe"].includes(body.model) || !Array.isArray(body.messages) || !body.messages.length) {
    response.writeHead(400, { "Content-Type": "application/json" }).end('{"error":{"message":"unexpected fixture chat request"}}');
    return;
  }
  if (body.model === "fixture-redirect" && request.url === "/v1/chat/completions") {
    response.writeHead(307, { Location: "/v1/chat/completions/redirect-target" }).end();
    return;
  }
  const last = body.messages.at(-1)?.content;
  if (Array.isArray(last) && last.some(part => part.text === "offline fixture capacity request")) {
    const hasImage = last.some(part => part.type === "image_url");
    const placeholder = last.some(part => part.text === "[image omitted: model has no vision support]");
    if ((body.model === "gpt-5.6-sol" && (!hasImage || placeholder || body.messages.length !== 8 ||
          body.messages.some(message => typeof message.content === "string" && message.content.includes("capacity discard")))) ||
        (body.model === "fixture-combo-a" && (hasImage || !placeholder || body.messages.length !== 10))) {
      response.writeHead(422, { "Content-Type": "application/json" }).end('{"error":{"message":"unexpected capacity adapter request"}}');
      return;
    }
  }
  if (last === "fixture fusion panel" &&
    (body.stream !== false || Object.hasOwn(body, "tools") || Object.hasOwn(body, "stream_options") ||
      !body.messages.some(message => message.content?.includes?.("[Tool result: fixture text result]")))) {
    response.writeHead(422, { "Content-Type": "application/json" }).end('{"error":{"message":"unexpected fusion panel request"}}');
    return;
  }
  if (typeof last === "string" && last.startsWith("You are the JUDGE in a model-fusion panel.") &&
    (body.model !== "gpt-5.6-sol" || body.stream !== true || !body.tools?.length ||
      !last.includes("[Source 1]\nfixture-combo-a") || !last.includes("[Source 2]\nfixture-combo-b"))) {
    response.writeHead(422, { "Content-Type": "application/json" }).end('{"error":{"message":"unexpected fusion judge request"}}');
    return;
  }
  if (last === "fixture compatible thinking" &&
      (body.model !== "gpt-5.6-sol" || body.reasoning_effort !== "high" || "thinking" in body)) {
    response.writeHead(422, { "Content-Type": "application/json" })
      .end(JSON.stringify({ error: { message: "unexpected compatible thinking format" } }));
    return;
  }
  if (last === "fixture stripped thinking" && ["thinking", "reasoning_effort", "reasoning"].some(key => key in body)) {
    response.writeHead(422, { "Content-Type": "application/json" })
      .end(JSON.stringify({ error: { message: "unexpected thinking for a model without reasoning" } }));
    return;
  }
  if (typeof last === "string" && last.startsWith("Headroom Chat fixture")) {
    const expected = last.endsWith("skip") ? headroomRaw : headroomCompact;
    if (body.messages.find(message => message.role === "tool")?.content !== expected) {
      response.writeHead(422, { "Content-Type": "application/json" })
        .end(JSON.stringify({ error: { message: "unexpected Headroom Chat output" } }));
      return;
    }
  }
  if (typeof last === "string" && last.startsWith("RTK Chat fixture")) {
    const expected = last.endsWith("off") ? rawRtkText : compactRtkText;
    if (body.messages.find(message => message.role === "tool")?.content !== expected) {
      response.writeHead(422, { "Content-Type": "application/json" })
        .end(JSON.stringify({ error: { message: "unexpected Chat tool compression" } }));
      return;
    }
  }
  if (typeof last === "string" && last.startsWith("system prompt fixture")) {
    const system = body.messages.find(message => message.role === "system")?.content;
    const expected = last.endsWith("disabled") ? [customPromptFixture] : injectedFixture;
    const prefix = ["fixture initial", ...expected].join("\n\n");
    const valid = typeof system === "string" && (
      last.endsWith("schema") ? system.startsWith(prefix + "\n\nYou must respond with valid JSON") &&
        body.response_format?.type === "json_object" : system === prefix
    );
    if (!valid) {
      response.writeHead(422, { "Content-Type": "application/json" })
        .end(JSON.stringify({ error: { message: "unexpected injected Chat system or schema" } }));
      return;
    }
  }
  if (body.model === "fixture-tools" && (
    body.tools?.[0]?.function?.name !== "fixture_lookup" ||
    !body.messages.some((message) => message.role === "tool" && message.content === "fixture value")
  )) {
    response.writeHead(400, { "Content-Type": "application/json" }).end('{"error":{"message":"unexpected fixture tool translation"}}');
    return;
  }
  const base = { id: "chatcmpl-migration-fixture", created: 1784419200, model: body.model };
  if (body.stream) {
    response.writeHead(200, { "Content-Type": "text/event-stream", "Cache-Control": "no-cache" });
    const chunk = (frame) => response.write(`data: ${JSON.stringify({ ...base, object: "chat.completion.chunk", ...frame })}\n\n`);
    const choice = (delta, finish_reason = null) => ({ choices: [{ index: 0, delta, finish_reason }] });
    chunk(choice({ role: "assistant" }));

    if (body.model === "fixture-observe") {
      await new Promise(resolve => setTimeout(resolve, 400));
    }
    if (body.model === "fixture-combo-empty" || body.model === "fixture-combo-filter") {
      chunk({ ...choice({}, body.model === "fixture-combo-empty" ? "stop" : "content_filter"), usage });
      response.end("data: [DONE]\n\n");
      return;
    }
    if (body.model === "fixture-tools") {
      chunk(choice({ reasoning_content: "fixture thinks" }));
      chunk(choice({ tool_calls: [{ index: 0, id: "call_fixture_lookup", type: "function",
        function: { name: "fixture_lookup", arguments: '{"term":"' } }] }));
      chunk(choice({ tool_calls: [{ index: 0, function: { arguments: 'rust"}' } }] }));
      chunk(choice({}, "tool_calls"));
    } else {
      chunk(choice({ content: "rust " }));
      if (body.model === "fixture-sse-abort") {
        await new Promise((resolve) => setTimeout(resolve, 30));
        response.destroy();
        return;
      }
      chunk(choice({ content: "migration" }));
      const finish = choice({}, "stop");
      if (body.model === "fixture-sse-cached" || body.model === "fixture-observe") {
        finish.usage = body.model === "fixture-observe" ? usage
          : { prompt_tokens: 6, completion_tokens: 3, total_tokens: 9,
            prompt_tokens_details: { cached_tokens: 2, cache_creation_tokens: 1 } };
        chunk(finish);
        response.end("data: [DONE]\n\n");
        return;
      }
      chunk(finish);
      if (body.model === "fixture-sse-deferred") {
        await new Promise((resolve) => setTimeout(resolve, 3600));
        response.end();
        return;
      }
    }

    chunk({ choices: [], usage });
    response.end("data: [DONE]\n\n");
    return;
  }

  response.writeHead(200, { "Content-Type": "application/json" }).end(JSON.stringify({
    ...base,
    object: "chat.completion",
    choices: body.model === "fixture-combo-empty" ? [] : [{
      index: 0,
      ...(body.model === "fixture-tools" ? {
        message: {
          role: "assistant", content: null, reasoning_content: "fixture thinks",
          tool_calls: [{ id: "call_fixture_lookup", type: "function",
            function: { name: "fixture_lookup", arguments: '{"term":"rust"}' } }],
        },
        finish_reason: "tool_calls",
      } : {
        message: { role: "assistant", content: body.model === "gpt-5.6-sol" ? "rust migration"
          : body.model.startsWith("fixture-combo-") ? body.model : account },
        finish_reason: "stop",
      }),
    }],
    usage,
  }));
}

async function responses(request, response) {
  const fail = (status, message) => response.writeHead(status, { "Content-Type": "application/json" })
    .end(JSON.stringify({ error: { message } }));
  if (request.headers.authorization !== "Bearer offline-responses-provider") {
    fail(401, "invalid Responses fixture provider key");
    return;
  }
  let raw = "";
  for await (const bytes of request) raw += bytes;
  let body;
  try { body = JSON.parse(raw); } catch {
    fail(400, "invalid Responses fixture JSON");
    return;
  }
  let label = body.input;
  if (Array.isArray(body.input)) {
    label = body.input[0]?.content?.[0]?.text;
    if (body.input[0]?.type !== "message" || body.input[0]?.role !== "user" ||
      body.input[0]?.content?.[0]?.type !== "input_text" ||
      body.stream !== true || body.store !== false) {
      fail(400, "unexpected Chat-to-Responses payload");
      return;
    }
  }
  if (!["fixture-response-model", "fixture-response-prompt", "fixture-response-rtk", "fixture-response-headroom"].includes(body.model) &&
      !(body.model === "gpt-4o-mini" && typeof label === "string" && label.startsWith("fixture vision "))) {
    fail(400, "unexpected Responses fixture model");
    return;
  }
  if (!verifyResponsesVision(body)) { fail(422, "unexpected fixture Responses vision blocks"); return; }
  if (label === "direct persisted Responses" &&
      (body.previous_response_id !== "resp_previous" || body.include?.[0] !== "reasoning.encrypted_content" ||
       body.store !== false || body.stream !== false)) {
    fail(400, "Responses continuity fields were not preserved");
    return;
  }
  if (label === "fixture Responses prompt" && body.instructions !== ["fixture initial", ...injectedFixture].join("\n\n")) {
    fail(422, "unexpected injected Responses instructions");
    return;
  }
  if (label === "fixture Responses RTK" && body.input.find(item => item.type === "function_call_output")?.output !== compactRtkText) {
    fail(422, "unexpected Responses tool compression");
    return;
  }
  if (label === "fixture Responses Headroom" && body.input.find(item => item.type === "function_call_output")?.output !== headroomCompact) {
    fail(422, "unexpected Responses Headroom output");
    return;
  }
  if (label === "direct native SSE" &&
      (body.stream !== true || request.headers.accept !== "text/event-stream")) {
    fail(400, "Responses stream flags disagreed");
    return;
  }
  if (label === "client JSON from Chat" && request.headers.accept === "text/event-stream") {
    fail(400, "executor should not send SSE Accept for explicitly nonstreaming Chat");
    return;
  }

  if (label === "direct default response" &&
      (Object.hasOwn(body, "stream") || request.headers.accept !== "text/event-stream")) {
    fail(400, "default Responses executor stream should not mutate the wire flag");
    return;
  }
  const item = { id: "msg_responses_fixture", type: "message", role: "assistant",
    content: [{ type: "output_text", text: "native responses migration", annotations: [] }] };
  const realUsage = { input_tokens: 5, output_tokens: 3, total_tokens: 8, input_tokens_details: { cached_tokens: 2 } };
  const base = { id: "resp_native_fixture", created_at: 1784419200, object: "response", model: body.model };
  const done = { ...base, status: "completed", output: [item], usage: realUsage };
  if (label === "direct malformed JSON") {
    response.writeHead(200, { "Content-Type": "application/json" }).end('{"not-complete":');
    return;
  }
  if (label === "direct genuine JSON") {
    response.writeHead(200, { "Content-Type": "application/json" }).end(JSON.stringify(done));
    return;
  }

  response.writeHead(200, { "Content-Type": "text/event-stream", "Cache-Control": "no-cache" });
  let seq = 0;
  const event = (type, data) =>
    response.write(`event: ${type}\ndata: ${JSON.stringify({ type, ...data, sequence_number: ++seq })}\n\n`);
  event("response.created", { response: { ...base, status: "in_progress", output: [] } });
  if (label === "stream Responses tool pivot") {
    if (body.tools?.[0]?.name !== "fixture_lookup") {
      response.destroy();
      return;
    }
    const id = "item_fixture_pivot";
    const tool = { id, type: "function_call", call_id: "call_fixture_pivot", name: "fixture_lookup", arguments: "" };
    event("response.reasoning_summary_text.delta", { delta: "fixture pivot thinks" });
    event("response.output_item.added", { output_index: 0, item: tool });
    event("response.function_call_arguments.delta", { item_id: id, delta: '{"term":"' });
    event("response.function_call_arguments.delta", { item_id: id, delta: 'rust"}' });
    event("response.output_item.done", { output_index: 0, item: { ...tool, arguments: '{"term":"rust"}' } });
    event("response.completed", { response: { ...done, output: [tool] } });
    response.end("data: [DONE]\n\n");
    return;
  }
  event("response.output_text.delta", { output_index: 0, item_id: item.id, delta: "native responses migration" });
  if (label === "direct Responses incomplete") {
    response.end();
    return;
  }
  event("response.output_item.done", { output_index: 0, item });
  event("response.completed", { response: done });
  response.end("data: [DONE]\n\n");
}

async function messages(request, response) {
  const fail = (status, message) => response.writeHead(status, { "Content-Type": "application/json" })
    .end(JSON.stringify({ error: { message } }));
  const beta = request.headers["anthropic-beta"]?.split(",") ?? [];
  const official = request.url === "/anthropic/v1/messages";
  const credential = official ? "offline-official-anthropic" : "offline-anthropic-provider";
  if (request.headers["x-api-key"] !== credential
      || (official ? !!request.headers.authorization : request.headers.authorization !== `Bearer ${credential}`)
      || request.headers["anthropic-version"] !== "2023-06-01"
      || !beta.includes("interleaved-thinking-2025-05-14")
      || beta.includes("claude-code-20250219") !== official) {
    fail(401, "incorrect Anthropic fixture credentials or beta");
    return;
  }
  let raw = "";
  for await (const bytes of request) raw += bytes;
  let body;
  try { body = JSON.parse(raw); } catch {
    fail(400, "invalid Anthropic fixture JSON");
    return;
  }
  if (!["claude-migration-fixture","claude-sonnet-4-20250514","claude-haiku-4-5","claude-sonnet-5-5","claude-opus-5-5"].includes(body.model) ||
      body.messages?.[0]?.role !== "user") {
    fail(400, "unexpected Anthropic fixture model or message");
    return;
  }
  if (official && (Object.hasOwn(body,"context_management") || beta.includes("oauth-2025-04-20"))) {
    fail(400, "unexpected official Anthropic metadata");
    return;
  }
  const content = body.messages[0].content;
  const label = Array.isArray(content) ? content[0]?.text : content;
  if (label === "Anthropic native CLI cache" &&
      (body.system?.[0]?.cache_control?.ttl !== "1h" || content?.[0]?.cache_control?.type !== "ephemeral")) {
    return fail(422, "unexpected Claude Code cache anchors");
  }
  if (label === "Anthropic native CLI fold" &&
      (content?.[1]?.text !== "fixture mid-conversation system" ||
       body.messages.some(message => message.role === "system") ||
       body.messages[1]?.content?.some(block => block.type === "thinking") ||
       body.messages[1]?.content?.[0]?.cache_control?.type !== "ephemeral" ||
       body.messages.at(-1)?.content?.[0]?.text !== "Continue.")) {
    return fail(422, "unexpected Claude Code normalization");
  }
  if (label === "Anthropic native CLI Haiku" &&
      (body.thinking?.type !== "enabled" || body.thinking?.budget_tokens !== 10000 ||
       body.output_config?.effort || body.output_config?.fixture_keep !== true)) {
    return fail(422, "unexpected Claude Code Haiku thinking profile");
  }
  if (label === "Anthropic inline vision" && (
      content?.[1]?.type !== "image" || content[1].source?.type !== "base64" ||
      content[1].source?.media_type !== "image/png" || content[1].source?.data !== "aW1hZ2Utc3ludGhldGlj")) {
    return fail(422, "unexpected Anthropic inline image");
  }
  if (label === "Anthropic thinking budget" &&
      (body.thinking?.type !== "enabled" || body.thinking?.budget_tokens !== 8192 ||
       body.thinking?.display !== "summarized" || body.max_tokens !== 9216 ||
       beta.includes("redact-thinking-2026-02-12"))) {
    return fail(422, "incorrect Claude budget, output ceiling, or beta");
  }
  if (label === "Anthropic thinking off" && body.thinking?.type !== "between_tools") {
    return fail(422, "expected Sonnet between_tools mode");
  }
  if (label === "Anthropic adaptive thinking" &&
      (body.thinking?.type !== "adaptive" || body.thinking?.display !== "summarized" || body.output_config?.effort !== "high")) {
    return fail(422, "expected Claude adaptive thinking");
  }
  if (label === "Anthropic thinking history") {
    const blocks = body.messages.find(message => message.role === "assistant")?.content;
    const thought = blocks?.find(block => block.type === "thinking");
    const tool = blocks?.find(block => block.type === "tool_use");
    if (!thought?.signature || thought.signature === "fixture-original-signature" || thought.cache_control ||
        thought.thinking !== "fixture-visible-thought" || !tool?.cache_control) {
      return fail(422, "incorrect Claude compatibility history");
    }
  }
  if (label === "Anthropic system prompt fixture") {
    const blocks = body.system;
    if (!Array.isArray(blocks) || JSON.stringify(blocks.map(block => block.text)) !==
      JSON.stringify([...injectedFixture, "fixture initial"]) || !blocks.at(-1)?.cache_control) {
      return fail(422, "unexpected Claude system prompt cache placement");
    }
  }
  if (label === "Anthropic RTK fixture") {
    const result = body.messages.at(-1)?.content?.find?.(item => item.type === "tool_result");
    if (result?.content !== compactRtkText) return fail(422, "unexpected Anthropic tool compression");
  }
  if (label === "Anthropic Headroom fixture") {
    const result = body.messages.at(-1)?.content?.find?.(item => item.type === "tool_result");
    if (result?.content !== headroomCompact) return fail(422, "unexpected Anthropic Headroom output");
  }
  if (!label?.includes("Anthropic") || (label === "Anthropic beta fixture" && !beta.includes("fixture-flag"))) {
    fail(400, "unexpected Anthropic content or missing client beta");
    return;
  }
  const tool = label === "Anthropic tool stream";
  if (tool && body.tools?.[0]?.name !== "fixture_lookup") {
    fail(400, "missing converted Anthropic tool declaration");
    return;
  }
  const usage = { input_tokens: 6, output_tokens: 3, cache_read_input_tokens: 2, cache_creation_input_tokens: 1 };
  const start = { id: "msg_migration_fixture", type: "message", role: "assistant", model: body.model };
  if (!body.stream) {
    response.writeHead(200, { "Content-Type": "application/json" }).end(JSON.stringify({
      ...start, content: [{ type: "text", text: "native Anthropic migration" }],
      stop_reason: "end_turn", stop_sequence: null, usage,
    }));
    return;
  }
  if (request.headers.accept !== "text/event-stream") { fail(400, "missing Anthropic SSE Accept"); return; }
  response.writeHead(200, { "Content-Type": "text/event-stream", "Cache-Control": "no-cache" });
  const event = (type, data = {}) =>
    response.write(`event: ${type}\ndata: ${JSON.stringify({ type, ...data })}\n\n`);
  event("message_start", { message: { ...start, content: [], stop_reason: null, stop_sequence: null,
    usage: { input_tokens: 6, output_tokens: 0, cache_read_input_tokens: 2, cache_creation_input_tokens: 1 } } });
  if (tool) {
    event("content_block_start", { index: 0, content_block: { type: "tool_use", id: "toolu_fixture", name: "fixture_lookup", input: {} } });
    event("content_block_delta", { index: 0, delta: { type: "input_json_delta", partial_json: '{"term":"' } });
    event("content_block_delta", { index: 0, delta: { type: "input_json_delta", partial_json: 'rust"}' } });
  } else {
    event("content_block_start", { index: 0, content_block: { type: "text", text: "" } });
    event("content_block_delta", { index: 0, delta: { type: "text_delta", text: "native Anthropic " } });
    event("content_block_delta", { index: 0, delta: { type: "text_delta", text: "migration" } });
  }
  event("content_block_stop", { index: 0 });
  event("message_delta", { delta: { stop_reason: tool ? "tool_use" : "end_turn", stop_sequence: null }, usage: { output_tokens: 3 } });
  event("message_stop");
  response.end();
}

createServer((request, response) => {
  if (isXiaomiChatMock(request)) {
    handleXiaomiChatMock(request, response).catch(() => response.destroy());
    return;
  }
  if (isMuseChatMock(request)) {
    handleMuseChatMock(request, response).catch(() => response.destroy());
    return;
  }
  if (isKimiChatMock(request)) {
    handleKimiChatMock(request, response).catch(() => response.destroy());
    return;
  }
  if (isOAuthProbeMock(request)) {
    handleOAuthProbeMock(request, response);
    return;
  }
  if (isNousKiloMock(request)) {
    handleNousKiloMock(request, response).catch(() => response.destroy());
    return;
  }
  if (request.url === "/iflow-fixture/v1/chat/completions" && request.method === "POST") {
    (async () => {
      const key = "sk-iflow-fixture-private";
      const signature = createHmac("sha256", key).update([
        request.headers["user-agent"],
        request.headers["session-id"],
        request.headers["x-iflow-timestamp"],
      ].join(":")).digest("hex");
      let raw = "";
      for await (const chunk of request) raw += chunk;
      const body = JSON.parse(raw);
      if (request.headers.authorization !== `Bearer ${key}` || request.headers["user-agent"] !== "iFlow-Cli" ||
          !request.headers["session-id"]?.startsWith("session-") ||
          request.headers["x-iflow-signature"] !== signature || request.headers.cookie ||
          body.model !== "qwen3-coder-plus" || (body.stream && body.stream_options?.include_usage !== true)) {
        return response.writeHead(422, { "content-type": "application/json" })
          .end(JSON.stringify({ error: { message: "Unexpected signed iFlow request" } }));
      }
      const identity = { id: "chatcmpl-iflow-fixture", created: 1795046400, model: body.model };
      const usage = { prompt_tokens: 8, completion_tokens: 3, total_tokens: 11 };
      const content = "signed iFlow fixture";
      if (!body.stream) {
        return response.writeHead(200, { "content-type": "application/json" }).end(JSON.stringify({
          ...identity, object: "chat.completion", choices: [
            { index: 0, message: { role: "assistant", content }, finish_reason: "stop" },
          ], usage,
        }));
      }
      response.writeHead(200, { "content-type": "text/event-stream" });
      for (const [choices, meta] of [
        [[{ index: 0, delta: { role: "assistant" }, finish_reason: null }], {}],
        [[{ index: 0, delta: { content }, finish_reason: null }], {}],
        [[{ index: 0, delta: {}, finish_reason: "stop" }], { usage }],
      ]) {
        response.write(`data: ${JSON.stringify({ ...identity, object: "chat.completion.chunk", choices, ...meta })}\n\n`);
      }
      response.end("data: [DONE]\n\n");
    })().catch(() => response.destroy());
    return;
  }
  if (request.url === "/iflow-fixture/api/openapi/apikey") {
    const reply = (status, body) => response.writeHead(status, { "content-type": "application/json" })
      .end(JSON.stringify(body));
    if (request.headers.cookie !== "theme=test; BXAuth=synthetic-iflow-cookie;") {
      response.writeHead(401, { "content-type": "text/plain" }).end("Fixture cookie rejected");
      return;
    }
    if (request.method === "GET") {
      reply(200, { success: true, data: { name: "iflow-fixture-account" } });
      return;
    }
    (async () => {
      let raw = "";
      for await (const chunk of request) raw += chunk;
      if (request.method !== "POST" || request.headers.origin !== "https://platform.iflow.cn" ||
          JSON.parse(raw).name !== "iflow-fixture-account") {
        reply(400, { success: false, message: "Unexpected iFlow refresh" });
        return;
      }
      reply(200, { success: true, data: {
        name: "iflow-fixture-account", apiKey: "sk-iflow-fixture-private", expireTime: "2040-12-01T00:00:00Z",
      } });
    })().catch(() => response.destroy());
    return;
  }
  if (request.method === "GET" && request.url === "/zed-fixture/client/users/me") {
    const ok = request.headers.authorization === "42 synthetic-zed-session" &&
      request.headers["x-zed-system-id"] === "fixture-zed-system";
    response.writeHead(ok ? 200 : 401, { "content-type": "application/json" })
      .end(JSON.stringify(ok ? {
        display_name: "Zed Fixture", email: "fixture-zed@example.invalid",
        organizations: [{ id: "fixture-other" }, { id: { id: "fixture-personal" }, is_personal: true }],
      } : { error: { message: "Fixture Zed authorization rejected" } }));
    return;
  }
  if (request.method === "GET" && request.url === "/catalog-suggestions/api.kilo.ai/api/gateway/models") {
    response.writeHead(200, { "content-type": "application/json" }).end(JSON.stringify({ data: [
      { id: "fixture/free", name: "Free fixture model", isFree: true, context_length: 60000 },
      { id: "fixture/paid", name: "Paid fixture model", isFree: false, context_length: 50000 },
      { id: "fixture/no-limit", isFree: true },
      { id: "fixture/not-boolean", isFree: "true" },
    ] }));
    return;
  }
  if (isCoworkDiscoveryMock(request)) {
    handleCoworkDiscoveryMock(request, response).catch(() => response.destroy());
    return;
  }
  if (request.method === "GET" && request.url === "/gitlab-fixture/api/v4/user") {
    if (request.headers["private-token"] === "synthetic-gitlab-pat" &&
      request.headers.accept === "application/json") {
      response.writeHead(200, { "content-type": "application/json" })
        .end(JSON.stringify({ username: "fixture-user", name: "GitLab Fixture", public_email: "g-pat@example.invalid" }));
    } else {
      response.writeHead(401, { "content-type": "text/plain" }).end("Fixture token rejected");
    }
    return;
  }
  if (request.method === "GET" && request.url === "/mimo-fixture/v1/models") {
    if (request.headers.authorization === "Bearer sk-synthetic-mimo-key" &&
      request.headers["x-mimo-source"] === "mimocode-cli") {
      response.writeHead(200, { "content-type": "application/json" })
        .end(JSON.stringify({ data: [{ id: "mimo-one" }, { id: "mimo-two" }] }));
    } else {
      response.writeHead(401).end();
    }
    return;
  }
  if (isKiroModelsMock(request)) {
    handleKiroModelsMock(request, response).catch(() => response.destroy());
    return;
  }
  if (isKiroChatMock(request)) {
    handleKiroChatMock(request, response).catch(() => response.destroy());
    return;
  }
  if (isLocalImageMock(request)) {
    handleLocalImageMock(request, response).catch(() => response.destroy());
    return;
  }
  if (isCatalogSyncMock(request)) {
    handleCatalogSyncMock(request, response).catch(() => response.destroy());
    return;
  }
  if (request.method === "POST" && request.url === "/cloudflare-quick/api/tunnel/register") {
    (async () => {
      let raw = "";
      for await (const chunk of request) raw += chunk;
      const body = JSON.parse(raw);
      if (!/^[a-z0-9]{6}$/.test(body.shortId) || !/^https:\/\/fixture-[a-z]+\.trycloudflare\.com$/.test(body.tunnelUrl)) {
        response.writeHead(422).end();
        return;
      }
      response.writeHead(body.shortId === "fail01" ? 503 : 204).end();
    })().catch(() => response.destroy());
    return;
  }
  if (request.method === "GET" && request.url === "/cloudflare-quick/api/health") {
    response.writeHead(200, { "content-type": "application/json" }).end('{"status":"ok"}');
    return;
  }
  if (isSpeechProviderMock(request)) {
    handleSpeechProviderMock(request, response).catch(() => response.destroy());
    return;
  }
  if (isBedrockMock(request)) {
    handleBedrockMock(request, response).catch(() => response.destroy());
    return;
  }
  if (isOpenCodeChatMock(request)) {
    handleOpenCodeChatMock(request, response).catch(() => response.destroy());
    return;
  }
  if (isClineMock(request)) {
    handleClineMock(request, response).catch(() => response.destroy());
    return;
  }
  if (isOllamaChatMock(request)) {
    handleOllamaChatMock(request, response).catch(() => response.destroy());
    return;
  }
  if (isVertexVideoMock(request)) {
    handleVertexVideoMock(request, response).catch(() => response.destroy());
    return;
  }
  if (isVertexMock(request)) {
    handleVertexMock(request, response).catch(() => response.destroy());
    return;
  }
  if (isGithubMock(request)) {
    handleGithubMock(request, response).catch(() => response.destroy());
    return;
  }
  if (request.method === "POST" && request.url?.startsWith("/azure-fixture/openai/deployments/")) {
    azureChat(request, response).catch(() => response.destroy());
    return;
  }
  if (isVoiceMock(request)) {
    handleVoiceMock(request, response).catch(() => response.destroy());
    return;
  }
  if (isGeminiNativeTtsMock(request)) {
    handleGeminiNativeTtsMock(request, response).catch(() => response.destroy());
    return;
  }
  if (isGeminiCliMock(request)) {
    handleGeminiCliMock(request, response).catch(() => response.destroy());
    return;
  }
  if (isSystemOneMock(request)) {
    handleSystemOneMock(request, response).catch(() => response.destroy());
    return;
  }
  if (isCodexMock(request)) {
    handleCodexMock(request, response).catch(() => response.destroy());
    return;
  }
  if (request.method === "POST" && request.url?.includes("headroom-fixture/v1/compress")) {
    (async () => {
      let raw = "";
      for await (const chunk of request) raw += chunk;
      const body = JSON.parse(raw);
      const fail = (status, reason) => response.writeHead(status, { "Content-Type": "application/json" })
        .end(JSON.stringify({ error: { message: reason } }));
      if (request.url.startsWith("/broken-")) return fail(503, "mock compressor failure");
      if (!Array.isArray(body.messages) || !body.model || body.config?.compress_user_messages !== true) {
        return fail(422, "unexpected Headroom mock payload");
      }
      const messages = body.messages.map(message => message.content === headroomRaw ? { ...message, content: headroomCompact } : message);
      if (!messages.some(message => message.content === headroomCompact)) return fail(422, "missing eligible Headroom text");
      response.writeHead(200, { "Content-Type": "application/json" })
        .end(JSON.stringify({ messages, stats: { tokens_saved: 5 } }));
    })().catch(() => response.destroy());
    return;
  }
  if (isWebGatewayMock(request)) {
    handleWebGatewayMock(request, response).catch(() => response.destroy());
    return;
  }
  if (isVideoMock(request)) {
    handleVideoMock(request, response).catch(() => response.destroy());
    return;
  }
  if (request.method === "POST" && request.url === "/relay-fixture") {
    if (request.headers["x-relay-target"] !== "http://127.0.0.1:52985" ||
        request.headers["x-relay-path"] !== "/v1/chat/completions") {
      response.writeHead(400).end();
      return;
    }
    chat(request, response).catch(() => response.destroy());
    return;
  }
  if (isImageGenerationMock(request)) {
    handleImageGenerationMock(request, response).catch(() => response.destroy());
    return;
  }
  if (isTranscriptionMock(request)) {
    handleTranscriptionMock(request, response).catch(() => response.destroy());
    return;
  }
  if (isStaticChat(request)) {
    handleStaticChat(request, response).catch(() => response.destroy());
    return;
  }
  if (isSpeechMock(request)) {
    handleSpeechMock(request, response).catch(() => response.destroy());
    return;
  }
  if (isEmbeddingMock(request)) {
    handleEmbeddingMock(request, response).catch(() => response.destroy());
    return;
  }
  if (isGeminiChatMock(request)) {
    handleGeminiChatMock(request, response).catch(() => response.destroy());
    return;
  }
  if (request.method === "POST" && ["/v1/messages", "/anthropic/v1/messages"].includes(request.url)) {
    messages(request, response).catch(() => response.destroy());
    return;
  }
  if (request.method === "POST" && request.url === "/v1/responses") {
    responses(request, response).catch(() => response.writeHead(500).end());
    return;
  }
  if (request.method === "POST" && ["/v1/chat/completions", "/v1/chat/completions/redirect-target"].includes(request.url)) {
    chat(request, response).catch(() => response.writeHead(500).end());
    return;
  }
  if (request.method !== "GET" || request.url !== "/v1/models") {
    response.writeHead(404).end();
    return;
  }
  if (request.headers.authorization !== "Bearer offline-models-provider"
      || request.headers["x-9r-internal-models-fetch"] !== "1") {
    response.writeHead(401).end();
    return;
  }
  response.writeHead(200, { "Content-Type": "application/json" }).end(JSON.stringify({ data: models }));
}).listen(52989, "127.0.0.1").unref();

async function azureChat(request, response) {
  let raw = "";
  for await (const chunk of request) raw += chunk;
  const body = JSON.parse(raw);
  const explicit = body.model === "fixture-azure-deployment";
  const deployment = explicit ? "custom-deploy" : "fixture-azure-model";
  const version = explicit ? "2025-04-01-preview" : "2024-10-01-preview";
  const path = "/azure-fixture/openai/deployments/" + deployment + "/chat/completions?api-version=" + version;
  const expectedKey = explicit ? "offline-azure-specific" : "offline-azure-default";
  const expectedOrg = explicit ? "fixture-azure-org" : undefined;
  if (request.url !== path || request.headers["api-key"] !== expectedKey || request.headers.authorization ||
      request.headers["openai-organization"] !== expectedOrg || body.messages?.at(-1)?.content !== "fixture azure prompt" ||
      (body.stream && request.headers.accept !== "text/event-stream") || (explicit && !body.response_format?.json_schema)) {
    return response.writeHead(422, { "content-type": "application/json" })
      .end(JSON.stringify({ error: { message: "unexpected fixture Azure request" } }));
  }
  const header = { id: "chatcmpl-fixture-azure", created: 1784419200, model: body.model };
  if (!body.stream) {
    return response.writeHead(200, { "content-type": "application/json" }).end(JSON.stringify({
      ...header, object: "chat.completion", choices: [{
        index: 0, message: { role: "assistant", content: deployment }, finish_reason: "stop",
      }], usage,
    }));
  }
  response.writeHead(200, { "content-type": "text/event-stream" });
  const frame = (delta, finish_reason) => response.write("data: " + JSON.stringify({
    ...header, object: "chat.completion.chunk", choices: [{ index: 0, delta, finish_reason }],
  }) + "\n\n");
  frame({ role: "assistant", content: deployment }, null);
  frame({}, "stop");
  response.end("data: [DONE]\n\n");
}
