import { CAVEMAN_PROMPTS } from "../../open-sse/rtk/cavemanPrompts.js";
import { PONYTAIL_PROMPTS } from "../../open-sse/rtk/ponytailPrompt.js";
import { verifyGeminiVision } from "./mock-chat-vision.mjs";

export function isGeminiChatMock(request) {
  return request.method === "POST" &&
    /^\/gemini\/v1beta\/models\/[^/]+:(?:generateContent|streamGenerateContent)(?:\?|$)/.test(request.url);
}

export async function handleGeminiChatMock(request, response) {
  const url = new URL(request.url, "http://127.0.0.1");
  const action = url.pathname.split("/").at(-1);
  const model = action.split(":")[0].replace(/\([^()]+\)$/, "");
  const stream = action.endsWith(":streamGenerateContent");
  const fail = (status, message) => response.writeHead(status, { "Content-Type": "application/json" })
    .end(JSON.stringify({ error: { message } }));
  if (request.headers["x-goog-api-key"] !== "offline-provider-placeholder" ||
    request.headers.authorization || (stream && url.searchParams.get("alt") !== "sse")) {
    return fail(401, "unexpected Gemini chat fixture credentials or operation");
  }

  let raw = "";
  for await (const chunk of request) raw += chunk;
  const body = JSON.parse(raw);
  const turns = body.contents;
  const label = turns?.[0]?.parts?.[0]?.text;
  if (body.model !== model || !Array.isArray(turns) || !turns.length ||
    body.safetySettings?.length !== 5 || Object.hasOwn(body, "messages")) {
    return fail(400, "unexpected Gemini request shape");
  }
  if (!verifyGeminiVision(body)) return fail(422, "unexpected Gemini vision blocks");
  if (label === "Gemini thinking" &&
    (JSON.stringify(body.generationConfig.thinkingConfig) !== '{"thinkingLevel":"low","includeThoughts":true}' ||
      body.generationConfig.maxOutputTokens !== 8192)) {
    return fail(400, "unexpected Gemini thinking or max output");
  }
  if (label === "Gemini sampling" &&
    (body.generationConfig.temperature !== 0.25 || body.generationConfig.topP !== 0.6 || body.generationConfig.topK !== 33 ||
      body.generationConfig.maxOutputTokens !== 90 ||
      body.systemInstruction?.parts?.[0]?.text !== "fixture instructions")) {
    return fail(400, "unexpected Gemini sampling, system or token budget");
  }
  if (label === "Gemini sampling removed" && ["temperature", "topP", "topK"].some(key => key in body.generationConfig)) {
    return fail(400, "Gemini 4 rejects sampling");
  }
  if (label === "Gemini tool" &&
    (body.tools?.[0]?.functionDeclarations?.[0]?.name !== "_9_lookup" ||
      body.tools[0].functionDeclarations[0].parameters?.properties?.mode?.enum?.[0] !== "local")) {
    return fail(400, "unexpected Gemini tool schema");
  }
  if (label === "Gemini system prompt fixture") {
    const instructions = body.systemInstruction?.parts?.map(part => part.text);
    if (JSON.stringify(instructions) !== JSON.stringify([
      "fixture initial", CAVEMAN_PROMPTS.lite, PONYTAIL_PROMPTS.lite, "  Fixture custom system instruction  ",
    ])) {
      return fail(422, "unexpected Gemini system instructions");
    }
  }
  if (label === "Gemini signature" &&
    (turns[1]?.parts?.[0]?.thoughtSignature !== "migration-thought-signature" ||
      turns[2]?.parts?.at(-1)?.functionResponse?.response?.result?._ref !== "sample")) {
    return fail(400, "unexpected signature replay or tool result");
  }

  const meta = { responseId: "migration-gemini", modelVersion: model };
  const usageMetadata = { promptTokenCount: 5, candidatesTokenCount: 4, totalTokenCount: 11,
    cachedContentTokenCount: 1, thoughtsTokenCount: 2 };
  const answer = { text: "Gemini migration" };
  const thought = { text: "Gemini considers", thought: true };
  const call = { functionCall: { id: "fixture-gemini-call", name: "_9_lookup", args: { mode: "local" } },
    thoughtSignature: "migration-thought-signature" };
  const candidate = (parts, extra = {}) => ({ content: { role: "model", parts }, ...extra });
  if (!stream) {
    response.writeHead(200, { "Content-Type": "application/json" }).end(JSON.stringify({
      ...meta, createTime: "2026-11-19T00:00:00Z",
      candidates: [candidate([thought, answer], { finishReason: "STOP" })], usageMetadata,
    }));
    return;
  }

  response.writeHead(200, { "Content-Type": "text/event-stream" });
  const event = (candidates, extra = {}) =>
    response.write(`data: ${JSON.stringify({ ...meta, candidates, ...extra })}\n\n`);
  event([candidate([thought])]);
  event([candidate([label === "Gemini tool" ? call : answer])]);
  event([candidate([], { finishReason: "STOP" })], { usageMetadata });
  response.end();
}
