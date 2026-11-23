const originalFetch = globalThis.fetch;
const known = new Set([
  "https://openrouter.ai/api/v1/models",
  "https://api.kilo.ai/api/gateway/models",
  "https://opencode.ai/zen/v1/models",
  "https://opencode.ai/inference/go/v1/models",
  "https://models.dev/api.json",
  "https://agent.minimax.cn/mavis/api/v1/models?region=cn&buildEnv=prod",
  "https://agent.minimax.io/mavis/api/v1/models?region=en&buildEnv=prod",
  "https://api.airforce/v1/models",
  "https://inference-api.nousresearch.com/v1/models",
]);

globalThis.fetch = (input, ...options) => {
  const url = new URL(typeof input === "string" ? input : input.url ?? input);
  if (!known.has(url.href)) return Promise.reject(new Error("Unexpected catalog fixture destination"));
  return originalFetch(`http://127.0.0.1:52989/catalog-suggestions/${url.host}${url.pathname}${url.search}`, ...options);
};
