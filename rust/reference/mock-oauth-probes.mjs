const endpoints = new Map([
  ["https://api.github.com/user", "github"],
  ["https://gitlab.com/api/v4/user", "gitlab"],
  ["https://api.kilo.ai/api/profile", "kilocode"],
  ["https://openapi.qoder.sh/api/v1/userinfo", "qoder"],
  ["https://openapi.qoder.com.cn/api/v1/userinfo", "qoder-cn"],
]);

export function oauthProbeFixture(url) {
  const provider = endpoints.get(url?.href);
  return provider ? `http://127.0.0.1:52989/oauth-probes/${provider}` : null;
}

export function isOAuthProbeMock(request) {
  return request.url.startsWith("/oauth-probes/");
}

export function handleOAuthProbeMock(request, response) {
  const provider = request.url.slice("/oauth-probes/".length);
  const expected = provider === "kilocode" ? "offline-kilo-chat" : `offline-probe-${provider}`;
  const authenticated = request.method === "GET" && request.headers.authorization === `Bearer ${expected}` &&
    (provider !== "github" || request.headers["user-agent"] === "9Router" &&
      request.headers.accept === "application/vnd.github+json");
  const status = authenticated ? { qoder: 401, "qoder-cn": 403, github: 200, gitlab: 200, kilocode: 200 }[provider] : 422;
  response.writeHead(status ?? 404, { "content-type": "application/json" }).end(JSON.stringify({
    result: authenticated ? "fixture" : "unexpected OAuth probe",
  }));
}
