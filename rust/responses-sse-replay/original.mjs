import { mkdtempSync, rmSync } from "node:fs";
import { registerHooks } from "node:module";
import { tmpdir } from "node:os";
import { join } from "node:path";

const repo = new URL("../../", import.meta.url);
const data = mkdtempSync(join(tmpdir(), "9router-sse-reference-"));
process.env.DATA_DIR = data;
process.on("exit", () => rmSync(data, { recursive: true, force: true }));
registerHooks({
  resolve(specifier, context, next) {
    if (specifier.startsWith("@/")) {
      return next(new URL("src/" + specifier.slice(2), repo).href, context);
    }
    return next(specifier, context);
  },
});
const { initState, translateResponse } = await import(new URL("open-sse/translator/index.js", repo));
const { formatSSE } = await import(new URL("open-sse/utils/streamHelpers.js", repo));

const body = JSON.parse(process.argv[2]);
const state = initState("openai-responses");
state.targetFormat = "openai";
state.customToolNames = new Set(body.custom_tools || []);
for (const [index, chunk] of body.chunks.entries()) {
  const events = translateResponse("openai", "openai-responses", chunk, state);
  for (const event of events) process.stdout.write(formatSSE(event, "openai-responses"));
  console.log(
    `step ${index} pending=${state.completionPending && !state.completedSent} completed=${state.completedSent}`,
  );
}
