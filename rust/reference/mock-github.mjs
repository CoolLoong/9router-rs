const CHAT = "/github/chat/completions";
const RESPONSES = "/github/responses";
const MESSAGES = "/github/v1/messages";
const MODELS = "/github/models";

export function githubFixture(url) {
  if (url.origin !== "https://api.githubcopilot.com") return null;
  if (url.pathname === "/chat/completions") return "http://127.0.0.1:52989" + CHAT + url.search;
  if (url.pathname === "/responses") return "http://127.0.0.1:52989" + RESPONSES + url.search;
  if (url.pathname === "/v1/messages") return "http://127.0.0.1:52989" + MESSAGES + url.search;
  if (url.pathname === "/models") return "http://127.0.0.1:52989" + MODELS + url.search;
  return null;
}

export function isGithubMock(request) {
  return (request.method === "POST" && [CHAT, RESPONSES, MESSAGES].includes(request.url))
    || (request.method === "GET" && request.url === MODELS);
}

export async function handleGithubMock(request, response) {
  const error = (status, message) => response.writeHead(status, { "Content-Type": "application/json" })
    .end(JSON.stringify({ error: { message } }));
  if (request.url === MODELS) {
    const live = request.headers["x-github-api-version"] !== undefined;
    const expected = live ? {
      "user-agent": "GitHubCopilotChat/0.38.0",
      "editor-version": "vscode/1.110.0",
      "editor-plugin-version": "copilot-chat/0.38.0",
      "x-github-api-version": "2025-04-01",
    } : {
      "user-agent": "GitHubCopilotChat/0.26.7",
      "editor-version": "vscode/1.107.1",
      "editor-plugin-version": "copilot-chat/0.26.7",
    };
    if (!Object.entries(expected).every(([name,value]) => request.headers[name] === value) ||
        request.headers["copilot-integration-id"] !== "vscode-chat") {
      return error(422, "Unexpected GitHub model discovery headers");
    }
    const token = request.headers.authorization;
    if (token === "Bearer offline-github-empty-catalog") {
      return response.writeHead(200, { "Content-Type": "application/json" }).end('{"data":[]}');
    }
    if (token === "Bearer offline-github-failed-catalog") {
      return error(503, "Fixture GitHub model catalog unavailable");
    }
    if (token !== "Bearer offline-github-copilot") {
      return error(401, "Unexpected GitHub model discovery token");
    }
    const chat = { type: "chat" };
    const data = [
      { id: "fixture-github-live", name: "Fixture GitHub Live", version: "2", capabilities: chat, model_picker_enabled: true },
      { id: "fixture-github-enabled", capabilities: chat, policy: { state: "enabled" } },
      { id: "fixture-github-live", name: "Duplicate", capabilities: chat },
      { id: "fixture-github-preview", capabilities: chat, policy: { state: "preview" } },
      { id: "fixture-github-disabled", capabilities: chat, policy: { state: "disabled" } },
      { id: "fixture-github-embedding", capabilities: { type: "embedding" } },
    ];
    return response.writeHead(200, { "Content-Type": "application/json" }).end(JSON.stringify({ data }));
  }

  const expected = {
    authorization: "Bearer offline-github-copilot",
    "content-type": "application/json",
    "user-agent": "GitHubCopilotChat/0.38.0",
    "editor-version": "vscode/1.110.0",
    "editor-plugin-version": "copilot-chat/0.38.0",
    "x-github-api-version": "2025-04-01",
    "copilot-integration-id": "vscode-chat",
    "openai-intent": "conversation-panel",
    "anthropic-version": "2023-06-01",
    "x-initiator": "user",
    "x-vscode-user-agent-library-version": "electron-fetch",
  };
  if (!Object.entries(expected).every(([name, value]) => request.headers[name] === value) ||
      !/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i.test(request.headers["x-request-id"] || "")) {
    return error(422, "Unexpected GitHub Copilot request headers");
  }
  let raw = "";
  for await (const bytes of request) raw += bytes;
  let body;
  try { body = JSON.parse(raw); } catch { return error(422, "Invalid GitHub fixture body"); }
  if (request.url === MESSAGES) {
    const label = body.messages?.find(message => message.role === "user")?.content?.[0]?.text;
    const streaming = label === "fixture GitHub Claude stream";
    const lastSystem = body.system?.at(-1);
    if (!["fixture GitHub Claude", "fixture GitHub Claude stream"].includes(label) ||
        body.model !== "claude-sonnet-4.6" || body.stream !== true || body.max_tokens !== 64 ||
        !lastSystem?.text?.includes("fixture instructions") || lastSystem.cache_control?.ttl !== "1h" ||
        request.headers.accept !== (streaming ? "text/event-stream" : "application/json")) {
      return error(422, "Unexpected GitHub Claude Messages body or Accept");
    }
    const text = streaming ? "GitHub Claude SSE fixture" : "GitHub Claude fixture";
    response.writeHead(200, { "Content-Type": "text/event-stream" });
    for (const data of [
      { type: "message_start", message: { id: "msg_gh_claude_fixture", model: body.model,
        usage: { input_tokens: 7, cache_read_input_tokens: 3, cache_creation_input_tokens: 2 } } },
      { type: "content_block_start", index: 0, content_block: { type: "text", text: "" } },
      { type: "content_block_delta", index: 0, delta: { type: "text_delta", text } },
      { type: "content_block_stop", index: 0 },
      { type: "message_delta", delta: { stop_reason: "end_turn" }, usage: { output_tokens: 3 } },
      { type: "message_stop" },
    ]) {
      response.write(`event: ${data.type}\ndata: ${JSON.stringify(data)}\n\n`);
    }
    return response.end("data: [DONE]\n\n");
  }
  if (request.url === RESPONSES) {
    const label = body.input?.find(item => item.role === "user")?.content?.[0]?.text;
    if (!["fixture GitHub Responses", "fixture GitHub cached Responses"].includes(label) ||
        body.model !== "gpt-5.3-codex" || body.stream !== true || body.store !== false ||
        body.max_output_tokens !== 96 || "messages" in body ||
        request.headers.accept !== (label === "fixture GitHub Responses" ? "application/json" : "text/event-stream")) {
      return error(422, "Unexpected GitHub Responses body or Accept");
    }
    const text = label === "fixture GitHub cached Responses" ? "GitHub cached Responses fixture" : "GitHub Responses fixture";
    response.writeHead(200, { "Content-Type": "text/event-stream" });
    for (const data of [
      { type: "response.created" },
      { type: "response.output_text.delta", delta: text },
      { type: "response.completed", response: { usage: { input_tokens: 8, output_tokens: 3, total_tokens: 11 } } },
    ]) {
      response.write(`event: ${data.type}\ndata: ${JSON.stringify(data)}\n\n`);
    }
    return response.end("data: [DONE]\n\n");
  }
  const label = body.messages?.find(message => message.role === "user")?.content;
  if (label === "fixture GitHub cached Responses") return error(422, "GitHub Responses route should have been cached");
  if (label === "fixture GitHub Responses" && body.model === "gpt-5.3-codex" &&
      body.max_completion_tokens === 64 && !("max_tokens" in body)) {
    return error(400, "Model not accessible via the /chat/completions endpoint");
  }
  const streaming = label === "fixture GitHub Chat stream";
  if (!["fixture GitHub Chat", "fixture GitHub Chat stream"].includes(label) || body.model !== "gpt-5.4" ||
      body.max_completion_tokens !== 64 || "max_tokens" in body || "temperature" in body ||
      request.headers.accept !== (streaming ? "text/event-stream" : "application/json")) {
    return error(422, "Unexpected GitHub Chat Completions body or Accept");
  }
  const common = { id: "chatcmpl_gh_fixture", created: 1795046400, model: body.model };
  const usage = { prompt_tokens: 4, completion_tokens: 2, total_tokens: 6 };
  if (!streaming) {
    return response.writeHead(200, { "Content-Type": "application/json" })
      .end(JSON.stringify({
        ...common, object: "chat.completion",
        choices: [{ index: 0, message: { role: "assistant", content: "GitHub Chat fixture" }, finish_reason: "stop" }],
        usage,
      }));
  }
  response.writeHead(200, { "Content-Type": "text/event-stream" });
  for (const chunk of [
    { choices: [{ index: 0, delta: { role: "assistant", content: "GitHub stream fixture" }, finish_reason: null }] },
    { choices: [{ index: 0, delta: {}, finish_reason: "stop" }], usage },
  ]) response.write(`data: ${JSON.stringify({ ...common, object: "chat.completion.chunk", ...chunk })}\n\n`);
  response.end("data: [DONE]\n\n");
}
