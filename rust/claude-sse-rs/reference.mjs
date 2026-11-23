import { readFile } from "node:fs/promises";

globalThis.fetch = () => { throw new Error("Claude translator replay must be offline"); };

const root = new URL("../../open-sse/translator/", import.meta.url);
const path = new URL("response/openai-to-claude.js", root);
const source = await readFile(path, "utf8");
const codeStart = source.indexOf("const CLAUDE_OAUTH_TOOL_PREFIX");
const codeEnd = source.indexOf("// Register\n", codeStart);
if (codeStart < 0 || codeEnd < 0) throw new Error("Missing upstream Claude translator");

const evaluate = [
  "import { ROLE, CLAUDE_BLOCK, MODEL_FALLBACK } from " + JSON.stringify(new URL("schema/index.js", root).href) + ";",
  "import { fromOpenAIFinish } from " + JSON.stringify(new URL("concerns/finishReason.js", root).href) + ";",
  "import { extractReasoningText } from " + JSON.stringify(new URL("concerns/reasoning.js", root).href) + ";",
  source.slice(codeStart, codeEnd),
].join("\n");
const upstream = await import("data:text/javascript;base64," + Buffer.from(evaluate).toString("base64"));

export function openaiToClaudeResponse(chunks, fixture) {
  if (!Array.isArray(chunks)) return null;
  const state = { toolCalls: new Map(), usage: fixture?.usage || null };
  const frames = chunks.map((chunk) => upstream.openaiToClaudeResponse(chunk, state) || []);
  frames.push(upstream.openaiToClaudeResponse(null, state) || []);
  return {
    frames,
    usage: state.usage,
    finished: frames.some((step) => step.some((frame) => frame.type === "message_stop")),
  };
}
