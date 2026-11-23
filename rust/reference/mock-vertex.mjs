import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { verify } from "node:crypto";
import { DEFAULT_THINKING_VERTEX_SIGNATURE } from "../../open-sse/config/defaultThinkingSignature.js";

const ORIGIN = "https://aiplatform.googleapis.com";
const OAUTH = "https://oauth2.googleapis.com/token";
const fixture = "http://127.0.0.1:52989";
const seenProbes = new Set();
let mintedServiceToken = false;
const publicKeyPath = () => resolve(process.env.DATA_DIR || ".", "vertex-fixture-key.pub");

export function vertexFixture(url) {
  if (url?.origin === ORIGIN && url.pathname.startsWith("/v1/")) return fixture + "/vertex" + url.pathname + url.search;
  if (url?.href === OAUTH && process.env.DATA_DIR) {
    try {
      readFileSync(publicKeyPath());
      return fixture + "/vertex-oauth/token";
    } catch { return null; }
  }
  return null;
}

export function isVertexMock(request) {
  const path = new URL(request.url, fixture).pathname;
  return request.method === "POST" && (path.startsWith("/vertex/v1/") || path === "/vertex-oauth/token");
}

function json(response, status, payload) {
  return response.writeHead(status, { "content-type": "application/json" }).end(JSON.stringify(payload));
}

function verifyAssertion(input) {
  const [header, data, sign, extra] = input.split(".");
  if (!header || !data || !sign || extra) return false;
  const protectedHeader = JSON.parse(Buffer.from(header, "base64url").toString());
  const claims = JSON.parse(Buffer.from(data, "base64url").toString());
  if (Object.keys(protectedHeader).join() !== "alg" || protectedHeader.alg !== "RS256" ||
      claims.iss !== "vertex-fixture@example.invalid" || claims.aud !== OAUTH ||
      claims.scope !== "https://www.googleapis.com/auth/cloud-platform" ||
      claims.exp - claims.iat !== 3600 || Math.abs(claims.iat - Math.floor(Date.now() / 1000)) > 30) return false;
  return verify("RSA-SHA256", Buffer.from(header + "." + data), readFileSync(publicKeyPath()), Buffer.from(sign, "base64url"));
}

function token(request, response, body) {
  const data = new URLSearchParams(body);
  const failure = () => json(response, 401, { error: "unexpected fixture OAuth request" });
  if (request.headers["content-type"] !== "application/x-www-form-urlencoded" || request.headers.authorization) return failure();
  if (data.get("grant_type") === "urn:ietf:params:oauth:grant-type:jwt-bearer") {
    if (mintedServiceToken || data.size !== 2 || !verifyAssertion(data.get("assertion") || "")) return failure();
    mintedServiceToken = true;
    return json(response, 200, { access_token: "offline-vertex-sa-token", expires_in: 3600 });
  }
  if (request.headers.accept !== "application/json" || data.size !== 4 ||
      data.get("grant_type") !== "refresh_token" || data.get("client_id") !== "offline-vertex-client" ||
      data.get("client_secret") !== "offline-vertex-client-secret" || data.get("refresh_token") !== "offline-vertex-refresh") return failure();
  return json(response, 200, { access_token: "offline-vertex-adc-token", expires_in: 3600 });
}

const cases = new Map([
  ["vertex projectless fixture", { path: "/v1/publishers/google/models/gemini-2.5-flash:generateContent", key: "offline-vertex-projectless", text: "raw projectless" }],
  ["vertex scoped fixture", { path: "/v1/projects/fixture-raw-project/locations/eu/publishers/google/models/gemini-4-argon:streamGenerateContent", key: "offline-vertex-scoped", text: "raw scoped", stream: true }],
  ["vertex bearer fixture", { path: "/v1/projects/fixture-bearer-project/locations/global/publishers/google/models/gemini-4-argon:generateContent", token: "offline-vertex-preset-bearer", text: "configured bearer" }],
  ["vertex service fixture", { path: "/v1/projects/fixture-sa-project/locations/us-central1/publishers/google/models/gemini-2.5-flash:generateContent", token: "offline-vertex-sa-token", text: "service account" }],
  ["vertex adc fixture", { path: "/v1/projects/fixture-adc-project/locations/us-east1/publishers/google/models/gemini-2.5-flash:generateContent", token: "offline-vertex-adc-token", text: "authorized user" }],
  ["vertex tool fixture", { path: "/v1/publishers/google/models/gemini-2.5-flash:generateContent", key: "offline-vertex-projectless", text: "vertex tool history" }],
  ["vertex key redaction fixture", { path: "/v1/publishers/google/models/gemini-2.5-flash:generateContent", key: "offline-vertex-projectless", error: "offline-vertex-projectless" }],
  ["vertex bearer redaction fixture", { path: "/v1/projects/fixture-bearer-project/locations/global/publishers/google/models/gemini-4-argon:generateContent", token: "offline-vertex-preset-bearer", error: "offline-vertex-preset-bearer" }],
  ["partner probe fixture", { path: "/v1/projects/fixture-probed-project/locations/global/endpoints/openapi/chat/completions", key: "offline-vertex-partner-probe", text: "partner discovered", partner: true }],
  ["partner configured fixture", { path: "/v1/projects/fixture-partner-project/locations/global/endpoints/openapi/chat/completions", key: "offline-vertex-partner-project", text: "partner configured", partner: true }],
]);

const usageMetadata = { promptTokenCount: 8, candidatesTokenCount: 3, totalTokenCount: 11 };
const chatUsage = { prompt_tokens: 8, completion_tokens: 3, total_tokens: 11 };

export async function handleVertexMock(request, response) {
  const url = new URL(request.url, fixture);
  let raw = "";
  for await (const chunk of request) raw += chunk;
  if (url.pathname === "/vertex-oauth/token") return token(request, response, raw);
  const fail = message => json(response, 422, { error: { message } });
  const key = url.searchParams.get("key");
  if (url.pathname === "/vertex/v1/publishers/google/models/__probe__:generateContent") {
    if (raw !== "{}" || request.headers.authorization || key !== "offline-vertex-partner-probe" || seenProbes.has(key)) return fail("unexpected fixture project probe");
    seenProbes.add(key);
    return json(response, 404, [{ error: { message: "Publisher could not locate projects/fixture-probed-project/locations/global/models/__probe__" } }]);
  }
  if (request.headers["content-type"] !== "application/json") return fail("unexpected fixture Content-Type");
  let body;
  try { body = JSON.parse(raw); } catch { return fail("unexpected fixture body"); }
  const label = body.messages?.[0]?.content ?? body.contents?.[0]?.parts?.[0]?.text;
  const check = cases.get(label);
  if (!check || url.pathname !== "/vertex" + check.path || (check.key || null) !== key ||
      (check.token ? request.headers.authorization !== "Bearer " + check.token : request.headers.authorization) ||
      request.headers["x-goog-api-key"] || (check.stream ? request.headers.accept !== "text/event-stream" : request.headers.accept === "text/event-stream") ||
      url.searchParams.get("alt") !== (check.stream ? "sse" : null)) return fail("unexpected Vertex fixture route or credentials");
  if (check.partner) {
    if (body.model !== "deepseek-ai/deepseek-v3.2-maas" || !body.messages?.length || Object.hasOwn(body, "contents")) {
      return fail("Vertex Partner request requires OpenAI Chat wire");
    }
    return json(response, 200, { id: "chatcmpl-vertex-partner-fixture", created: 1795046400, model: body.model,
      object: "chat.completion", choices: [{ index: 0, message: { role: "assistant", content: check.text }, finish_reason: "stop" }], usage: chatUsage });
  }
  if (body.model !== check.path.split("/").at(-1).split(":")[0] || !body.contents?.length ||
    Object.hasOwn(body, "messages") || body.safetySettings?.length !== 5) return fail("Vertex request requires Gemini wire");
  if (check.error) return fail("fixture response echoed " + check.error);

  if (label === "vertex tool fixture") {
    const parts = body.contents.flatMap(turn => turn.parts);
    const call = parts.find(part => part.functionCall);
    const result = parts.find(part => part.functionResponse);
    const signature = parts.find(part => part.thoughtSignature);
    if (!call || Object.hasOwn(call.functionCall, "id") || !result || Object.hasOwn(result.functionResponse, "id") ||
        signature?.thoughtSignature !== DEFAULT_THINKING_VERTEX_SIGNATURE) return fail("Vertex tool parts or pinned signature changed");
  }
  const base = { responseId: "vertex-fixture", modelVersion: body.model, createTime: "2026-11-19T00:00:00Z" };
  const candidate = { content: { role: "model", parts: [{ text: check.text }] }, finishReason: "STOP" };
  if (check.stream) {
    response.writeHead(200, { "content-type": "text/event-stream" });
    const data = { ...base, candidates: [{ content: candidate.content }] };
    response.write("data: " + JSON.stringify({ response: data }) + "\n\n");
    response.end("data: " + JSON.stringify({ ...base, candidates: [{ finishReason: "STOP", content: { parts: [] } }], usageMetadata }) + "\n\n");
    return;
  }
  return json(response, 200, { response: { ...base, candidates: [candidate], usageMetadata } });
}
