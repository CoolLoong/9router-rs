import { registerHooks } from "node:module";

registerHooks({
  resolve(specifier, context, next) {
    return next(
      specifier.startsWith("open-sse/") ? new URL(`../../${specifier}`, import.meta.url).href : specifier,
      context,
    );
  },
});
const { KiroService } = await import("../../src/lib/oauth/services/kiro.js");
const { resolveKiroModels } = await import("../../open-sse/services/kiroModels.js");
const service = new KiroService();

const base = process.argv[2];
const localFetch = globalThis.fetch;
const buffers = [];
for await (const data of process.stdin) buffers.push(data);
const accounts = JSON.parse(Buffer.concat(buffers));
const result = [];

for (const account of accounts) {
  const details = account.providerSpecificData;
  const token = (account.accessToken || account.apiKey).trim();
  const profile = account.authType === "oauth";
  const requestedRegion = details.region || "us-east-1";
  let capture;
  globalThis.fetch = async (input, init) => {
    const address = new URL(input);
    const headers = new Headers(init?.headers);
    if (profile && address.origin === "https://codewhisperer.us-east-1.amazonaws.com") {
      const response = await localFetch(base + "/codewhisperer/us-east-1", { ...init, redirect: "manual" });
      capture = response.clone();
      return response;
    }
    if (
      !profile && address.origin === `https://q.${requestedRegion}.amazonaws.com`
      && address.pathname === "/ListAvailableModels" && headers.get("TokenType") === "API_KEY"
    ) {
      const response = await localFetch(base + `/q/${requestedRegion}` + address.pathname + address.search, {
        ...init, redirect: "manual",
      });
      capture = response.clone();
      return response;
    }
    if (
      address.pathname === "/ListAvailableModels" && address.protocol === "https:"
      && /^q\.[a-z]{2}-[a-z]+-[0-9]{1,2}\.amazonaws\.com$/.test(address.hostname)
    ) {
      try {
        if (profile) {
          await service.listAvailableModels(token, details.profileArn);
        } else {
          await service.listAvailableApiKeyModels(token, requestedRegion);
        }
      } catch {}
      return capture || new Response(null, {status: 502});
    }
    throw new Error("Only the mocked standard Kiro endpoint is available");
  };
  const credentials = { ...account, accessToken: token };
  const discovered = await resolveKiroModels(credentials);
  result.push({ eligible: true, models: discovered?.models ?? null });
}
console.log(JSON.stringify(result));
