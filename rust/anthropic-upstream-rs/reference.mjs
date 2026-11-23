import { readFile } from "node:fs/promises";

globalThis.fetch = () => { throw new Error("Anthropic translator replay must be offline"); };

const root = new URL("../../open-sse/", import.meta.url);
const streamCode = await readFile(new URL("translator/response/claude-to-openai.js", root), "utf8");
const jsonCode = await readFile(new URL("handlers/chatCore/nonStreamingHandler.js", root), "utf8");
const streamStart = streamCode.indexOf("function createChunk(");
const streamEnd = streamCode.indexOf("// Register\n", streamStart);
const jsonStart = jsonCode.indexOf("  if (targetFormat === FORMATS.CLAUDE) {");
const jsonEnd = jsonCode.indexOf("  // Ollama\n", jsonStart);
if (streamStart < 0 || streamEnd < streamStart || jsonStart < 0 || jsonEnd < jsonStart) {
  throw new Error("Missing upstream Anthropic conversion");
}
const source = [
  "import { FORMATS } from " + JSON.stringify(new URL("translator/formats.js", root).href) + ";",
  "import { ROLE, OPENAI_BLOCK, CLAUDE_BLOCK, OPENAI_FINISH } from " + JSON.stringify(new URL("translator/schema/index.js", root).href) + ";",
  ...[
    ["chunk", "buildChunk"], ["usage", "toOpenAIUsage"],
    ["reasoning", "reasoningDelta"], ["finishReason", "toOpenAIFinish"],
  ].map(([file, name]) => "import {" + name + "} from " + JSON.stringify(new URL("translator/concerns/" + file + ".js", root).href) + ";"),
  streamCode.slice(streamStart, streamEnd),
  "export function nativeJson(responseBody) {",
  "  const targetFormat = FORMATS.CLAUDE;",
  jsonCode.slice(jsonStart, jsonEnd),
  "}",
].join("\n");
const original = await import("data:text/javascript;base64," + Buffer.from(source).toString("base64"));
const wire = (response) => JSON.parse(JSON.stringify(response));

export function claudeToOpenAIResponse(fixture) {
  if (!fixture?.mode) return null;
  const now = Date.now;
  Date.now = () => 1_785_000_123_456;
  try {
    if (fixture.mode === "json") return wire(original.nativeJson(fixture.data));
    if (fixture.mode === "stream") {
      const state = { messageId: null, model: null, toolCalls: new Map(), usage: null };
      const frames = (fixture.frames || []).map(({ event, data }) => {
        const chunk = data && !data.type && event ? { ...data, type: event } : data;
        return wire(original.claudeToOpenAIResponse(chunk, state) || []);
      });
      frames.push(original.claudeToOpenAIResponse(null, state) || []);
      return { frames, usage: state.usage };
    }
    return null;
  } finally {
    Date.now = now;
  }
}
