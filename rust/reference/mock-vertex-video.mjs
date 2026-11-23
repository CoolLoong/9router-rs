const base = "http://127.0.0.1";
const model = "veo-3.1-generate-preview";
const bearer = { token: "offline-vertex-video-bearer", prefix: "projects/fixture-video-bearer/locations/europe-west1/publishers/google/models/" + model };
const service = { token: "offline-vertex-sa-token", prefix: "projects/fixture-sa-project/locations/us-central1/publishers/google/models/" + model };
const pollCounts = new Map();
let submittedAmbiguous = false;

export function isVertexVideoMock(request) {
  const path = new URL(request.url, base).pathname;
  return request.method === "POST" && path.startsWith("/vertex/v1/") &&
    (path.endsWith(":predictLongRunning") || path.endsWith(":fetchPredictOperation"));
}

function json(response, status, object, mime = "application/json") {
  return response.writeHead(status, { "Content-Type": mime }).end(JSON.stringify(object));
}

function matches(object, expected) {
  if (object === expected) return true;
  if (object === null || expected === null || typeof object !== "object" || typeof expected !== "object" ||
    Array.isArray(object) !== Array.isArray(expected)) return false;
  return Object.keys(object).length === Object.keys(expected).length &&
    Object.entries(expected).every(([name, value]) => Object.hasOwn(object, name) && matches(object[name], value));
}

export async function handleVertexVideoMock(request, response) {
  const url = new URL(request.url, base);
  const token = request.headers.authorization?.replace(/^Bearer /, "");
  const account = token === bearer.token ? bearer : token === service.token ? service : null;
  const fail = message => json(response, 422, { error: { message } });
  if (!account || request.headers.accept !== "application/json" || request.headers["content-type"] !== "application/json" ||
      request.headers["idempotency-key"] || request.headers["x-goog-api-key"] || url.search) {
    return fail("unexpected Vertex video fixture headers");
  }
  let raw = "";
  for await (const chunk of request) raw += chunk;
  let payload;
  try { payload = JSON.parse(raw); } catch { return fail("invalid Vertex video fixture JSON"); }
  if (url.pathname.endsWith(":predictLongRunning")) {
    if (url.pathname !== "/vertex/v1/" + account.prefix + ":predictLongRunning") return fail("unexpected field precedence or path");
    const instances = payload.instances;
    if (!Array.isArray(instances) || instances.length !== 1) return fail("expected single fixture instance");
    const prompt = instances[0].prompt;
    if (prompt === "Vertex video primary fixture") {
      const expected = {
        instances: [{ prompt, image: { bytesBase64Encoded: "ZmFrZS1pbWFnZQ==", mimeType: "image/png" }, video: { gcsUri: "gs://example.invalid/input.mp4" } }],
        parameters: { sampleCount: 2, durationSeconds: 4, aspectRatio: "16:9", resolution: "720p", seed: "0009",
          negativePrompt: "no overlays", storageUri: "gs://example.invalid/output", generateAudio: false },
      };
      if (!matches(payload, expected)) return fail("unexpected Vertex generation instance or parameters");
      return json(response, 202, { name: account.prefix + "/operations/job-pending", done: false }, "application/vnd.fixture+json");
    }
    if (prompt === "Vertex video service fixture") {
      if (account !== service || !matches(payload, { instances: [{ prompt }] })) return fail("unexpected Vertex service payload");
      return json(response, 200, { name: account.prefix + "/operations/job-service" });
    }
    if (prompt === "Vertex video redaction fixture") {
      return fail("fixture message echoed " + token);
    }
    if (prompt === "Vertex video ambiguous fixture") {
      if (submittedAmbiguous) return json(response, 200, { repeated: true });
      submittedAmbiguous = true;
      return json(response, 502, { error: { message: "fixture may have accepted the job" } });
    }
    if (!prompt && account === bearer && matches(payload, { instances: [{ image: { gcsUri: "gs://example.invalid/reference.png" } }] })) {
      return json(response, 200, { resource: "opaque fixture" }, "application/vnd.fixture+json");
    }
    return fail("unexpected Vertex operation body");
  }

  const operation = payload.operationName;
  if (typeof operation !== "string" || !operation.startsWith(account.prefix + "/operations/") ||
      url.pathname !== "/vertex/v1/" + account.prefix + ":fetchPredictOperation" ||
      Object.keys(payload).length !== 1) {
    return fail("unexpected fixture operation or account affinity");
  }
  const job = operation.split("/").at(-1);
  if (job === "job-pending") {
    const next = (pollCounts.get(operation) || 0) + 1;
    pollCounts.set(operation, next);
    if (next === 1) return json(response, 200, { name: operation, done: false, metadata: { ignored: true } });
    return json(response, 200, { name: operation, done: true, response: { generateVideoResponse: { generatedSamples: [
      { video: { uri: "gs://example.invalid/result.mp4", mimeType: "video/mp4" } },
      { video: { bytesBase64Encoded: "ZmFrZS12aWRlbw==", mimeType: "video/webm" } },
      { uri: "gs://example.invalid/default.mp4" },
    ] } } });
  }
  if (job === "job-service") {
    return json(response, 200, { name: operation, done: true, response: { videos: [
      { gcsUri: "gs://example.invalid/service.mp4", bytesBase64Encoded: "ZmFrZS12aWRlbw==" },
    ] } });
  }
  if (job === "job-failed") {
    return json(response, 200, { name: operation, error: { code: 3, message: "fixture rejected" } });
  }
  if (job === "job-empty") {
    return json(response, 200, { name: operation, done: true, response: { videos: [], generateVideoResponse: {
      generatedSamples: [{ uri: "gs://example.invalid/should-not-appear.mp4" }],
    } } });
  }
  return fail("unexpected fixture poll ID");
}
