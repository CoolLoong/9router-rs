import { registerHooks } from "node:module";

const mocked = "http://127.0.0.1:52989/codebuddy-quota/cn/v2/billing/meter/get-user-resource";
globalThis.fetchCodeBuddyFixture = (url, options) => {
  if (url !== "https://copilot.tencent.com/v2/billing/meter/get-user-resource") throw new Error("Unexpected CodeBuddy quota endpoint");
  return fetch(mocked, { ...options, redirect: "manual" });
};
registerHooks({
  resolve(specifier, context, next) {
    if (specifier.endsWith("/utils/proxyFetch.js")) {
      return next("data:text/javascript,export const proxyAwareFetch = globalThis.fetchCodeBuddyFixture;", context);
    }
    return next(specifier, context);
  },
});
const { getCodeBuddyCnUsage } = await import("../../open-sse/services/usage/codebuddy-cn.js");
const account = JSON.parse(process.argv[2]);
console.log(JSON.stringify(await getCodeBuddyCnUsage(account.accessToken, account.apiKey, account.providerSpecificData)));
