import { readFileSync } from "node:fs";
import "./provider-modules.mjs";

const { default: registry } = await import("../../open-sse/providers/registry/index.js");
const { PROVIDERS, PROVIDER_MODELS } = await import("../../open-sse/providers/index.js");
const source = readFileSync(new URL("../../open-sse/executors/index.js", import.meta.url), "utf8");
const packageVersion = JSON.parse(readFileSync(new URL("../../package.json", import.meta.url), "utf8")).version;
const specialized = new Set([...source.matchAll(/^\s*(?:"([^"]+)"|([\w-]+)):\s+new\s+\w+/gm)].map(match => match[1] || match[2]));
const permittedHeaders = new Set(["http-referer", "x-title", "accept", "user-agent", "content-type"]);
const minimaxCode = new Set(["minimax-code", "minimax-code-global"]);
const codeBuddy = new Set(["codebuddy-cn", "codebuddy-intl"]);
const codeBuddyHeaders = new Set(["x-product", "x-ide-type", "x-ide-name", "x-requested-with", "x-codebuddy-request"]);
const messagesProviders = new Set(["deepseek", "glm", "kimi", "minimax", "minimax-cn", ...minimaxCode, "xiaomi-tokenplan", "xiaomi-mimo"]);
const messagesHeaders = new Set(["anthropic-version", "anthropic-beta"]);
const describe = (transport = {}, common = {}, allowMessages = false, allowedHook) => {
  const { baseUrl, urlSuffix, format = "openai", auth, headers = {} } = transport;
  const messages = format === "claude" && allowMessages;
  const credential = auth?.combined ? auth : auth?.apiKey || (!auth ? { header: "Authorization", scheme: "bearer" } : null);
  if (!baseUrl?.startsWith("https://") || transport.baseUrls?.length || auth?.hooks?.some(hook => hook !== allowedHook) ||
    !credential?.header || !["bearer", "raw"].includes(credential.scheme) ||
    Object.keys(headers).some(name => !permittedHeaders.has(name.toLowerCase()) &&
      !(allowedHook === "minimaxHeaders" && name.toLowerCase() === "x-mavis-agent-id") &&
      !(allowedHook === "codebuddyHeaders" && codeBuddyHeaders.has(name.toLowerCase())) &&
      !(messages && messagesHeaders.has(name.toLowerCase()))) ||
    !messages && !["openai", "openai-responses", "responses"].includes(format)) return null;
  const { claudeSupportedToolTypes, dropOutputConfig, requireClaudeToolType } = common.quirks || {};
  return {
    url: baseUrl + (urlSuffix || ""), format, headers, authHeader: credential.header,
    bearer: credential.scheme === "bearer", combinedAuth: !auth || auth.combined === true,
    authHook: auth?.hooks?.[0],
    timeoutMs: common.timeoutMs, forceStream: common.forceStream === true,
    thinkingFormat: transport.thinkingFormat || common.thinkingFormat,
    reasoningScope: common.reasoningInject?.scope, dropClientMetadata: common.quirks?.dropClientMetadata === true,
    preserveCacheControl: common.quirks?.preserveCacheControl === true ? true : undefined,
    claudeSupportedToolTypes: messages ? claudeSupportedToolTypes : undefined,
    dropOutputConfig: messages && dropOutputConfig ? true : undefined,
    requireClaudeToolType: messages && requireClaudeToolType ? true : undefined,
  };
};
const result = {};
for (const entry of registry) {
  const { id, alias = id } = entry;
  const transport = PROVIDERS[id];
  if (!transport || specialized.has(id) && !minimaxCode.has(id) && !codeBuddy.has(id) && !["kimchi", "xiaomi-tokenplan", "xiaomi-mimo"].includes(id) || entry.noAuth ||
    !minimaxCode.has(id) && !["apikey", "freeTier"].includes(entry.category) && !entry.authModes?.includes("apikey") ||
    entry.serviceKinds && !entry.serviceKinds.includes("llm")) continue;
  const allowMessages = messagesProviders.has(id);
  const allowedHook = minimaxCode.has(id) ? "minimaxHeaders" : codeBuddy.has(id) ? "codebuddyHeaders" : { kimi: "kimiHeaders", muse: "museHeaders" }[id];
  const describeTransport = item => {
    if (id !== "xiaomi-tokenplan") return describe(item, transport, allowMessages, allowedHook);
    const regionalUrls = Object.fromEntries(Object.entries(transport.regions).map(([region, root]) => [
      region, item.format === "claude" ? `${root.replace(/\/v1\/?$/, "")}/anthropic/v1/messages` : `${root}/chat/completions`,
    ]));
    const descriptor = describe({...item, baseUrl:regionalUrls[transport.defaultRegion]}, transport, allowMessages);
    return descriptor && {...descriptor, regionalUrls};
  };
  const primary = describeTransport(transport);
  if (id === "kimchi" && primary) primary.headers = { "User-Agent": `9Router/${packageVersion}` };
  const overrides = Object.fromEntries((transport.transports || []).map(item => [item.format, describeTransport(item)]));
  const supportedQuirks = Object.keys(transport.quirks || {}).every(key =>
    key === "dropClientMetadata" || key === "preserveCacheControl" ||
    key === "reasoningAliasFields" && ["sensenova", "sensenova-intl"].includes(id) || allowMessages &&
      ["claudeSupportedToolTypes", "dropOutputConfig", "requireClaudeToolType"].includes(key));
  if (!supportedQuirks || !primary && !Object.values(overrides).some(Boolean)) continue;
  const models = Object.fromEntries((PROVIDER_MODELS[alias] || [])
    .filter(model => model.strip?.length || model.targetFormat || model.supportedFormats?.length ||
      model.upstreamModelId || ["tts", "stt"].includes(model.kind || model.type))
    .map(model => [model.id, {
      strip: model.strip, to: model.targetFormat, supported: model.supportedFormats,
      upstream: model.upstreamModelId, kind: model.kind || model.type,
    }]));
  result[id] = { primary, overrides, models };
}
process.stdout.write(JSON.stringify(result, null, 2) + "\n");
