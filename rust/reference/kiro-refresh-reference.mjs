import { registerHooks } from "node:module";

const fixture = "http://127.0.0.1:52989/kiro-refresh/";
const allowed = new Set([
  "oidc.us-east-1.amazonaws.com", "oidc.eu-central-1.amazonaws.com",
  "prod.us-east-1.auth.desktop.kiro.dev", "login.microsoftonline.com",
]);
globalThis.fixtureKiroRefresh = (endpoint, options) => {
  const url = new URL(endpoint);
  if (url.protocol !== "https:" || !allowed.has(url.hostname)) throw new Error("Only loopback fixture requests are allowed.");
  return fetch(fixture + url.hostname + url.pathname + url.search, { ...options, redirect: "manual" });
};
registerHooks({
  resolve(specifier, context, next) {
    if (specifier.endsWith("/proxyFetch.js") && context.parentURL?.endsWith("/tokenRefresh/providers.js")) {
      return next("data:text/javascript,export const proxyAwareFetch = globalThis.fixtureKiroRefresh;", context);
    }
    return next(specifier, context);
  },
});
const { refreshKiroToken } = await import("../../open-sse/services/tokenRefresh/providers.js");
const chunks = [];
for await (const bytes of process.stdin) chunks.push(bytes);
const row = JSON.parse(Buffer.concat(chunks));
const result = await refreshKiroToken(row.refreshToken, row.providerSpecificData);
console.log(JSON.stringify({
  accepted: result?.accessToken === "offline-kiro-refresh-after",
  rotated: result?.refreshToken === "offline-kiro-refresh-rotated",
  ttl: result?.expiresIn,
  preservedProfile: (result?.providerSpecificData?.profileArn || row.providerSpecificData.profileArn)
    === row.providerSpecificData.profileArn,
}));
