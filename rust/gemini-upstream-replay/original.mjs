import { readFile } from "node:fs/promises";

globalThis.fetch = () => { throw new Error("Gemini translator replay must be offline"); };
const root = new URL("../../open-sse/", import.meta.url);
const stream = await readFile(new URL("translator/response/gemini-to-openai.js", root), "utf8");
const json = await readFile(new URL("handlers/chatCore/nonStreamingHandler.js", root), "utf8");
const streamStart = stream.indexOf("function chunkMeta(");
const streamEnd = stream.indexOf("register(FORMATS.GEMINI", streamStart);
const jsonStart = json.indexOf("  if (targetFormat === FORMATS.GEMINI ||");
const jsonEnd = json.indexOf("  // Claude\n", jsonStart);
if (streamStart < 0 || streamEnd < streamStart || jsonStart < 0 || jsonEnd < jsonStart) {
  throw new Error("Pinned Gemini conversion was not found");
}
const source = [
  "import { FORMATS } from " + JSON.stringify(new URL("translator/formats.js", root).href) + ";",
  "import { ROLE, OPENAI_BLOCK, OPENAI_FINISH, DEFAULT_IMAGE_MIME } from " + JSON.stringify(new URL("translator/schema/index.js", root).href) + ";",
  ...[
    ["chunk","buildChunk"], ["usage","toOpenAIUsage"], ["reasoning","reasoningDelta"],
    ["image","encodeDataUri"], ["finishReason","toOpenAIFinish"],
  ].map(([file, name]) => "import {" + name + "} from " + JSON.stringify(new URL("translator/concerns/" + file + ".js", root).href) + ";"),
  `export const signatures = [];
   function storeGeminiThoughtSignature(toolCallId, signature, _sessionId, model) {
     if (typeof toolCallId === "string" && toolCallId && typeof signature === "string" && signature) {
       signatures.push({tool_call_id:toolCallId, signature, model:typeof model === "string" ? model : ""});
     }
   }`,
  stream.slice(streamStart, streamEnd),
  "export function nativeJson(responseBody) {",
  " const targetFormat = FORMATS.GEMINI;",
  json.slice(jsonStart, jsonEnd),
  "}",
].join("\n");
const original = await import("data:text/javascript;base64," + Buffer.from(source).toString("base64"));
const input = JSON.parse(process.argv[2]);
const now = Date.now;
Date.now = () => 1_785_000_123_456;
try {
  if (input.mode === "json") {
    const raw = input.data.usageMetadata || input.data.response?.usageMetadata;
    const accounting_usage = raw ? {
      prompt_tokens:raw.promptTokenCount || 0,
      completion_tokens:raw.candidatesTokenCount || 0,
      cached_tokens:raw.cachedContentTokenCount || 0,
      reasoning_tokens:raw.thoughtsTokenCount || 0,
    } : null;
    console.log(JSON.stringify({result:original.nativeJson(input.data), accounting_usage}));
  } else {
    const state = {messageId:null,model:input.model || "",usage:null};
    const frames = input.frames.map(({data}) => original.geminiToOpenAIResponse(data, state) || []);
    frames.push(original.geminiToOpenAIResponse(null, state) || []);
    console.log(JSON.stringify({frames,usage:state.usage,signatures:original.signatures}));
  }
} finally {
  Date.now = now;
}
