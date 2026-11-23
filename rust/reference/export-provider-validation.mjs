import { readFileSync } from "node:fs";
import "./provider-modules.mjs";
const { PROVIDERS } = await import("../../open-sse/config/providers.js");
const { getDefaultModel } = await import("../../open-sse/config/providerModels.js");
const { AI_PROVIDERS } = await import("../../src/shared/constants/providers.js");
const route = readFileSync(new URL("../../src/app/api/providers/validate/route.js", import.meta.url), "utf8");
const cases = new Set([...route.split("switch (provider) {")[1].matchAll(/case "([^"]+)":/g)].map(match => match[1]));
for (const id of ["cloudflare-ai", "azure"]) cases.add(id);
const ids = [
  "cloudflare-ai", "deepseek", "groq", "xai", "mistral", "perplexity", "together",
  "fireworks", "cerebras", "cohere", "nebius", "siliconflow", "hyperbolic", "ollama",
  "assemblyai", "nanobanana", "chutes", "runcrate", "synthetic", "xiaomi-mimo",
  "xiaomi-tokenplan", "nvidia", "opencode-go",
];
const result = Object.fromEntries(ids.map(id => {
  const config = PROVIDERS[id] || {};
  return [id, {
    validateUrl: config.validateUrl, defaultModel: getDefaultModel(id),
    regions: config.regions, defaultRegion: config.defaultRegion,
  }];
}));
const webKinds = new Set(["webSearch", "webFetch"]);
const mediaKinds = new Set(["tts", "embedding", "stt", "image", "video", "music", "imageToText"]);
const mediaAuth = new Set(["bearer", "key", "x-api-key", "x-key", "xi-api-key", "token", "basic"]);
for (const [id, provider] of Object.entries(AI_PROVIDERS)) {
  const kinds = provider.serviceKinds || ["llm"];
  let probe;
  if (kinds.every(kind => webKinds.has(kind))) {
    const config = provider.searchConfig || provider.fetchConfig;
    if (config?.authType === "none") probe = { accept: true };
    else if (config) probe = {
      kind: "web", url: config.validateUrl || config.baseUrl, method: config.method || "GET",
      auth: config.authHeader,
    };
  } else if (kinds.every(kind => mediaKinds.has(kind))) {
    const config = provider.ttsConfig || provider.sttConfig || provider.embeddingConfig
      || provider.imageConfig || provider.videoConfig || provider.musicConfig;
    if (!config || provider.noAuth || config.authType === "none"
      || ["playht", "aws-sigv4"].includes(config.authHeader)) probe = { accept: true };
    else if (mediaAuth.has(config.authHeader)) probe = {
      kind: "media", url: config.baseUrl, method: config.method || "POST", auth: config.authHeader,
      headers: config.extraHeaders, model: getDefaultModel(id) || "test",
    };
  }
  if (probe) {
    (result[id] ??= {}).metadataProbe = probe;
  }
  const transport = PROVIDERS[id];
  if (!probe && !cases.has(id) && provider.credentialForm !== "aws"
    && transport?.format === "openai" && typeof transport.baseUrl === "string") {
    (result[id] ??= {}).defaultProbe = {
      url: transport.baseUrl, noAuth: transport.noAuth === true,
      headers: transport.headers, auth: transport.authHeader, model: getDefaultModel(id) || "test",
    };
  }
}
process.stdout.write(JSON.stringify(result, null, 2) + "\n");
