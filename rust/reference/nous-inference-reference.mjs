import { registerHooks } from "node:module";

const job = JSON.parse(process.argv[2]);
const project = new URL("../../", import.meta.url);
registerHooks({
  resolve(specifier, context, next) {
    if (specifier.endsWith("/utils/proxyFetch.js")) {
      return next("data:text/javascript,export const proxyAwareFetch = (...args) => fetch(...args);", context);
    }
    if (job.management) {
      if (specifier === "@/lib/localDb") {
        return next("data:text/javascript,export const getProviderConnectionById = async () => globalThis.fixtureAccount; export const updateProviderConnection = async (id, patch) => Object.assign(globalThis.fixtureAccount, patch);", context);
      }
      if (specifier === "@/lib/network/connectionProxy") {
        return next("data:text/javascript,export const resolveConnectionProxyConfig = async () => ({});", context);
      }
      if (specifier === "@/lib/network/proxyTest" || specifier === "open-sse/executors/bedrock.js") {
        return next("data:text/javascript,export const testProxyUrl = null, probeBedrockCredentials = null;", context);
      }
      if (specifier.startsWith("@/")) {
        const name = specifier.slice(2);
        return next(new URL("src/" + name + (name.endsWith(".js") ? "" : ".js"), project).href, context);
      }
      if (specifier.startsWith("open-sse/")) {
        return next(new URL(specifier, project).href, context);
      }
    }
    return next(specifier, context);
  },
});
const { refreshNousToken } = await import("../../open-sse/services/tokenRefresh/providers.js");
const { DefaultExecutor } = await import("../../open-sse/executors/default.js");
const fetchLoopback = globalThis.fetch;
globalThis.fetch = (endpoint, options) => {
  const destinations = {
    "https://portal.nousresearch.com/api/oauth/token": "/nous-device/token",
    "https://portal.nousresearch.com/api/oauth/agent-key": "/nous-device/agent-key",
    "https://inference-api.nousresearch.com/v1/chat/completions": job.management
      ? "/nous-management/chat/completions" : "/static-chat/nous",
  };
  if (!destinations[endpoint]) throw new Error("Unexpected provider endpoint");
  return fetchLoopback("http://127.0.0.1:52989" + destinations[endpoint], { ...options, redirect: "manual" });
};
const { credentials, body, refresh } = job;
if (job.management) {
  globalThis.fixtureAccount = { ...credentials, provider: "nous", id: "nous_fixture" };
  for (const level of ["log", "info", "warn", "error"]) console[level] = () => {};
  const { testSingleConnection } = await import("../../src/app/api/providers/[id]/test/testUtils.js");
  const { valid, error, refreshed } = await testSingleConnection("nous_fixture");
  process.stdout.write(JSON.stringify({ valid, error, refreshed }) + "\n");
} else {
  let rotated = null;
  if (refresh) {
    rotated = await refreshNousToken(credentials.refreshToken);
    Object.assign(credentials, rotated);
  }
  const executor = new DefaultExecutor("nous");
  const url = executor.buildUrl(body.model, body.stream, 0, credentials);
  const prepared = executor.transformRequest(body.model, body, body.stream, credentials);
  const response = await fetch(url, {
    method: "POST", headers: executor.buildHeaders(credentials, body.stream, url, body.model, prepared),
    body: JSON.stringify(prepared),
  });
  const raw = await response.text();
  const content = body.stream
    ? raw.split("\n").filter(line => line.startsWith("data: {")).map(line => JSON.parse(line.slice(6)).choices[0]?.delta?.content || "").join("")
    : JSON.parse(raw).choices[0].message.content;
  console.log(JSON.stringify({ status: response.status, content, rotated }));
}
