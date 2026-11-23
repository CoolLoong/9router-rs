import pkg from "../../package.json" with { type: "json" };

const loopback = "http://127.0.0.1:52989";
const keyPath = "/kiro/q/eu-west-2/ListAvailableModels";
const profilePath = "/kiro/codewhisperer/us-east-1";
const oidcPath = "/kiro/oidc/eu-west-2/token";
const key = "ksk_fixture_router_only";
const profileToken = "fixture-kiro-profile-token";
const profile = "arn:aws:codewhisperer:us-east-1:123456789012:profile/FIXTURE";

export function isKiroModelsMock(request) {
  const path = new URL(request.url, loopback).pathname;
  return (path === keyPath && request.method === "GET") ||
    ((path === profilePath || path === oidcPath) && request.method === "POST");
}

function reply(response, status, value) {
  return response.writeHead(status, { "content-type": "application/json" }).end(JSON.stringify(value));
}

export async function handleKiroModelsMock(request, response) {
  const address = new URL(request.url, loopback);
  const headers = request.headers;
  if (headers.accept !== "application/json" || headers["user-agent"] !== "9Router/" + pkg.version
      || headers["x-amz-user-agent"] || headers["amz-sdk-invocation-id"] || headers["x-amzn-kiro-agent-mode"]) {
    return reply(response, 422, { error: "Unexpected Kiro fixture identity" });
  }
  if (address.pathname === keyPath) {
    if (headers.authorization !== "Bearer " + key || headers.tokentype !== "API_KEY" ||
        address.search !== "?origin=AI_EDITOR") {
      return reply(response, 401, { error: "Invalid fixture API-key request" });
    }
    return reply(response, 200, { models: [
      { modelId: "auto", modelName: "Auto" },
      { modelId: "fixture-key-model", modelName: "Key model", rateMultiplier: 1.25,
        description: "Synthetic API-key catalog result", tokenLimits: { maxInputTokens: 300000 } },
    ] });
  }
  let body = "";
  for await (const chunk of request) body += chunk;
  const input = JSON.parse(body);
  if (address.pathname === oidcPath) {
    if (headers.authorization || headers["content-type"] !== "application/json" || input.grantType !== "refresh_token" ||
        input.clientId !== "fixture-kiro-client" || input.clientSecret !== "fixture-kiro-client-secret" ||
        input.refreshToken !== "fixture-kiro-refresh") {
      return reply(response, 401, { error: "Invalid fixture OIDC request" });
    }
    const identity = Buffer.from(JSON.stringify({ email: "fixture-kiro@example.invalid" })).toString("base64url");
    return reply(response, 200, {
      accessToken: `eyJmaXh0dXJlIjp0cnVlfQ.${identity}.synthetic`, refreshToken: "fixture-kiro-renewed-refresh",
      expiresIn: 3600, profileArn: profile,
    });
  }
  if (headers.authorization !== "Bearer " + profileToken || headers.tokentype ||
      headers["content-type"] !== "application/x-amz-json-1.0" ||
      headers["x-amz-target"] !== "AmazonCodeWhispererService.ListAvailableModels" ||
      input.origin !== "AI_EDITOR" || input.profileArn !== profile) {
    return reply(response, 401, { error: "Invalid fixture OAuth profile request" });
  }
  return reply(response, 200, { models: [
    { modelId: "fixture-profile-model", modelName: "Profile model", tokenLimits: { maxInputTokens: 100000 } },
  ] });
}
