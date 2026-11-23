const prefix = "/gemini-cli/v1internal:";

export function isGeminiCliMock(request) {
  return request.method === "POST" && request.url.startsWith(prefix);
}

export async function handleGeminiCliMock(request, response) {
  const stream = request.url.endsWith(":streamGenerateContent?alt=sse");
  const invalid = message => response.writeHead(422, {"Content-Type":"application/json"})
    .end(JSON.stringify({error:{message}}));
  if (request.headers.authorization !== "Bearer offline-gemini-cli" ||
      !/^GeminiCLI\/[^/]+\/gemini-2\.5-flash \([^)]*; terminal\)$/.test(request.headers["user-agent"] || "") ||
      !request.headers["x-goog-api-client"]?.includes("google-genai-sdk/") ||
      request.headers.accept !== (stream ? "text/event-stream" : "application/json")) {
    return invalid("unexpected Gemini CLI request headers");
  }
  let raw = "";
  for await (const chunk of request) raw += chunk;
  const body = JSON.parse(raw);
  const content = body.request;
  if (body.project !== "fixture-code-assist" || body.model !== "gemini-2.5-flash" || body.userAgent !== "gemini-cli" ||
      !body.requestId?.startsWith("agent-") || content?.sessionId !== "1234" ||
      content?.safetySettings?.length !== 5 || !content?.contents?.length ||
      "model" in content || "messages" in body) return invalid("unexpected Cloud Code Assist envelope");
  const label = content.contents[0]?.parts?.[0]?.text;
  if (label === "Gemini CLI tool validation" &&
      (content.tools?.[0]?.functionDeclarations?.[0]?.name !== "fixture_lookup" ||
       content.toolConfig?.functionCallingConfig?.mode !== "VALIDATED")) {
    return invalid("missing Gemini CLI function mode");
  }
  const meta = { responseId:"gemini-cli-fixture",modelVersion:body.model };
  const usageMetadata = {promptTokenCount:6,candidatesTokenCount:4,totalTokenCount:10};
  const text = { text:"Cloud Code Assist fixture" };
  const candidate = (parts, extra = {}) => ({content:{role:"model",parts},...extra});
  if (!stream) {
    response.writeHead(200, {"Content-Type":"application/json"}).end(JSON.stringify({
      response:{...meta,createTime:"2026-11-19T00:00:00Z",
        candidates:[candidate([text],{finishReason:"STOP"})],usageMetadata}
    }));
    return;
  }
  response.writeHead(200, {"Content-Type":"text/event-stream"});
  for (const [candidates, extra] of [[[candidate([text])],{}],[[candidate([],{finishReason:"STOP"})],{usageMetadata}]]) {
    response.write(`data: ${JSON.stringify({response:{...meta,candidates,...extra}})}\n\n`);
  }
  response.end();
}
