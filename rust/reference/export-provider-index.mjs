import "./provider-modules.mjs";
const {
  AI_PROVIDERS, APIKEY_PROVIDERS, FREE_PROVIDERS, FREE_TIER_PROVIDERS, WEB_COOKIE_PROVIDERS,
  OAUTH_PROVIDERS, USAGE_SUPPORTED_PROVIDERS, USAGE_APIKEY_PROVIDERS,
} = await import("../../src/shared/constants/providers.js");

const entries = Object.values(AI_PROVIDERS).map((p) => ({
  id: p.id,
  alias: p.alias,
  name: p.name,
  authType: p.authType,
  authGroup: FREE_PROVIDERS[p.id] ? "free" : OAUTH_PROVIDERS[p.id] ? "oauth"
    : APIKEY_PROVIDERS[p.id] ? "apikey" : undefined,
  canCreateWithKey: !!(APIKEY_PROVIDERS[p.id] || FREE_TIER_PROVIDERS[p.id] ||
    WEB_COOKIE_PROVIDERS[p.id] || p.authModes?.includes("apikey")),
  webCookie: !!WEB_COOKIE_PROVIDERS[p.id],
  noAuth: p.noAuth === true || undefined,
  noAuthFree: !!FREE_PROVIDERS[p.id]?.noAuth,
  apiKeyOptionalWith: p.apiKeyOptionalWith,
  credentialForm: p.credentialForm,
  serviceKinds: p.serviceKinds,
  hidden: p.hidden,
  hiddenKinds: p.hiddenKinds,
  passthroughModels: p.passthroughModels,
  modelsFetcher: p.modelsFetcher ? { url: p.modelsFetcher.url, type: p.modelsFetcher.type } : undefined,
  usageOrder: USAGE_SUPPORTED_PROVIDERS.includes(p.id)
    ? USAGE_SUPPORTED_PROVIDERS.indexOf(p.id) : undefined,
  usageApiKey: USAGE_APIKEY_PROVIDERS.includes(p.id) || undefined,
}));
process.stdout.write(JSON.stringify(entries, null, 2) + "\n");
