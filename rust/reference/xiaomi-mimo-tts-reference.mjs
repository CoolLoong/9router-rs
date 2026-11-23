import {Buffer} from "node:buffer";
import {registerHooks} from "node:module";

const project = new URL("../../", import.meta.url);
const input = JSON.parse(process.argv[2]);
globalThis.mimoFixtureAccount = { id: "mimo-tts-fixture", provider: "xiaomi-mimo", authType: "apikey", isActive: true,
  name: "MiMo Fixture", ...input.credentials };
registerHooks({
  resolve(specifier, context, next) {
    if (specifier === "@/lib/localDb" || specifier === "@/lib/localDb.js") {
      return next("data:text/javascript,export const getSettings=async()=>({requireApiKey:true}); export const getModelAliases=async()=>({}); export const getComboByName=async()=>null; export const getProviderNodes=async()=>[]; export const getProxyPools=async()=>[]; export const getProxyPoolById=async()=>null; export const getProviderConnections=async()=>[globalThis.mimoFixtureAccount]; export const validateApiKey=async(key)=>key==='offline-chat-key'; export const updateProviderConnection=async(_id,patch)=>Object.assign(globalThis.mimoFixtureAccount,patch);", context);
    }
    if (specifier === "@/models") {
      return next("data:text/javascript,export const getProxyPoolById=async()=>null;", context);
    }
    if (context.parentURL?.endsWith("/src/sse/handlers/tts.js") && specifier === "../services/keyAccess.js") {
      return next("data:text/javascript,export const getKeyAccessContext=async()=>null; export const enforceKeyAccess=async()=>null;", context);
    }
    if (context.parentURL?.endsWith("/src/sse/services/auth.js") && specifier === "./antigravityQuota.js") {
      return next("data:text/javascript,export const getAntigravityQuotaCache=()=>new Map(); export const getAntigravityModelQuota=()=>null;", context);
    }
    if (specifier === "../utils/logger.js") {
      return next("data:text/javascript,const noop=()=>{}; export {noop as request,noop as info,noop as warn,noop as debug,noop as error};", context);
    }
    if (specifier.startsWith("open-sse/")) return next(new URL(specifier, project).href, context);
    if (specifier.startsWith("@/")) {
      const name = specifier.slice(2);
      return next(new URL("src/" + name + (name.endsWith(".js") ? "" : ".js"), project).href, context);
    }
    return next(specifier, context);
  },
});
const fetchLoopback = globalThis.fetch;
globalThis.fetch = (url, options) => {
  if (url !== "https://api.xiaomimimo.com/v1/chat/completions") throw new Error("Unexpected MiMo audio endpoint");
  return fetchLoopback("http://127.0.0.1:52989/xiaomi-mimo/v1/chat/completions", { ...options, redirect: "manual" });
};
const {handleTtsCore} = await import("../../open-sse/handlers/ttsCore.js");
if (input.unregistered) {
  const {handleSttCore} = await import("../../open-sse/handlers/sttCore.js");
  const {AI_PROVIDERS} = await import("../../src/shared/constants/providers.js");
  const provider = "xiaomi-tokenplan";
  const stt = await handleSttCore({ provider, model: "mimo-v2.5-asr", sttConfig: AI_PROVIDERS[provider].sttConfig,
    formData: {get: () => new Blob(["offline audio"])}, credentials: {} });
  const tts = await handleTtsCore({ provider, model: "mimo-v2.5-tts", input: "offline text", credentials: {} });
  console.log(JSON.stringify({ stt: {status: stt.status, error: stt.error}, tts: {status: tts.status, error: tts.error} }));
} else if (input.core) {
  const result = await handleTtsCore({ ...input, provider: "xiaomi-mimo" });
  console.log(JSON.stringify({ status: result.status, error: result.error }));
} else {
  console.error = () => {};
  const {handleTts} = await import("../../src/sse/handlers/tts.js");
  const query = input.responseFormat === "json" ? "?response_format=json" : "";
  const request = new Request(`http://127.0.0.1/v1/audio/speech${query}`, {method: "POST",
    headers: {"content-type": "application/json", Authorization: "Bearer offline-chat-key"},
    body: JSON.stringify({model: input.alias + "/" + input.model, input: input.input, language: input.language,
      style: input.style, response_format: query ? "mp3" : input.responseFormat}) });
  const reply = await handleTts(request);
  const result = !reply.ok ? {status: reply.status, error: (await reply.json()).error.message}
    : {status: reply.status, mime: reply.headers.get("content-type"), cors: reply.headers.get("access-control-allow-origin"),
        body: Buffer.from(await reply.arrayBuffer()).toString("base64")};
  console.log(JSON.stringify(result));
}
