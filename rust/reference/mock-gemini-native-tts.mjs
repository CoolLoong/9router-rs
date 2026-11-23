import { gzipSync } from "node:zlib";

const mediaType = "audio/L16;rate=24000";
const pcm = Buffer.from([1, 0, 2, 0, 255, 127, 0, 128]).toString("base64");

export function isGeminiNativeTtsMock(request) {
  const url = new URL(request.url, "http://127.0.0.1");
  return request.method === "POST" && url.pathname.startsWith("/gemini/v1beta/models/") && url.searchParams.has("tts_fixture");
}

function fail(response, status, message) {
  return response.writeHead(status, { "content-type": "application/json" }).end(JSON.stringify({ error: { message } }));
}

function compressed(response, status, body, type, mark) {
  const data = gzipSync(body);
  return response.writeHead(status, {
    "content-type": type, "content-encoding": "gzip", "content-length": data.length,
    "access-control-allow-origin": "https://example.invalid", "x-fixture-upstream": mark,
    "vary": "Origin",
  }).end(data);
}

function completion(connection) {
  return `{ "candidates": [{"content":{"parts":[{"inlineData":{"mimeType":"${mediaType}","data":"${pcm}"}}]}}], "fixtureConnection": "${connection}" }\n`;
}

export async function handleGeminiNativeTtsMock(request, response) {
  const url = new URL(request.url, "http://127.0.0.1");
  const scenario = url.searchParams.get("tts_fixture");
  const operation = url.pathname.split("/").at(-1);
  const clientKey = request.headers["x-goog-api-key"];
  const primary = !request.headers.authorization && ["offline-provider-placeholder", "offline-native-tts-primary"].includes(clientKey);
  const alternate = !clientKey && request.headers.authorization === "Bearer offline-native-tts-bearer";
  if (url.searchParams.has("key") || (!primary && !alternate)) return fail(response, 401, "unexpected native Gemini credential");
  if (request.headers["x-api-key"]) return fail(response, 400, "client policy key forwarded to upstream");
  const account = alternate ? "bearer" : "apikey";
  let raw = "";
  for await (const chunk of request) raw += chunk;
  let body;
  try { body = JSON.parse(raw); } catch { return fail(response, 422, "invalid fixture data"); }
  const text = body.contents?.[0]?.parts?.[0]?.text;
  const voice = body.generationConfig?.speechConfig?.voiceConfig?.prebuiltVoiceConfig?.voiceName;
  const hasAudio = body.generationConfig?.responseModalities?.[0]?.toUpperCase() === "AUDIO";
  if (Object.hasOwn(body, "model") || voice !== "Kore" || text !== `native ${scenario}` ||
    body.fixture?.numeric !== 3 || !raw.includes('"fixture":{"numeric":3}')) {
    return fail(response, 422, "native request changed the fixture payload");
  }
  if (scenario === "known-gzip") {
    if (operation !== "gemini-2.5-pro-preview-tts:generateContent" || hasAudio ||
      request.headers["content-type"] !== "application/json; charset=UTF-8" ||
      url.searchParams.getAll("label").join() !== "a b,c+d" || url.searchParams.get("empty") !== "") {
      return fail(response, 422, "unexpected native model operation or query parameters");
    }
    return compressed(response, 201, completion(account), "application/json; charset=utf-8", "gzip-json");
  }
  if (!hasAudio) return fail(response, 422, "native response modality was dropped");
  if (scenario === "stream" || scenario === "stream-delayed") {
    if (operation !== "gemini-2.5-flash-preview-tts:streamGenerateContent" || url.searchParams.get("alt") !== "sse") {
      return fail(response, 422, "unexpected native stream operation");
    }
    response.writeHead(200, { "content-type": "text/event-stream", "cache-control": "no-cache",
      "access-control-allow-origin": "https://example.invalid", "x-fixture-upstream": "sse" });
    response.write(`data: ${completion(account).trimEnd()}\n\n`);
    if (scenario === "stream-delayed") await new Promise(resolve => setTimeout(resolve, 550));
    return response.end('data: {"candidates":[{"finishReason":"STOP"}]}\n\n');
  }
  if (scenario === "typed-audio") {
    if (operation !== "fixture-native-audio:generateContent" || body.generationConfig.responseModalities[0] !== "aUdIo") {
      return fail(response, 422, "unexpected audio modality passthrough");
    }
    return compressed(response, 200, completion(account), "application/json", "unknown-audio-model");
  }
  if (scenario === "upstream-error") {
    return compressed(response, 422, "\ufefffixture synthesis rejected\n", "text/plain; charset=utf-8", "gzip-error");
  }
  if (scenario === "rotate") {
    if (operation !== "fixture-native-rotate:generateContent") return fail(response, 422, "unexpected rotation model");
    return primary ? fail(response, 429, "fixture quota reached")
      : compressed(response, 200, completion(account), "application/json", "alternate-account");
  }
  if (scenario === "exhausted") return fail(response, 429, `${account} fixture quota reached`);
  if (scenario === "redaction") {
    return compressed(response, 422, `fixture key ${clientKey} and Bearer offline-native-tts-fake`, "text/plain", "redacted-error");
  }
  return fail(response, 422, "unknown native speech fixture");
}
