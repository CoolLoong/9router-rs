import { registerHooks } from "node:module";

const project = new URL("../../", import.meta.url);
registerHooks({
  resolve(specifier, context, next) {
    if (specifier.startsWith("open-sse/")) return next(new URL(specifier, project).href, context);
    return next(specifier, context);
  },
});
const fetchLoopback = globalThis.fetch;
globalThis.fetch = (url, options) => {
  const routes = {
    "https://auth.meta.com/oidc/device/authorization/": "authorization",
    "https://auth.meta.com/oidc/device/token/": "token",
    "https://api.meta.ai/muse-code/key": "key",
  };
  if (!routes[url]) throw new Error("Unexpected Muse issuer endpoint");
  return fetchLoopback(`http://127.0.0.1:52989/muse-device/${routes[url]}`, { ...options, redirect: "manual" });
};
const { default: provider } = await import("../../src/lib/oauth/providers/muse.js");
const { polls } = JSON.parse(process.argv[2]);
const device = await provider.requestDeviceCode(provider.config);
const results = [];
for (let index = 0; index < polls; ++index) {
  const { ok, data } = await provider.pollToken(provider.config, device.device_code);
  if (ok && data.access_token) {
    try {
      const minted = await provider.postExchange(data);
      results.push({ success: true, fields: provider.mapTokens(data, minted) });
    } catch (error) {
      results.push({ success: false, error: "exchange_failed", errorDescription: error.message, pending: false, fatal: true });
    }
  } else {
    const pending = ["authorization_pending", "slow_down"].includes(data.error);
    results.push({ success: false, error: data.error, pending });
  }
}
delete device.device_code;
console.log(JSON.stringify({ device, results }));
