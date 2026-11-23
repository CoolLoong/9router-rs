import { appendFileSync, existsSync, readFileSync } from "node:fs";
import { join } from "node:path";

const upstream = "https://models.dev/api.json";
const fixture = "http://127.0.0.1:52989/models-dev/api.json";
const initialTag = '"catalog-fixture-1"';
const updatedTag = '"catalog-fixture-2"';

const initial = {
  google: { models: {
    "vendor/GEMINI-2.5-FLASH:free": { modalities: { input: ["text", "image", "audio"] }, limit: { context: 1200000, output: 17000 }, reasoning: true },
    "gemini-2.5-flash": { modalities: { input: ["text", "pdf"] }, limit: { context: 5000000, output: 6000 }, reasoning: false },
  } },
  kiro: { models: { "gpt-5.6-sol": { modalities: { input: ["text", "image"] }, limit: { context: 2000000, output: 4321 } } } },
  anthropic: { models: { "claude-sonnet-4-6": { modalities: { input: ["text", "pdf"] } } } },
  "fixture-upstream": { models: { "vendor/Special:large": { modalities: { input: ["text", "audio", "video", "unknown"] }, limit: { context: 2500 } } } },
};
const updated = {
  google: { models: { "gemini-2.5-flash": { modalities: { input: ["text", "video"] }, limit: { context: 3050000, output: 1000 } } } },
};

export function catalogSyncFixture(url) {
  return url?.href === upstream ? fixture : null;
}

export function isCatalogSyncMock(request) {
  return request.method === "GET" && request.url === "/models-dev/api.json";
}

export async function handleCatalogSyncMock(request, response) {
  const path = name => join(process.env.DATA_DIR, "catalog-fixture-" + name);
  if (!process.env.DATA_DIR || request.headers.accept !== "application/json") {
    return response.writeHead(422).end();
  }
  let mode = "initial";
  try { mode = readFileSync(path("mode"), "utf8").trim(); } catch {}
  const tag = request.headers["if-none-match"] ?? null;
  appendFileSync(path("requests"), JSON.stringify({ mode, tag }) + "\n");
  if (mode === "hold") {
    const start = Date.now();
    while (!existsSync(path("release")) && Date.now() - start < 3500) {
      await new Promise(resolve => setTimeout(resolve, 10));
    }
    if (!existsSync(path("release"))) return response.writeHead(503).end();
  }
  if (mode === "fail") return response.writeHead(503).end();
  if (mode === "invalid") {
    return response.writeHead(200, { "content-type": "application/json" }).end("[");
  }
  const resultTag = mode === "updated" ? updatedTag : initialTag;
  if (tag === resultTag) {
    return response.writeHead(304, { etag: resultTag }).end();
  }
  return response.writeHead(200, { etag: resultTag, "content-type": "application/json" })
    .end(JSON.stringify(mode === "updated" ? updated : initial));
}
