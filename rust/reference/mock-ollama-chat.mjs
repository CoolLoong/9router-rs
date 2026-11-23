const CHAT = "/ollama-local/api/chat";

export function isOllamaChatMock(request) {
  return request.method === "POST" && request.url === CHAT;
}

export async function handleOllamaChatMock(request, response) {
  const fail = message => response.writeHead(422, { "Content-Type": "application/json" })
    .end(JSON.stringify({ error: { message } }));
  if (request.headers.authorization !== "Bearer offline-ollama-fixture" ||
      request.headers["content-type"] !== "application/json") {
    return fail("Unexpected Ollama Local authorization or content type");
  }
  let raw = "";
  for await (const chunk of request) raw += chunk;
  let body;
  try { body = JSON.parse(raw); } catch { return fail("Invalid Ollama Local fixture body"); }
  const label = body.messages?.find(message => message.role === "user")?.content;
  const streaming = label === "fixture Ollama stream";
  if (!["fixture Ollama JSON", "fixture Ollama stream"].includes(label) ||
      body.model !== "qwen3:8b" || body.stream !== streaming ||
      body.options?.num_predict !== 64 || body.options?.temperature !== 0.125 ||
      request.headers.accept !== (streaming ? "text/event-stream" : "*/*")) {
    return fail("Unexpected Ollama Local request body or Accept");
  }
  const usage = { prompt_eval_count: 11, eval_count: 3, prompt_eval_cached_count: 4 };
  if (!streaming) {
    return response.writeHead(200, { "Content-Type": "application/json" }).end(JSON.stringify({
      model: body.model, done: true, done_reason: "stop", ...usage,
      message: { role: "assistant", content: "Ollama JSON fixture" },
    }));
  }
  const tool = body.messages.find(message => message.role === "tool");
  const call = body.messages.find(message => message.role === "assistant")?.tool_calls?.[0];
  if (tool?.tool_name !== "fixture_lookup" || tool.content !== "previous result" ||
      call?.function?.name !== "fixture_lookup" || call.function.arguments?.q !== "prior" ||
      body.tools?.[0]?.function?.name !== "fixture_lookup") {
    return fail("Unexpected Ollama Local tool history");
  }
  const chunk = {
    model: body.model, done: false,
    message: { role: "assistant", content: "Ollama stream fixture", tool_calls: [
      { id: "call_ollama_fixture", function: { index: 0, name: "fixture_lookup", arguments: { q: "next" } } },
    ] },
  };
  const done = { model: body.model, done: true, done_reason: "stop", ...usage };
  response.writeHead(200, { "Content-Type": "application/json" });
  response.write(JSON.stringify(chunk) + "\n");
  response.end(JSON.stringify(done));
}
