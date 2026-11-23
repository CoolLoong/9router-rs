import { verifyChatVision } from "./mock-chat-vision.mjs";
import { handleStaticMessages, staticMessagesFixture } from "./mock-static-messages.mjs";

const originalUrls = new Map([
  ["https://api.openai.com/v1/chat/completions", "openai"],
  ["https://api.groq.com/openai/v1/chat/completions", "groq"],
  ["https://openrouter.ai/api/v1/chat/completions", "openrouter"],
  ["https://api.deepseek.com/chat/completions", "deepseek"],
  ["https://api.perplexity.ai/v1/responses", "perplexity-agent"],
  ["https://api.minimax.io/v1/chat/completions", "minimax"],
  ["https://api.minimaxi.com/v1/chat/completions", "minimax-cn"],
  ["https://coding.dashscope.aliyuncs.com/v1/chat/completions", "alicode"],
  ["https://coding-intl.dashscope.aliyuncs.com/v1/chat/completions", "alicode-intl"],
  ["https://dashscope-intl.aliyuncs.com/compatible-mode/v1/chat/completions", "alims-intl"],
  ["https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1/chat/completions", "alitp-intl"],
  ["https://token.sensenova.cn/v1/chat/completions", "sensenova"],
  ["https://token.sensenova.ai/v1/chat/completions", "sensenova-intl"],
]);
const modelUrls = new Map([
  ["https://coding.dashscope.aliyuncs.com/v1/models", "alicode"],
  ["https://coding-intl.dashscope.aliyuncs.com/v1/models", "alicode-intl"],
  ["https://dashscope-intl.aliyuncs.com/compatible-mode/v1/models", "alims-intl"],
  ["https://token.sensenova.cn/v1/models", "sensenova"],
  ["https://token.sensenova.ai/v1/models", "sensenova-intl"],
]);
const dashscopeProbes = new Set(["alicode", "alicode-intl", "alims-intl"]);

export function staticChatFixture(url) {
  const catalog = modelUrls.get(url?.href);
  if (catalog) return `http://127.0.0.1:52989/provider-catalog/${catalog}`;
  const provider = originalUrls.get(url?.href);
  return provider ? `http://127.0.0.1:52989/static-chat/${provider}` : staticMessagesFixture(url);
}

export function isStaticChat(request) {
  return request.method === "POST" && request.url.startsWith("/static-chat/") ||
    request.method === "GET" && request.url.startsWith("/provider-catalog/");
}

export async function handleStaticChat(request, response) {
  if (request.url.startsWith("/provider-catalog/")) {
    const provider = request.url.split("/").at(-1);
    const header = provider.startsWith("sensenova") ? "accept" : "content-type";
    const status = request.headers.authorization !== `Bearer offline-static-${provider}` ? 401
      : request.headers[header] !== "application/json" ? 422
      : provider === "sensenova-intl" ? 503 : 200;
    return response.writeHead(status,{"content-type":"application/json"}).end(JSON.stringify({
      data:[{id:"fixture-model-for-"+provider,description:"Synthetic listing record"}],
    }));
  }
  if (request.url.startsWith("/static-chat/messages/")) {
    return handleStaticMessages(request, response);
  }
  const provider = request.url.split("/").at(-1);
  const fail = message => response.writeHead(422, { "Content-Type": "application/json" })
    .end(JSON.stringify({ error: { message } }));
  const expectedKey = ["openai", "groq"].includes(provider)
    ? `offline-${provider}-${provider === "openai" ? "speech" : "stt"}`
    : `offline-static-${provider}`;
  const rejectedProbe = dashscopeProbes.has(provider) && request.headers.authorization === `Bearer ${expectedKey}-rejected`;
  if (request.headers.authorization !== `Bearer ${expectedKey}` && !rejectedProbe) {
    return fail(`unexpected ${provider} fixture credential`);
  }
  let text = "";
  for await (const chunk of request) text += chunk;
  const body = JSON.parse(text);
  if (!verifyChatVision(body)) return fail("unexpected fixture chat vision blocks");
  const headers = { "Content-Type": "application/json" };
  if (provider === "perplexity-agent") {
    if (body.model !== "perplexity/sonar" || body.input !== "perplexity fixture" || body.store !== false) {
      return fail("unexpected Perplexity Agent request");
    }
    response.writeHead(200, headers).end(JSON.stringify({
      id: "resp_static_fixture", object: "response", model: body.model, created_at: 1795046400, status: "completed",
      output: [{ type: "message", role: "assistant", content: [{ type: "output_text", text: "native fixture" }] }],
      usage: { input_tokens: 6, output_tokens: 4, total_tokens: 10 },
    }));
    return;
  }

  if (!Array.isArray(body.messages)) return fail("missing chat messages");
  if (provider === "groq" && body.messages.some(message =>
    message.role === "assistant" && ["reasoning", "reasoning_content", "reasoning_details"].some(key => key in message))) {
    return fail("Groq rejected assistant reasoning replay");
  }
  const label = body.messages.at(-1)?.content;
  if (dashscopeProbes.has(provider) && label === "test") {
    if (body.model !== "qwen3.5-plus" || body.max_tokens !== 1) return fail("Wrong DashScope credential probe");
    return response.writeHead(rejectedProbe ? 401 : 400, headers).end("{}");
  }
  if (rejectedProbe) return fail("Unexpected inference using a rejected fixture key");
  if (["alicode", "alicode-intl", "alims-intl", "alitp-intl"].includes(provider)) {
    const part = label?.[0];
    if (part?.cache_control?.type !== "ephemeral" || "signature" in part) {
      return fail("DashScope cache-control was lost or signature leaked");
    }
    if (part.text === "fixture ali messages") {
      const system = body.messages.find(message => message.role === "system")?.content;
      if (system?.length !== 1 || system[0]?.text !== "Fixture Alibaba system" || system[0]?.cache_control?.ttl !== "1h") {
        return fail("DashScope system cache boundary did not survive billing metadata stripping");
      }
    }
  }
  if (provider === "openrouter") {
    const overridden = label === "fixture header override";
    if (request.headers["x-title"] !== (overridden ? "fixture title" : "Endpoint Proxy") ||
      request.headers["http-referer"] !== "https://endpoint-proxy.local" ||
      request.headers["x-fixture-override"] !== (overridden ? "fixture transport header" : undefined)) {
      return fail("unexpected OpenRouter provider headers");
    }
  }
  if (provider === "deepseek" && label === "fixture DeepSeek thinking" &&
    (body.model !== "deepseek-v4-flash" || body.thinking?.type !== "enabled" || body.reasoning_effort !== "high")) {
    return fail("unexpected DeepSeek normalized thinking");
  }
  if (provider === "deepseek" && label !== "fixture DeepSeek thinking" && (body.model !== "deepseek-v4-pro" || body.extra_body?.thinking?.type !== "enabled" ||
    body.reasoning_effort !== "max" || body.messages.find(message => message.role === "assistant")?.reasoning_content !== " ")) {
    return fail("unexpected DeepSeek V4 Pro alias or reasoning");
  }
  if (provider === "openrouter" && body.response_format?.type !== "json_schema") return fail("structured output schema was altered");
  if (provider === "minimax" &&
    (label !== "fixture minimax chat" || body.model !== "MiniMax-M3" ||
      body.messages.find(message => message.role === "assistant")?.reasoning_content !== " ")) {
    return fail("unexpected MiniMax Chat transport or reasoning replay");
  }
  if (provider === "openai" && (body.max_completion_tokens !== 74 || "max_tokens" in body ||
    request.headers.accept !== "text/event-stream")) return fail("missing OpenAI token rename or upstream stream selection");
  if (label === "fixture OpenAI thinking" &&
    (body.model !== "gpt-5.4" || body.reasoning_effort !== "high" || "thinking" in body)) {
    return fail("unexpected OpenAI normalized thinking");
  }
  if (label === "fixture provider override precedence" &&
    (body.model !== "gpt-5.4" || body.reasoning_effort !== "low")) {
    return fail("provider setting replaced client reasoning effort");
  }

  const model = body.model;
  const usage = { prompt_tokens: 6, completion_tokens: 4, total_tokens: 10 };
  const base = { id: "chatcmpl-static-fixture", created: 1795046400, model };
  const content = `native ${provider} fixture`;
  if (provider === "openai" || body.stream === true) {
    response.writeHead(200, { "Content-Type": "text/event-stream" });
    const chunk = (choices, extra = {}) => response.write(`data: ${JSON.stringify({
      ...base, object: "chat.completion.chunk", choices, ...extra,
    })}\n\n`);
    chunk([{ index: 0, delta: { role: "assistant" }, finish_reason: null }]);
    const reasoningField = provider.startsWith("sensenova") ? "reasoning" : "reasoning_content";
    chunk([{ index: 0, delta: { [reasoningField]: "before text" }, finish_reason: null }]);
    chunk([{ index: 0, delta: { content }, finish_reason: null }]);
    chunk([{ index: 0, delta: {}, finish_reason: "stop" }], { usage });
    response.end("data: [DONE]\n\n");
    return;
  }
  response.writeHead(200, headers).end(JSON.stringify({
    ...base, object: "chat.completion",
    choices: [{ index: 0, message: { role: "assistant", content }, finish_reason: "stop" }], usage,
  }));
}
