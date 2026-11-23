import fs from "node:fs";
import { registerHooks } from "node:module";

const upstream = "https://grok.com/grok_api_v2.GrokBuildBilling/GetGrokCreditsConfig";
registerHooks({
  resolve(specifier, context, next) {
    if (context.parentURL?.endsWith("/services/usage/grok-cli.js") && specifier.endsWith("/proxyFetch.js")) {
      return next("data:text/javascript,export const proxyAwareFetch = (...args) => globalThis.fixtureFetch(...args);", context);
    }
    return next(specifier, context);
  },
});

globalThis.fixtureFetch = (url, options) => {
  if (url !== upstream) throw new Error("Unexpected Grok weekly usage route");
  return fetch("http://127.0.0.1:52989/grok-build-weekly", { ...options, redirect: "error" });
};
const { fetchGrokCliCreditsConfig } = await import("../../open-sse/services/usage/grok-cli.js");
const data = await fetchGrokCliCreditsConfig(JSON.parse(fs.readFileSync(0, "utf8")));
const used = data && Math.round(Math.max(0, Math.min(100, data.percentUsed)));
const quota = data ? { used, total: 100, remainingPercentage: (100 - used) / 100 * 100,
  resetAt: data.resetAt, unlimited: false } : null;
console.log(JSON.stringify({ quota }));
