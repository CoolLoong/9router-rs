import { createHash } from "node:crypto";
import { registerHooks } from "node:module";

const providerId = process.argv[2];
const cn = providerId === "qoder-cn";
const source = new URL("../../", import.meta.url);
const originalHost = cn ? "https://openapi.qoder.com.cn" : "https://openapi.qoder.sh";
const local = `http://127.0.0.1:52989/qoder-device/${providerId}`;
registerHooks({
  resolve(specifier, context, next) {
    if (specifier === "@/lib/oauth/services/qoder") {
      return next(new URL("src/lib/oauth/services/qoder.js", source).href, context);
    }
    if (specifier.startsWith("open-sse/")) {
      return next(new URL(specifier, source).href, context);
    }
    return next(specifier, context);
  },
});
const fetchLoopback = globalThis.fetch;
globalThis.fetch = (endpoint, options) => {
  const url = new URL(endpoint);
  if (url.origin !== originalHost || !["/api/v1/deviceToken/poll", "/api/v1/userinfo"].includes(url.pathname)) {
    throw new Error("Unexpected provider endpoint");
  }
  return fetchLoopback(local + url.pathname + url.search, { ...options, redirect: "manual" });
};
const { default: provider } = await import(new URL(`src/lib/oauth/providers/${providerId}.js`, source).href);
const code = await provider.requestDeviceCode(provider.config);
const location = new URL(code.verification_uri_complete);
const challenge = createHash("sha256").update(code.codeVerifier).digest("base64url");
if (challenge !== location.searchParams.get("challenge")) throw new Error("Unexpected PKCE challenge");
await fetchLoopback(local + "/fixture-registration", { method: "POST", body: JSON.stringify(code) });
const results = [];
for (let i = 0; i < 2; ++i) {
  const result = await provider.pollToken(provider.config, code.device_code, code.codeVerifier, code);
  results.push(result.ok
    ? { success: true, fields: provider.mapTokens(result.data) }
    : { success: false, error: result.data.error, pending: result.data.error === "authorization_pending" });
}
console.log(JSON.stringify({ code, results }));
