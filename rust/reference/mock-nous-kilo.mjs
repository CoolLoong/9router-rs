const originToFixture = new Map([
  ["https://inference-api.nousresearch.com", "/nous-management"],
  ["https://api.kilo.ai", "/kilocode-device/api/openrouter"],
]);

export function nousKiloFixture(url) {
  const fixture = originToFixture.get(url?.origin);
  if (fixture === "/nous-management" && ["/v1/models", "/v1/chat/completions"].includes(url.pathname)) {
    return `http://127.0.0.1:52989${fixture}${url.pathname.slice(3)}`;
  }
  if (fixture === "/kilocode-device/api/openrouter" && url.pathname === "/api/openrouter/chat/completions") {
    return `http://127.0.0.1:52989/kilocode-device${url.pathname}`;
  }
  return null;
}

export function isNousKiloMock(request) {
  return request.url.startsWith("/nous-management/") || request.url === "/kilocode-device/api/openrouter/chat/completions";
}

export async function handleNousKiloMock(request, response) {
  const reply = (status, body) => response.writeHead(status, { "content-type": "application/json" }).end(JSON.stringify(body));
  const bearer = request.headers.authorization;
  if (request.url === "/nous-management/models" && request.method === "GET") {
    if (bearer === "Bearer offline-nous-rejected") return reply(503, { error: "fixture unavailable" });
    if (bearer !== "Bearer offline-nous-key") return reply(401, { error: "fixture authorization required" });
    return reply(200, { results: [
      { id: " other/paid ", name: "Paid fixture", top_provider: { context_length: "128000", max_completion_tokens: "8192" } },
      { id: "only-image", architecture: { output_modalities: ["image"] } },
      { id: "nousresearch/fixture", description: "Nous text model", architecture: { output_modalities: ["text", "image"] } },
      { id: " " },
      { id: "other/text", context_length: 200000, architecture: { output_modalities: [] } },
    ] });
  }
  let raw = "";
  for await (const chunk of request) raw += chunk;
  const body = JSON.parse(raw);
  if (request.url === "/nous-management/chat/completions" && request.method === "POST") {
    if (body.model !== "poolside/laguna-xs-2.1:free" || body.max_tokens !== 1 || body.stream !== false ||
        body.messages?.length !== 1 || body.messages[0].role !== "user" || body.messages[0].content !== "ping") {
      return reply(422, { error: "invalid credential probe" });
    }
    const status = bearer === "Bearer offline-nous-key" ? 200
      : bearer === "Bearer offline-nous-quota" ? 402 : 403;
    return reply(status, {});
  }
  if (request.method !== "POST" || bearer !== "Bearer offline-kilo-chat" ||
      request.headers["x-kilocode-organizationid"] !== "kilo-fixture-org" ||
      body.model !== "openai/gpt-4.1" || body.messages?.[0]?.content !== "Kilo shared fixture") {
    return reply(422, { error: { message: "unexpected Kilo chat request" } });
  }
  const base = { id: "chatcmpl-shared-kilo", created: 1795046400, model: body.model };
  const usage = { prompt_tokens: 5, completion_tokens: 3, total_tokens: 8 };
  const content = "Kilo fixture reply";
  if (!body.stream) {
    return reply(200, { ...base, object: "chat.completion",
      choices: [{ index: 0, message: { role: "assistant", content }, finish_reason: "stop" }], usage });
  }
  response.writeHead(200, { "content-type": "text/event-stream" });
  for (const [choices, extra] of [
    [[{ index: 0, delta: { role: "assistant", content }, finish_reason: null }], {}],
    [[{ index: 0, delta: {}, finish_reason: "stop" }], { usage }],
  ]) {
    response.write(`data: ${JSON.stringify({ ...base, object: "chat.completion.chunk", choices, ...extra })}\n\n`);
  }
  response.end("data: [DONE]\n\n");
}
