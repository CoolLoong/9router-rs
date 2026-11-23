import { handleOpenCodeSystemOneMock } from "./mock-opencode-systemone.mjs";

const routes = new Map([
  ["https://opencode.ai/zen/v1/systemone", "/opencode/zen/v1/systemone"],
  ["https://v1m.ir/v1/systemone", "/v1m/v1/systemone"],
  ["https://openrouter.ai/api/v1/systemone", "/openrouter/api/v1/systemone"],
  ["https://ollama.com/v1/systemone", "/ollama/v1/systemone"],
  ["http://localhost:11434/v1/systemone", "/ollama-local/v1/systemone"],
  ["https://console.nace.ai/v1/systemone", "/nace/v1/systemone"],
  ["https://api.cloudflare.com/client/v4/accounts/fixture-account/ai/run/@cf/cloudflare/clef-flash",
    "/cloudflare/client/v4/accounts/fixture-account/ai/run/@cf/cloudflare/clef-flash"],
]);
const paths = new Set([...routes.values(), "/nace-custom/v1/systemone"]);
const question = { route: { type: "choice", criterion: { a: "first", b: "second" } } };
const configs = new Map([
  ["v1m", "rev-latest"], ["openrouter", "typesafe/jev-1.13"], ["ollama", "clef"], ["ollama-local", "clef-flash"],
  ["nace", "drex-v2.0"], ["nace-custom", "drex-v2.0"], ["cloudflare", "clef-flash"],
]);

export function systemOneFixture(url) {
  const path = url && routes.get(url.origin + url.pathname);
  return path ? `http://127.0.0.1:52989${path}${url.search}` : null;
}

export function isSystemOneMock(request) {
  return request.method === "POST" && paths.has(new URL(request.url, "http://127.0.0.1").pathname);
}

function json(response, status, data) {
  return response.writeHead(status, { "content-type": "application/json" }).end(JSON.stringify(data));
}

export async function handleSystemOneMock(request, response) {
  const provider = new URL(request.url, "http://127.0.0.1").pathname.split("/")[1];
  if (provider === "opencode") return handleOpenCodeSystemOneMock(request, response);
  const key = request.headers.authorization;
  const canonical = provider === "nace-custom" ? "nace" : provider;
  const backup = provider === "v1m" && key === "Bearer offline-systemone-v1m-backup";
  if (key !== `Bearer offline-systemone-${canonical}` && !backup) return json(response, 401, { error: "invalid fixture key" });
  if (request.headers["content-type"] !== "application/json") return json(response, 422, { error: "wrong fixture content-type" });
  if (provider === "openrouter" && (request.headers["http-referer"] !== "https://endpoint-proxy.local" ||
      request.headers["x-title"] !== "Endpoint Proxy")) return json(response, 422, { error: "missing fixture attribution" });
  let raw = "";
  for await (const chunk of request) raw += chunk;
  let body;
  try { body = JSON.parse(raw); } catch { return json(response, 422, { error: "invalid fixture body" }); }
  if (body.model !== configs.get(provider) && !(provider === "v1m" && ["fixture-systemone-rotate", "fixture-systemone-malformed", "fixture-systemone-error", "fixture-systemone-redaction"].includes(body.model))) {
    return json(response, 422, { error: "unexpected fixture selector" });
  }
  if (body.model === "fixture-systemone-rotate" && !backup) {
    return json(response, 429, { error: { message: "fixture quota exhausted" } });
  }
  if (body.model === "fixture-systemone-malformed") {
    return response.writeHead(200, { "content-type": "application/json" }).end('{"answers":');
  }
  if (body.model === "fixture-systemone-error") {
    return json(response, 422, { error: { message: "invalid fixture condition" }, message: "not the inner error" });
  }
  if (body.model === "fixture-systemone-redaction") {
    return json(response, 422, { error: `Rejected ${key.slice(7)} and ${key}` });
  }
  if (provider === "v1m" && typeof body.state === "string" && body.state.startsWith("fixture smart ")) {
    const a = "fixture-live/fixture-combo-a";
    const b = "fixture-live/fixture-combo-b";
    const route = body.questions?.route;
    if (route?.type !== "choice" || route.instructions !== "Which of these configured models is the most appropriate for this request?" ||
        JSON.stringify(route.criteria) !== JSON.stringify({ [a]: null, [b]: null }) || Object.keys(body).length !== 3) {
      return json(response, 422, { error: "unexpected Smart classifier request" });
    }
    const choice = body.state === "fixture smart choose first" ? a : b;
    const confidence = body.state === "fixture smart low confidence" ? 0.4 : 0.95;
    return json(response, 200, { answers: { route: { choice, confidence } } });
  }
  if (body.state === false && provider === "ollama-local" && JSON.stringify(body.questions) === "{}") {
    return json(response, 200, { answers: {}, usage: {} });
  }
  if (body.state === true && provider === "nace-custom" && JSON.stringify(body.questions) === "{}") {
    return json(response, 200, { answers: { configured: { reached: true } } });
  }
  if (body.state?.text !== "offline fixture" || body.state.revision !== 2 ||
      JSON.stringify(body.questions) !== JSON.stringify(question) || body.response_mode !== "strict" ||
      Object.keys(body).length !== 4) return json(response, 422, { error: "unexpected fixture state or questions" });
  return json(response, 202, {
    model: body.model, answers: { route: { choice: backup ? "b" : "a", confidence: 0.96, probabilities: { a: 0.96, b: 0.04 } } },
    usage: { input_tokens: 9, output_tokens: 3 }, provider_extension: { received: provider },
  });
}
