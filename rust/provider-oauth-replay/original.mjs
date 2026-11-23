import http from "node:http";
import fs from "node:fs";
import { registerHooks } from "node:module";
import { fileURLToPath } from "node:url";

const [root, base] = process.argv.slice(2);
process.env.DATA_DIR = root;
registerHooks({
  resolve(specifier, context, next) {
    if (specifier === "node-machine-id") {
      return next("data:text/javascript,export const machineIdSync = () => 'fixture-machine';", context);
    }
    if (context.parentURL?.endsWith("/open-sse/executors/github.js") && specifier.endsWith("/proxyFetch.js")) {
      return next("data:text/javascript,export const proxyAwareFetch = (url, args) => globalThis.fixtureGitHubRefresh(url, args);", context);
    }
    if (!specifier.startsWith("@/") && !specifier.startsWith("open-sse/")) return next(specifier, context);
    const relative = specifier.startsWith("@/") ? `src/${specifier.slice(2)}` : specifier;
    const target = new URL(`../../${relative}`, import.meta.url);
    const url = fs.existsSync(fileURLToPath(target)) ? target : new URL(`${target.href}.js`);
    return next(url.href, context);
  },
});
const localFetch = globalThis.fetch;
const fetchFixture = (input, args) => {
  let requested = new URL(input);
  if (requested.origin === "https://cloudcode-pa.googleapis.com" && requested.pathname === "/v1internal:loadCodeAssist") {
    requested = new URL(`${base}/google/project`);
  }
  const transports = {
    "https://auth.openai.com/oauth/token": ["codex", "token"],
    "https://api.anthropic.com/v1/oauth/token": ["claude", "token"],
    "https://oauth2.googleapis.com/token": ["google", "token"],
    "https://github.com/login/oauth/access_token": ["github", "poll"],
    "https://api.github.com/copilot_internal/v2/token": ["github", "copilot"],
  };
  if (transports[requested.href]) {
    const [provider, action] = transports[requested.href];
    requested = new URL(`${base}/${provider}/${action}`);
    if (args?.body instanceof URLSearchParams) {
      const data = new URLSearchParams(args.body);
      data.set("client_id", `fixture-${provider}-client`);
      if (data.has("client_secret")) data.set("client_secret", "fixture-google-secret");
      args = { ...args, body: data };
    } else if (typeof args?.body === "string") {
      const data = JSON.parse(args.body);
      data.client_id = `fixture-${provider}-client`;
      args = { ...args, body: JSON.stringify(data) };
    }
  }
  if (requested.origin !== base) throw new Error("Fixture attempted non-loopback HTTP");
  return localFetch(requested, args);
};
globalThis.fetch = fetchFixture;
globalThis.fixtureGitHubRefresh = fetchFixture;
const controls = await import("../../src/lib/oauth/providers/index.js");
const { createProviderConnection, getProviderConnectionById, updateProviderConnection } =
  await import("../../src/lib/db/repos/connectionsRepo.js");
const { getAdapter } = await import("../../src/lib/db/driver.js");
const tokenRefresh = await import("../../open-sse/services/tokenRefresh.js");
const manager = await import("../../open-sse/services/oauthCredentialManager.js");
const { GithubExecutor } = await import("../../open-sse/executors/github.js");
await getAdapter();
globalThis.fetch = fetchFixture;
const claude = controls.getProvider("claude").config;
Object.assign(claude, { clientId: "fixture-claude-client", authorizeUrl: `${base}/claude/authorize`, tokenUrl: `${base}/claude/token` });
const codex = controls.getProvider("codex").config;
Object.assign(codex, { clientId: "fixture-codex-client", authorizeUrl: `${base}/codex/authorize`, tokenUrl: `${base}/codex/token` });
const google = controls.getProvider("gemini-cli").config;
const github = controls.getProvider("github").config;
const cline = controls.getProvider("cline").config;
const clinepass = controls.getProvider("clinepass").config;
Object.assign(cline, { authorizeUrl: `${base}/cline/authorize`, tokenExchangeUrl: `${base}/cline/token` });
Object.assign(clinepass, { authorizeUrl: `${base}/clinepass/authorize`, tokenUrl: `${base}/clinepass/token` });
Object.assign(google, {
  clientId: "fixture-google-client", clientSecret: "fixture-google-secret",
  authorizeUrl: `${base}/google/authorize`, tokenUrl: `${base}/google/token`, userInfoUrl: `${base}/google/user`,
});
Object.assign(github, {
  clientId: "fixture-github-client", deviceCodeUrl: `${base}/github/device`, tokenUrl: `${base}/github/poll`,
  userInfoUrl: `${base}/github/user`, copilotTokenUrl: `${base}/github/copilot`,
});

const server = http.createServer(async (request, response) => {
  const uri = new URL(request.url, "http://127.0.0.1");
  const [provider, action] = uri.pathname.split("/").slice(-2);
  const reply = (status, data) => {
    response.writeHead(status, { "Content-Type": "application/json" });
    response.end(JSON.stringify(data));
  };
  try {
    if (action === "authorize") {
      const redirect = uri.searchParams.get("redirect_uri") || "http://localhost:8080/callback";
      return reply(200, await controls.generateAuthData(provider, redirect));
    }
    if (action === "device-code") {
      if (controls.getProvider(provider).flowType !== "device_code") return reply(400, { error: "Provider does not support device code flow" });
      const auth = await controls.generateAuthData(provider, null);
      const device = await controls.requestDeviceCode(provider);
      return reply(200, { ...device, codeVerifier: device.codeVerifier || auth.codeVerifier });
    }
    const chunks = [];
    for await (const chunk of request) chunks.push(chunk);
    const payload = JSON.parse(Buffer.concat(chunks).toString());
    if (uri.pathname === "/fixture/refresh") return reply(200, await refreshAccount(payload));
    if (action === "exchange") {
      const { code, redirectUri, codeVerifier, state } = payload;
      const tokens = await controls.exchangeTokens(provider, code, redirectUri, codeVerifier, state);
      const connection = await save(provider, tokens);
      const { id, email, displayName } = connection;
      return reply(200, { success: true, connection: { id, provider, email, displayName } });
    }
    if (action === "poll") {
      const result = await controls.pollForToken(provider, payload.deviceCode, null, payload.extraData);
      if (!result.success) {
        const pending = result.pending || result.error === "authorization_pending" || result.error === "slow_down";
        return reply(200, { ...result, pending });
      }
      const connection = await save(provider, result.tokens);
      return reply(200, { success: true, connection: { id: connection.id, provider } });
    }
    return reply(400, { error: "Unknown action" });
  } catch {
    return reply(500, { error: "Provider operation failed" });
  }
});
server.listen(0, "127.0.0.1", () => console.log("ready", server.address().port));
process.prependOnceListener("SIGTERM", () => process.exit(0));

function save(provider, tokens) {
  return createProviderConnection({
    provider, authType: "oauth", ...tokens,
    expiresAt: tokens.expiresIn ? new Date(Date.now() + tokens.expiresIn * 1000).toISOString() : null,
    testStatus: "active",
  });
}


async function refreshAccount({ observed, rejected }) {
  let current = await getProviderConnectionById(observed.id);
  const provider = current.provider;
  if (provider === "github" && rejected) {
    const update = await new GithubExecutor().refreshCredentials(current);
    if (!update) return { error: "ProviderUnavailable" };
    const { copilotToken, copilotTokenExpiresAt, ...patch } = update;
    if (patch.expiresIn) patch.expiresAt = new Date(Date.now() + patch.expiresIn * 1000).toISOString();
    if (copilotToken) patch.providerSpecificData = {
      ...current.providerSpecificData, copilotToken, copilotTokenExpiresAt,
    };
    return { account: await updateProviderConnection(current.id, patch) };
  }
  if (current.refreshToken && (rejected || manager.shouldRefreshCredentials(provider, current))) {
    const refreshed = await manager.refreshProviderCredentials(provider, current);
    if (!refreshed) return { error: "ProviderUnavailable" };
    if (tokenRefresh.isUnrecoverableRefreshError(refreshed)) {
      return { error: "ReauthenticationRequired" };
    }
    current = await updateProviderConnection(current.id, refreshed);
  }
  const data = current.providerSpecificData || {};
  if (provider === "github" && (rejected || !data.copilotToken || data.copilotTokenExpiresAt * 1000 - Date.now() < 300000)) {
    const token = await tokenRefresh.refreshCopilotToken(current.accessToken);
    if (!token) return { error: "ProviderUnavailable" };
    current = await updateProviderConnection(current.id, {
      providerSpecificData: { ...data, copilotToken: token.token, copilotTokenExpiresAt: token.expiresAt },
    });
  }
  return { account: current };
}
