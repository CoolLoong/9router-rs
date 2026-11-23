import pkg from "../../package.json" with { type: "json" };
import { readFileSync } from "node:fs";
import { resolve } from "node:path";

const origin = "https://api.cline.bot";
const fixture = "http://127.0.0.1:52989";
const chat = "/api/v1/chat/completions";
const models = "/api/v1/models";
const recommended = "/api/v1/ai/cline/recommended-models";
const refresh = "/api/v1/auth/refresh";
const endpoints = new Map([[chat, "POST"], [models, "GET"], [recommended, "GET"], [refresh, "POST"]]);

export function clineFixture(url) {
  return url?.origin === origin && endpoints.has(url.pathname) ? fixture + "/cline" + url.pathname + url.search : null;
}

export function isClineMock(request) {
  const path = new URL(request.url, fixture).pathname;
  return path.startsWith("/cline/") && endpoints.get(path.slice("/cline".length)) === request.method;
}

function json(response, status, value) {
  return response.writeHead(status, { "content-type": "application/json" }).end(JSON.stringify(value));
}

function reject(response, message = "Fixture authorization rejected", status = 422) {
  return json(response, status, { error: { message } });
}

function implementation() {
  try {
    return readFileSync(resolve(process.env.DATA_DIR || ".", "cline-transport-implementation"), "utf8").trim();
  } catch {
    return "reference";
  }
}

const clientVersion = pkg.version;
const key = "clp_fixture_only";
const oauth = "WorkOS:eyJfixture.bearer.mock";
const bare = "eyJfixture.raw.mock";
const expiredRefresh = "fixture-expired-refresh";
const rejectedRefresh = "fixture-rejected-refresh";
const passRefresh = "fixture-pass-refresh";
const expiredAccess = "workos:eyJfixture.renewed.mock";
const rejectedAccess = "eyJfixture.retried.mock";
const passAccess = "eyJfixture.passrenewed.mock";

function authorized(headers, token) {
  return headers.authorization === "Bearer " + token;
}

function identity(headers) {
  return headers["http-referer"] === "https://cline.bot" && headers["x-title"] === "Cline"
    && headers["user-agent"] === "9Router/" + clientVersion && headers["x-platform"] === process.platform
    && headers["x-platform-version"] === (implementation() === "rust" ? "unknown" : process.version)
    && headers["x-client-type"] === "9router" && headers["x-client-version"] === clientVersion
    && headers["x-core-version"] === clientVersion && headers["x-is-multiroot"] === "false";
}

const live = [
  { id: "anthropic/fixture-live-cline", name: "Live Cline" },
  { id: "cline-pass/fixture-live-pass", name: "Live Pass" },
  { id: "anthropic/fixture-live-cline", name: "Last Cline entry" },
  { id: "cline-pass/fixture-plain-model", name: "" },
  { id: " " },
  { id: 4, name: "Invalid fixture" },
];

const recommendations = [
  { id: "cline-free/fixture-live-free", name: "Free Cline" },
  { id: "anthropic/fixture-live-cline", name: "Duplicate free item" },
  { id: null, name: "Invalid free item" },
];

async function requestBody(request) {
  let raw = "";
  for await (const chunk of request) raw += chunk;
  try { return JSON.parse(raw); } catch { return null; }
}

function modelList(request, response) {
  if (request.headers.accept !== "application/json") return reject(response, "Expected JSON model Accept");
  if (authorized(request.headers, key) && !request.headers["x-client-type"]) {
    return json(response, 200, { success: true, data: live });
  }
  if ((authorized(request.headers, oauth) || authorized(request.headers, "workos:" + bare)) && identity(request.headers)) {
    return json(response, 200, live);
  }
  return reject(response, "Fixture model credentials unavailable", 401);
}

const scenarios = {
  "pass-json": { model: "cline-pass/glm-5.2", token: key },
  "pass-stream": { model: "cline-pass/glm-5.2", token: key, stream: true },
  "oauth-json": { model: "anthropic/claude-sonnet-4.6", token: oauth },
  "oauth-stream": { model: "anthropic/claude-sonnet-4.6", token: oauth, stream: true },
  "oauth-normalized": { model: "anthropic/claude-sonnet-4.6", token: "workos:" + bare },
  "oauth-expired-refresh": { model: "anthropic/claude-sonnet-4.6", token: expiredAccess },
  "oauth-rejected-refresh": { model: "anthropic/claude-sonnet-4.6", token: "workos:" + rejectedAccess },
  "pass-oauth-refresh": { model: "cline-pass/glm-5.2", token: "workos:" + passAccess },
  "provider-error": { model: "cline-pass/glm-5.2", token: key, status: 400 },
};

const usage = { prompt_tokens: 6, completion_tokens: 2, total_tokens: 8 };

export async function handleClineMock(request, response) {
  const path = new URL(request.url, fixture).pathname.slice("/cline".length);
  if (path === models) return modelList(request, response);
  if (path === recommended) {
    if (request.headers.authorization || request.headers.accept !== "application/json") {
      return reject(response, "Fixture free feed requires only JSON Accept");
    }
    return json(response, 200, { free: recommendations });
  }

  const payload = await requestBody(request);
  if (!payload || request.headers["content-type"] !== "application/json") {
    return reject(response, "Expected fixture JSON");
  }
  if (path === refresh) {
    const fields = Object.keys(payload).sort().join();
    if (fields !== "clientType,grantType,refreshToken" || payload.grantType !== "refresh_token"
        || payload.clientType !== "extension" || request.headers.accept !== "application/json"
        || request.headers.authorization) {
      return reject(response, "Unexpected refresh fields");
    }
    if (payload.refreshToken === expiredRefresh) {
      return json(response, 200, { data: {
        accessToken: expiredAccess, refreshToken: "fixture-rotated-expired", expiresAt: "2099-11-19T00:00:00Z",
      } });
    }
    if (payload.refreshToken === rejectedRefresh) {
      return json(response, 200, { accessToken: rejectedAccess, expiresIn: 3600 });
    }
    if (payload.refreshToken === passRefresh) {
      return json(response, 200, { data: { accessToken: passAccess, refreshToken: "fixture-rotated-pass", expires_in: 4200 } });
    }
    return reject(response, "Unknown fixture refresh token", 401);
  }

  const label = payload.messages?.[0]?.content;
  const scenario = scenarios[label];
  if (!scenario || payload.model !== scenario.model || payload.stream !== Boolean(scenario.stream) || !identity(request.headers)) {
    return reject(response, "Unexpected fixture chat model, flags or identity");
  }
  if (scenario.stream && request.headers.accept !== "text/event-stream") {
    return reject(response, "Expected event-stream chat Accept");
  }
  if (label === "oauth-normalized" && authorized(request.headers, bare)) {
    return reject(response, "Fixture saw bare WorkOS bearer", 401);
  }
  if (!authorized(request.headers, scenario.token)) return reject(response, undefined, 401);
  if (scenario.status) return reject(response, "Fixture provider rejected chat", scenario.status);
  const base = { id: "chatcmpl-cline-fixture", created: 1795046400, model: scenario.model };
  if (!scenario.stream) {
    return json(response, 200, { success: true, data: { ...base, object: "chat.completion", usage,
      choices: [{ index: 0, message: { role: "assistant", content: "fixture " + label }, finish_reason: "stop" }] } });
  }
  response.writeHead(200, { "content-type": "text/event-stream", "cache-control": "no-cache" });
  const chunk = (delta, finish_reason, extra) => response.write("data: " + JSON.stringify({
    ...base, object: "chat.completion.chunk", choices: [{ index: 0, delta, finish_reason }], ...extra,
  }) + "\n\n");
  chunk({ role: "assistant" }, null);
  chunk({ content: "fixture " + label }, null);
  chunk({}, "stop", { usage });
  return response.end("data: [DONE]\n\n");
}
