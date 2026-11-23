const xai = "/xai/v1/videos";
const openrouter = "/openrouter/api/v1/videos";
const multipart = Buffer.concat([
  Buffer.from("--video-fixture\r\nContent-Disposition: form-data; name=\"image\"; filename=\"fake.bin\"\r\n\r\n"),
  Buffer.from([0, 127, 128, 255]),
  Buffer.from("\r\n--video-fixture--\r\n"),
]);
const multipartText = "--fixture\r\nContent-Disposition: form-data; name=\"prompt\"\r\n\r\ntext fixture\r\n--fixture--\r\n";
const models = new Map([
  ["text fixture", "grok-imagine-video"],
  ["edit fixture", "grok-imagine-video-edit"],
  ["extension fixture", "grok-imagine-video-extend"],
  ["openrouter fixture", "google/veo-3.1"],
  ["redaction fixture", "fixture-video-redaction"],
  ["affinity fixture", "fixture-video-affinity"],
  ["rotate fixture", "fixture-video-rotate"],
  ["unavailable fixture", "fixture-video-unavailable"],
  ["network fixture", "fixture-video-network"],
]);
const xaiAccounts = new Map([
  ["Bearer offline-xai-image", "primary"],
  ["Bearer offline-xai-video-secondary", "secondary"],
]);
const ambiguousCalls = new Set();

export function videoFixture(url) {
  const xaiPath = url?.origin === "https://api.x.ai" && url.pathname.startsWith("/v1/videos/");
  const openrouterPath = url?.origin === "https://openrouter.ai" &&
    (url.pathname === "/api/v1/videos" || url.pathname.startsWith("/api/v1/videos/"));
  if (!xaiPath && !openrouterPath) return null;
  return `http://127.0.0.1:52989/${xaiPath ? "xai" : "openrouter"}${url.pathname}${url.search}`;
}

export function isVideoMock(request) {
  const path = new URL(request.url, "http://127.0.0.1").pathname;
  return path.startsWith(xai + "/") || path === openrouter || path.startsWith(openrouter + "/");
}

function json(response, status, data) {
  return response.writeHead(status, { "Content-Type": "application/json" }).end(JSON.stringify(data));
}

export async function handleVideoMock(request, response) {
  const path = new URL(request.url, "http://127.0.0.1").pathname;
  const isOpenrouter = path === openrouter || path.startsWith(openrouter + "/");
  const account = isOpenrouter ? "openrouter" : xaiAccounts.get(request.headers.authorization);
  if (request.headers.accept !== "application/json" || !account || (isOpenrouter && (
    request.headers.authorization !== "Bearer offline-static-openrouter"
    || request.headers["http-referer"] !== "https://endpoint-proxy.local"
    || request.headers["x-title"] !== "Endpoint Proxy"
    || request.headers["idempotency-key"]
  ))) {
    return json(response, 401, { error: "fixture provider headers" });
  }
  if (request.method === "GET") {
    if (request.headers["content-type"] || request.headers["idempotency-key"]) {
      return json(response, 422, { error: "unexpected GET entity headers" });
    }
    if (path === openrouter + "/fixture-openrouter-job") {
      return json(response, 200, { id: "fixture-openrouter-job", status: "succeeded", unsigned_urls: ["https://example.invalid/fixture.mp4"] });
    }
    if (path === xai + "/fixture-job%2Fpart%3Fq") {
      return json(response, 200, { request_id: "fixture-job/part?q", status: "completed", video: { url: "https://example.invalid/fixture.mp4" }, account });
    }
    if (path === xai + "/fixture-pending") {
      return json(response, 200, { status: "pending", account });
    }
    if (path === xai + "/fixture-poll-no-rotation" && account === "secondary") {
      return json(response, 429, { error: "fixture polling rate limited" });
    }
    if (path === xai + "/fixture-poll-no-rotation") {
      return json(response, 200, { status: "unexpected rotation" });
    }
    return json(response, 404, { error: "unknown fixture job" });
  }
  if (request.method !== "POST") return json(response, 405, { error: "fixture method" });

  const chunks = [];
  for await (const bytes of request) chunks.push(bytes);
  const bytes = Buffer.concat(chunks);
  const contentType = request.headers["content-type"];
  const action = path.split("/").at(-1);
  const multipartOk = path === xai + "/generations" && (
    (contentType === "multipart/form-data; boundary=video-fixture" && bytes.equals(multipart)) ||
    (contentType === "multipart/form-data; boundary=fixture" && bytes.equals(Buffer.from(multipartText)))
  );
  if (multipartOk) {
    return json(response, 202, { request_id: "fixture-multipart", status: "pending", account });
  }
  if (!contentType?.includes("application/json")) return json(response, 422, { error: "fixture content type or multipart bytes" });
  let body;
  try { body = JSON.parse(bytes.toString()); } catch { return json(response, 422, { error: "fixture JSON" }); }
  if (models.get(body.prompt) !== body.model) return json(response, 422, { error: "fixture model resolution" });
  if (isOpenrouter) {
    if (body.prompt !== "openrouter fixture" || path !== openrouter || contentType !== "application/json") {
      return json(response, 422, { error: "fixture OpenRouter creation request" });
    }
    return json(response, 201, { id: "fixture-openrouter-job", status: "queued" });
  }
  if (action === "edits" && body.prompt === "edit fixture" && body.video_url === "https://example.invalid/video.mp4") {
    return json(response, 202, { request_id: "fixture-edit", status: "pending", account });
  }
  if (action === "extensions" && body.prompt === "extension fixture" && body.seconds === 3) {
    return json(response, 202, { request_id: "fixture-extension", status: "pending", account });
  }
  if (action !== "generations") return json(response, 422, { error: "fixture xAI action" });
  if (body.prompt === "redaction fixture") {
    return response.writeHead(422, { "Content-Type": "text/plain" })
      .end(`bEaReR\tfixture-Bearer.token_123 sent ${request.headers.authorization.slice(7)}`);
  }
  if (body.prompt === "text fixture") {
    const expected = request.headers["idempotency-key"] === "fixture-video-once"
      ? '{"model":"grok-imagine-video","prompt":"text fixture","seconds":5}'
      : '{ "model": "grok-imagine-video", "prompt": "text fixture" }';
    if (bytes.toString() !== expected) return json(response, 422, { error: "fixture JSON bytes or idempotency key" });
    return response.writeHead(202, { "Content-Type": "application/vnd.fixture+json" })
      .end('{ "request_id": "fixture-job/part?q", "status": "pending", "account": "' + account + '" }\n');
  }
  if (account === "primary" && ["unavailable fixture", "network fixture"].includes(body.prompt)
    && !ambiguousCalls.has(body.prompt)) {
    ambiguousCalls.add(body.prompt);
    if (body.prompt === "unavailable fixture") {
      return json(response, 502, { error: "fixture may have accepted job" });
    }
    return response.destroy();
  }
  if (body.prompt === "rotate fixture" && account === "primary") {
    return json(response, 429, { error: "fixture creation rate limited" });
  }
  return json(response, 202, { request_id: "fixture-job/part?q", status: "pending", account });
}
