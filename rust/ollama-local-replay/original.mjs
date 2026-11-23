import fs from "node:fs";
import { registerHooks } from "node:module";
import { fileURLToPath } from "node:url";

const fixture = JSON.parse(fs.readFileSync(0, "utf8"));
const localFetch = globalThis.fetch;
registerHooks({
  resolve(specifier, context, next) {
    if (specifier === "node-machine-id") {
      return next("data:text/javascript,export const machineIdSync = () => 'fixture-machine';", context);
    }
    if (context.parentURL?.endsWith("/open-sse/executors/base.js") && specifier.endsWith("/proxyFetch.js")) {
      return next("data:text/javascript,export const proxyAwareFetch = (url, init) => globalThis.fixtureOllama(url, init);", context);
    }
    if (!specifier.startsWith("@/") && !specifier.startsWith("open-sse/")) return next(specifier, context);
    const relative = specifier.startsWith("@/") ? `src/${specifier.slice(2)}` : specifier;
    const target = new URL(`../../${relative}`, import.meta.url);
    const info = fs.statSync(fileURLToPath(target), { throwIfNoEntry:false });
    const url = info?.isDirectory() ? new URL(`${target.href}/index.js`)
      : info ? target : new URL(`${target.href}.js`);
    return next(url.href, context);
  },
});
globalThis.fixtureOllama = (url, init) => {
  if (url !== fixture.expectedUrl) throw new Error("Unexpected fixture endpoint selection");
  return localFetch(fixture.endpoint, init);
};
const { OllamaLocalExecutor } = await import("../../open-sse/executors/ollama-local.js");
const executor = new OllamaLocalExecutor();
if (fixture.mode === "url") {
  const url = executor.buildUrl(fixture.model, fixture.stream, 0, fixture.account);
  console.log(JSON.stringify({url}));
} else {
  const { translateRequest } = await import("../../open-sse/translator/index.js");
  const { ollamaToOpenAIResponse, ollamaBodyToOpenAI } = await import("../../open-sse/translator/response/ollama-to-openai.js");
  const translated = translateRequest("openai","ollama",fixture.model,fixture.body,fixture.stream,fixture.account,"ollama-local");
  const result = await executor.execute({
    model:fixture.model, body:translated, stream:fixture.stream,
    credentials:fixture.account, providerOverrides:fixture.settings.providerOverrides?.["ollama-local"],
  });
  const raw = await result.response.text();
  let body = raw;
  if (result.response.ok && !fixture.stream) body = JSON.stringify(ollamaBodyToOpenAI(JSON.parse(raw)));
  if (result.response.ok && fixture.stream) {
    const state = { model:fixture.model };
    let frames = [];
    for (const line of raw.split("\n")) {
      if (!line.trim().startsWith("{")) continue;
      let parsed;
      try { parsed = JSON.parse(line); } catch { continue; }
      const chunk = ollamaToOpenAIResponse(parsed,state);
      if (chunk) frames.push(`data: ${JSON.stringify(chunk)}\n\n`);
    }
    body = frames.join("") + "data: [DONE]\n\n";
  }
  console.log(JSON.stringify({status:result.response.status,body,url:result.url}));
}
process.exit(0);
