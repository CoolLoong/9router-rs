import fs from "node:fs";
import { registerHooks } from "node:module";
import { fileURLToPath } from "node:url";

const fixture = JSON.parse(fs.readFileSync(0, "utf8"));
const directFetch = globalThis.fetch;
registerHooks({
  resolve(specifier, context, next) {
    if (specifier === "node-machine-id") {
      return next("data:text/javascript,export const machineIdSync = () => 'fixture-machine';", context);
    }
    if (/(\/base|\/github)\.js$/.test(context.parentURL || "") && (context.parentURL || "").includes("/open-sse/executors/") && specifier.endsWith("/proxyFetch.js")) {
      return next("data:text/javascript,export const proxyAwareFetch = (url, init) => globalThis.fixtureChat(url, init);", context);
    }
    if (!specifier.startsWith("@/") && !specifier.startsWith("open-sse/")) return next(specifier, context);
    const relative = specifier.startsWith("@/") ? `src/${specifier.slice(2)}` : specifier;
    const target = new URL(`../../${relative}`, import.meta.url);
    const info = fs.statSync(fileURLToPath(target), { throwIfNoEntry:false });
    const url = info?.isDirectory() ? new URL(`${target.href}/index.js`)
      : info ? target : new URL(`${target.href}.js`);
    return next(url.href, context);
  },
});
globalThis.fixtureChat = (url, init) => {
  const endpoints = {
    "https://api.githubcopilot.com/chat/completions":fixture.endpoint,
    "https://api.githubcopilot.com/responses":fixture.responsesEndpoint,
    "https://api.githubcopilot.com/v1/messages":fixture.messagesEndpoint,
  };
  if (!endpoints[url]) throw new Error("Fixture expected GitHub chat or Responses");
  return directFetch(endpoints[url], init);
};
const { GithubExecutor } = await import("../../open-sse/executors/github.js");
const executor = new GithubExecutor();

if (fixture.action === "monthly") {
  const upstream = fs.readFileSync(new URL("../../src/sse/services/auth.js", import.meta.url),"utf8");
  const part = upstream.slice(upstream.indexOf("const GITHUB_MONTHLY_USAGE_LIMIT"), upstream.indexOf("/**", upstream.indexOf("function githubMonthlyResetMs")));
  const check = new Function("Date", "resolveProviderId", part + "\nreturn githubMonthlyResetMs;");
  class FrozenClock extends Date {
    constructor() { super(fixture.now); }
  }
  console.log(JSON.stringify({reset:check(FrozenClock, value => value)(fixture.status,fixture.error,"github")}));
} else if (fixture.action === "classification") {
  console.log(JSON.stringify({
    supported:!executor.isClaudeModel(fixture.model),
    needsResponses:(fixture.status ?? 400) === 400 && executor.supportsResponsesEndpoint(fixture.model)
      && (fixture.error.includes("not accessible via the /chat/completions endpoint")
        || fixture.error.includes("The requested model is not supported")),
  }));
} else {
  const responses = [];
  for (let turn=0; turn<(fixture.turns || 1); turn++) {
    const result = await executor.execute({
      model:fixture.model, body:fixture.body, stream:fixture.stream,
      credentials:{accessToken:fixture.account.accessToken,copilotToken:fixture.account.providerSpecificData?.copilotToken},
      providerOverrides:fixture.settings.providerOverrides?.github,
    });
    responses.push({status:result.response.status,body:await result.response.text(),url:result.url});
  }
  console.log(JSON.stringify(["alternate","claude"].includes(fixture.action) ? {responses} : responses[0]));
}
process.exit(0);
