import { registerHooks } from "node:module";

const project = new URL("../../", import.meta.url);
registerHooks({
  resolve(specifier, context, next) {
    if (specifier.startsWith("open-sse/")) return next(new URL(specifier, project).href, context);
    return next(specifier, context);
  },
});
const fetchLoopback = globalThis.fetch;
globalThis.fetch = (endpoint, options) => {
  const url = new URL(endpoint);
  if (url.origin !== "https://copilot.tencent.com" || !["/v2/plugin/auth/state", "/v2/plugin/auth/token"].includes(url.pathname)) {
    throw new Error("Unexpected CodeBuddy issuer endpoint");
  }
  return fetchLoopback(`http://127.0.0.1:52989/codebuddy-device/cn${url.pathname}${url.search}`, { ...options, redirect: "manual" });
};
const { default: provider } = await import("../../src/lib/oauth/providers/codebuddy-cn.js");
const { polls } = JSON.parse(process.argv[2]);
const device = await provider.requestDeviceCode(provider.config);
const results = [];
for (let index = 0; index < polls; ++index) {
  const { ok, data } = await provider.pollToken(provider.config, device.device_code);
  if (ok && data.access_token) {
    results.push({ success: true, fields: provider.mapTokens(data) });
  } else {
    results.push({ success: false, error: data.error, pending: data.error === "authorization_pending" });
  }
}
delete device.device_code;
console.log(JSON.stringify({ device, results }));
