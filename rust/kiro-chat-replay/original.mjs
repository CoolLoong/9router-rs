import pkg from "../../package.json" with { type: "json" };
import { registerHooks } from "node:module";
import { statSync } from "node:fs";
import { fileURLToPath } from "node:url";

registerHooks({
  resolve(specifier, context, next) {
    if (specifier === "node-machine-id") {
      return next("data:text/javascript,export const machineIdSync = () => 'fixture-machine';", context);
    }
    if (!specifier.startsWith("@/") && !specifier.startsWith("open-sse/")) return next(specifier, context);
    const path = specifier.startsWith("@/") ? `src/${specifier.slice(2)}` : specifier;
    const target = new URL(`../../${path}`, import.meta.url);
    const info = statSync(fileURLToPath(target), { throwIfNoEntry: false });
    return next(info?.isDirectory() ? new URL(`${target.href}/index.js`).href : info ? target.href : target.href + ".js", context);
  },
});
const { KiroExecutor } = await import("../../open-sse/executors/kiro.js");
const { openaiToKiroRequest } = await import("../../open-sse/translator/request/openai-to-kiro.js");

const buffers = [];
for await (const chunk of process.stdin) buffers.push(chunk);
const input = JSON.parse(Buffer.concat(buffers));

async function exchange({ model, account, body, headers }) {
  const payload = openaiToKiroRequest(model, body, body.stream, { ...account, rawHeaders: headers });
  const region = account.providerSpecificData.region || "us-east-1";
  const executor = new KiroExecutor();
  const requestHeaders = (url) => {
    const pinned = executor.buildHeaders(account, true, url);
    const headers = {
      "Accept": pinned.Accept, "Content-Type": pinned["Content-Type"],
      "Authorization": pinned.Authorization, "User-Agent": `9Router/${pkg.version}`,
    };
    if (pinned.TokenType) headers.TokenType = pinned.TokenType;
    if (account.authType === "oauth") {
      for (const name of ["x-amz-sso-bearer", "x-amzn-codewhisperer-profile-arn", "X-Amz-Target"]) {
        if (pinned[name]) headers[name] = pinned[name];
      }
    }
    return headers;
  };
  const targets = executor.getOrderedBaseUrls(account);
  let response = await fetch(`${process.argv[2]}/${region}/generateAssistantResponse`, {
    method: "POST",
    headers: requestHeaders(targets[0]),
    body: JSON.stringify(payload),
    redirect: "manual",
  });
  if (account.authType === "oauth" && region === "us-east-1" && [401, 403, 404].includes(response.status)) {
    response = await fetch(`${process.argv[2]}/${region}/codewhisperer/generateAssistantResponse`, {
      method: "POST", headers: requestHeaders(targets[1]), body: JSON.stringify(payload), redirect: "manual",
    });
  }
  const normalized = response.ok ? executor.transformEventStreamToSSE(response, model) : response;
  return { native: true, status: normalized.status, wire: await normalized.text() };
}

if (Array.isArray(input.requests)) {
  const responses = [];
  for (const request of input.requests) responses.push(await exchange(request));
  console.log(JSON.stringify({ responses }));
} else {
  console.log(JSON.stringify(await exchange(input)));
}
