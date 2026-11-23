
import { registerHooks } from "node:module";
const repo = new URL("../../", import.meta.url);
registerHooks({
  resolve(specifier, context, next) {
    if (specifier === "@/lib/usageDb.js") return next("data:text/javascript,export const trackPendingRequest=()=>null,appendRequestLog=()=>null,saveRequestDetail=()=>null;",context);
    if (specifier.startsWith("@/")) return next(new URL("src/" + specifier.slice(2), repo).href, context);
    return next(specifier, context);
  }
});
const { translateRequest } = await import(new URL("open-sse/translator/index.js",repo));
const { DefaultExecutor } = await import(new URL("open-sse/executors/default.js",repo));
const { detectFormat } = await import(new URL("open-sse/services/provider.js",repo));
const { dedupeTools } = await import(new URL("open-sse/utils/toolDeduper.js",repo));
const { body, model, accept = "" } = JSON.parse(process.argv[2]);
if (detectFormat(body) !== "openai") {
  console.log("null");
  process.exit(0);
}
const stream = body.stream !== false && !(body.stream !== true && accept.includes("application/json") && !accept.includes("text/event-stream"));
const provider = "openai-compatible-chat-fixture";
const credentials = { apiKey: "offline-test", providerSpecificData: { baseUrl: "http://127.0.0.1:9/v1" } };
const result = translateRequest("openai", "openai", model, body, stream, credentials, provider);
delete result._toolNameMap;
delete result._customToolNames;
result.model = model;
for (const msg of result.messages) {
  delete msg.encrypted_content;
  delete msg.reasoning_encrypted_content;
}
if (Array.isArray(result.tools)) {
  const { tools, stripped } = dedupeTools(result.tools, { model, messages: result.messages });
  if (stripped.length) result.tools=tools;
}
console.log(JSON.stringify({ stream, body: new DefaultExecutor(provider).transformRequest(model,result,stream,credentials) }));
