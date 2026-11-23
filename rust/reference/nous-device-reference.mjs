import { registerHooks } from "node:module";

const host = "https://portal.nousresearch.com";
const paths = new Set(["/api/oauth/device/code", "/api/oauth/token", "/api/oauth/agent-key"]);
const index = new URL("../../src/lib/oauth/providers/index.js", import.meta.url).href;
const dependencies = new URL("../../open-sse/", import.meta.url);
registerHooks({
  resolve(specifier, context, next) {
    if (specifier === "open-sse/index.js") {
      return next("data:text/javascript,export {};", context);
    }
    if (specifier.startsWith("open-sse/")) {
      return next(new URL(specifier.slice(9), dependencies).href, context);
    }
    if (context.parentURL === index && specifier === "../providerHelpers.js") {
      return next("data:text/javascript,export const extractCodexAccountInfo = null, fetchKiroProfileArn = null;", context);
    }
    if (context.parentURL === index && specifier.startsWith("./") && specifier !== "./nous.js") {
      return next("data:text/javascript,export default {};", context);
    }
    return next(specifier, context);
  },
});
const fetchLoopback = globalThis.fetch;
globalThis.fetch = (endpoint, options) => {
  const url = new URL(endpoint);
  if (url.origin !== host || !paths.has(url.pathname)) throw new Error("Unexpected provider endpoint");
  return fetchLoopback("http://127.0.0.1:52989/nous-device" + url.pathname.slice(10), {
    ...options, redirect: "manual",
  });
};
const { requestDeviceCode, pollForToken } = await import("../../src/lib/oauth/providers/index.js");
const { polls } = JSON.parse(process.argv[2]);
const device = await requestDeviceCode("nous");
const results = [];
for (let attempt = 0; attempt < polls; ++attempt) {
  const result = await pollForToken("nous", device.device_code, null);
  results.push(result.success
    ? { success: true, tokens: result.tokens }
    : { success: false, error: result.error, pending: result.pending || ["authorization_pending", "slow_down"].includes(result.error) });
}
delete device.device_code;
console.log(JSON.stringify({ device, results }));
