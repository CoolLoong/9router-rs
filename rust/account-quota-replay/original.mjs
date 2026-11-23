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
    if (specifier.endsWith("/proxyFetch.js") && /(\/services\/usage\/(claude|codex|clinepass|commandcode|cursor|deepseek|github|glm|groq|kimi|kiro|minimax|misc|opencode-go|opencode-zen|shared)|\/services\/qoderModels|\/executors\/(default|github)|\/shared\/zedAuth)\.js$/.test(context.parentURL || "")) {
      return next("data:text/javascript,export const proxyAwareFetch = (url,options) => globalThis.fixtureTransport(url,options);", context);
    }
    if (!specifier.startsWith("@/") && !specifier.startsWith("open-sse/")) return next(specifier, context);
    const relative = specifier.startsWith("@/") ? `src/${specifier.slice(2)}` : specifier;
    const target = new URL(`../../${relative}`, import.meta.url);
    const info = fs.statSync(fileURLToPath(target), { throwIfNoEntry: false });
    const url = info?.isDirectory() ? new URL(`${target.href}/index.js`)
      : info ? target : new URL(`${target.href}.js`);
    return next(url.href, context);
  },
});
const localFetch = globalThis.fetch;
function fixtureTransport(input, options) {
  const source = new URL(input);
  const endpoints = {
    "https://chatgpt.com/backend-api/wham/usage": "/codex/quota",
    "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits": "/codex/credits",
    "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits/consume": "/quota-reset/codex/consume",
    "https://auth.openai.com/oauth/token": "/codex/token",
    "https://api.anthropic.com/api/oauth/usage": "/claude/quota",
    "https://api.anthropic.com/v1/settings": "/claude/settings",
    "https://api.anthropic.com/v1/oauth/token": "/claude/token",
    "https://api.anthropic.com/api/oauth/profile": "/quota-reset/claude/profile",
    "https://api.anthropic.com/api/organizations/org-fixture/reset_rate_limits": "/quota-reset/claude/organizations/org-fixture/reset_rate_limits",
    "https://api.anthropic.com/v1/organizations/org-fixture/usage": "/claude/organizations/org-fixture/usage",
    "https://api.cline.bot/api/v1/users/me/plan/usage-limits": "/clinepass/quota",
    "https://api.cline.bot/api/v1/auth/refresh": "/cline/api/v1/auth/refresh",
    "https://api.github.com/copilot_internal/user": "/github/quota",
    "https://api.github.com/copilot_internal/v2/token": "/github/copilot",
    "https://github.com/login/oauth/access_token": "/github/token",
    "https://cloudcode-pa.googleapis.com/v1internal:retrieveUserQuota": "/gemini/quota",
    "https://cloudcode-pa.googleapis.com/v1internal:loadCodeAssist": "/gemini/assist",
    "https://cloud.zed.dev/client/users/me": "/zed/quota",
    "https://api.deepseek.com/user/balance": "/deepseek/balance",
    "https://api.synthetic.new/v2/quotas": "/synthetic/quota",
    "https://api.groq.com/openai/v1/models": "/groq/models",
    "https://ai-gateway.vercel.sh/v1/credits": "/vercel/credits",
    "https://ollama.com/api/usage": "/ollama/usage",
    "https://ollama.com/api/me": "/ollama/me",
    "https://ollama.com/api/balance": "/ollama/balance",
    "https://api.kimi.com/coding/v1/usages": "/kimi/usages",
    "https://api2.cursor.sh/aiserver.v1.DashboardService/GetCurrentPeriodUsage": "/cursor/period",
    "https://api2.cursor.sh/aiserver.v1.DashboardService/GetPlanInfo": "/cursor/plan",
    "https://api2.cursor.sh/auth/usage": "/cursor/legacy",
    "https://api.z.ai/api/monitor/usage/quota/limit": "/glm/global",
    "https://open.bigmodel.cn/api/monitor/usage/quota/limit": "/glm/cn",
    "https://www.minimax.io/v1/token_plan/remains": "/minimax/global/token_plan/remains",
    "https://api.minimax.io/v1/api/openplatform/coding_plan/remains": "/minimax/global-alt/coding_plan/remains",
    "https://www.minimaxi.com/v1/api/openplatform/coding_plan/remains": "/minimax/cn/coding_plan/remains",
    "https://api.minimaxi.com/v1/api/openplatform/coding_plan/remains": "/minimax/cn-alt/coding_plan/remains",
    "https://openapi.qoder.sh/api/v2/quota/usage": "/qoder/global/usage",
    "https://openapi.qoder.sh/api/v1/jobToken/exchange": "/qoder/global/exchange",
    "https://openapi.qoder.sh/api/v1/userinfo": "/qoder/global/identity",
    "https://openapi.qoder.com.cn/api/v2/quota/usage": "/qoder/cn/usage",
    "https://openapi.qoder.com.cn/api/v1/jobToken/exchange": "/qoder/cn/exchange",
    "https://openapi.qoder.com.cn/api/v1/userinfo": "/qoder/cn/identity",
    "https://codewhisperer.us-east-1.amazonaws.com/getUsageLimits": "/kiro/cw/getUsageLimits",
    "https://codewhisperer.us-east-1.amazonaws.com/": "/kiro/cw",
    "https://q.us-east-1.amazonaws.com/getUsageLimits": "/kiro/q/getUsageLimits",
    "https://api.commandcode.ai/alpha/whoami": "/commandcode/alpha/whoami",
    "https://api.commandcode.ai/alpha/billing/credits": "/commandcode/alpha/billing/credits",
    "https://api.commandcode.ai/alpha/billing/subscriptions": "/commandcode/alpha/billing/subscriptions",
    "https://opencode.ai/inference/go/v1/usage": "/opencode/go",
    "https://opencode.ai/zen/v1/usage": "/opencode/zen",
  };
  const path = endpoints[source.origin + source.pathname];
  if (!path) throw new Error("Fixture avoided a non-loopback request");
  let init = options;
  if (["/claude/token", "/codex/token"].includes(path) && typeof options?.body === "string") {
    const body = JSON.parse(options.body);
    body.client_id = path.startsWith("/codex") ? "fixture-codex-client" : "fixture-claude-client";
    init = { ...options, body: JSON.stringify(body) };
  }
  return localFetch(`${base}${path}${source.search}`, init);
}
globalThis.fetch = fixtureTransport;
globalThis.fixtureTransport = fixtureTransport;
const usage = await import("../../src/app/api/usage/[connectionId]/route.js");
const reset = await import("../../src/app/api/usage/[connectionId]/codex-reset-credits/route.js");
const claudeReset = await import("../../src/app/api/usage/[connectionId]/claude-reset/route.js");
const { getAdapter } = await import("../../src/lib/db/driver.js");
await getAdapter();
globalThis.fetch = fixtureTransport;

const server = http.createServer(async (request, response) => {
  if (request.headers["x-replay-admin"] !== "fixture") {
    response.writeHead(401, { "Content-Type": "application/json" });
    response.end('{"error":"Unauthorized"}');
    return;
  }
  const uri = new URL(request.url, "http://127.0.0.1");
  const [, , , connectionId, action] = uri.pathname.split("/");
  try {
    const route = action === "codex-reset-credits" ? reset : action === "claude-reset" ? claudeReset : usage;
    const chunks = [];
    for await (const chunk of request) chunks.push(chunk);
    const body = chunks.length ? Buffer.concat(chunks) : undefined;
    const result = await route[request.method](new Request(uri, { method: request.method, headers: request.headers, body }),
      { params: Promise.resolve({ connectionId }) });
    response.writeHead(result.status, { "Content-Type": "application/json" });
    response.end(await result.text());
  } catch {
    response.writeHead(500, { "Content-Type": "application/json" });
    response.end('{"error":"Fixture could not read provider quota"}');
  }
});
process.prependOnceListener("SIGTERM", () => process.exit(0));
server.listen(0, "127.0.0.1", () => console.log("ready", server.address().port));
