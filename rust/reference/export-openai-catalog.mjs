import "./provider-modules.mjs";
import { registerHooks } from "node:module";

const levelSource = new URL("../../open-sse/providers/thinkingLevels.js", import.meta.url).href;
const hook = registerHooks({
  load(url, options, nextLoad) {
    const result = nextLoad(url, options);
    if (url !== levelSource || !result.source) return result;
    const source = typeof result.source === "string" ? result.source : new TextDecoder().decode(result.source);
    return {
      ...result,
      source: source + "\nexport { FORMAT_LEVELS, PATTERN_THINKING, CODEX_GPT_5_6_LEVELS, L };",
    };
  },
});
const [{ PROVIDER_MODELS, PROVIDER_ID_TO_ALIAS }, { AI_PROVIDERS, ALIAS_TO_ID }, thinking] = await Promise.all([
  import("../../open-sse/config/providerModels.js"),
  import("../../src/shared/constants/providers.js"),
  import(levelSource),
]);
hook.deregister();

const models = Object.fromEntries(Object.entries(PROVIDER_MODELS).map(([alias, entries]) => [
  alias,
  alias === "gcli" ? entries : entries.map(({ id, name, kind, type, thinkingLevels, params, capabilities, options, dimensions, contextWindow }) =>
    ({ id, name, kind, type, thinkingLevels, params, capabilities, options, dimensions, contextWindow })),
]));
process.stdout.write(JSON.stringify({
  models,
  transportAlias: PROVIDER_ID_TO_ALIAS,
  providerByAlias: { ...Object.fromEntries(Object.entries(PROVIDER_ID_TO_ALIAS).map(([id, alias]) => [alias, id])), ...ALIAS_TO_ID },
  infoProviderByAlias: ALIAS_TO_ID,
  searchModelInfo: Object.fromEntries(Object.values(AI_PROVIDERS).filter(p => p.searchConfig).map(p => {
    const { searchTypes, maxMaxResults, requiredOptions } = p.searchConfig;
    return [p.id, { searchTypes, maxResults: maxMaxResults, required: requiredOptions }];
  })),
  providersWithSearch: Object.values(AI_PROVIDERS).filter((p) => p.searchConfig).map((p) => p.id),
  providersWithFetch: Object.values(AI_PROVIDERS).filter((p) => p.fetchConfig).map((p) => p.id),
  thinking: {
    defaults: thinking.L.base, formats: thinking.FORMAT_LEVELS,
    patterns: thinking.PATTERN_THINKING, compatibleMax: thinking.CODEX_GPT_5_6_LEVELS,
  },
}) + "\n");
