import fs from "node:fs";
import { registerHooks } from "node:module";
import { fileURLToPath } from "node:url";

const fixture=JSON.parse(fs.readFileSync(0,"utf8"));
const localFetch=globalThis.fetch;
registerHooks({
  resolve(specifier,context,next) {
    if (specifier==="node-machine-id") {
      return next("data:text/javascript,export const machineIdSync = () => 'fixture-machine';",context);
    }
    if (!specifier.startsWith("@/") && !specifier.startsWith("open-sse/")) return next(specifier,context);
    const relative=specifier.startsWith("@/") ? `src/${specifier.slice(2)}` : specifier;
    const target=new URL(`../../${relative}`,import.meta.url);
    const info=fs.statSync(fileURLToPath(target),{throwIfNoEntry:false});
    const url=info?.isDirectory() ? new URL(`${target.href}/index.js`)
      : info ? target : new URL(`${target.href}.js`);
    return next(url.href,context);
  },
});
function transport(url,options) {
  const requested=new URL(url);
  if (requested.origin!==fixture.vendor) throw new Error("Fixture declined a non-loopback request");
  return localFetch(fixture.loopback+requested.pathname+requested.search,options);
}
globalThis.fetch=transport;
const { handleTtsCore }=await import("../../open-sse/handlers/ttsCore.js");
const { PROVIDER_MEDIA }=await import("../../open-sse/providers/index.js");
const { FORMAT_HANDLERS }=await import("../../open-sse/handlers/ttsProviders/genericFormats.js");
const { parseModelVoice }=await import("../../open-sse/handlers/ttsProviders/_base.js");
globalThis.fetch=transport;

let result;
if (fixture.mode==="registry") {
  result={supported:Boolean(PROVIDER_MEDIA[fixture.provider]?.ttsConfig)};
} else if (fixture.provider==="deepgram") {
  try {
    const {modelId,voiceId}=parseModelVoice(fixture.model);
    const data=await FORMAT_HANDLERS.deepgram({
      baseUrl:fixture.vendor+"/v1/speak",apiKey:fixture.apiKey,text:fixture.input.trim(),modelId,voiceId,
    });
    result={status:200,format:data.format,bytes:Buffer.from(data.base64,"base64").toString("base64")};
    if (fixture.responseFormat==="json") result.encoded=data.base64;
  } catch (error) {
    result={status:502,error:error.message};
  }
} else {
  const data=await handleTtsCore({
    provider:fixture.provider,model:fixture.model,input:fixture.input,
    credentials:{apiKey:fixture.apiKey},responseFormat:fixture.responseFormat,language:fixture.language,style:fixture.style,
  });
  if (!data.success) {
    result={status:data.status,error:data.error};
  } else if (fixture.responseFormat==="json") {
    const body=await data.response.json();
    result={status:data.response.status,format:body.format,encoded:body.audio,bytes:Buffer.from(body.audio,"base64").toString("base64")};
  } else {
    result={status:data.response.status,format:data.response.headers.get("content-type").slice(6),
      bytes:Buffer.from(await data.response.arrayBuffer()).toString("base64")};
  }
}
console.log(JSON.stringify(result));
process.exit(0);
