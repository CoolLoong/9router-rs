import { appendFileSync } from "node:fs";
import { join } from "node:path";

const fixture = "http://127.0.0.1:52989";
const sd = "http://localhost:7860";
const comfy = "http://localhost:8188";
const sdPath = "/sdapi/v1/txt2img";
const artifactPath = "/fixture-local-artifact";
const encoded = text => Buffer.from(text).toString("base64");

export function localImageFixture(url) {
  if (url?.origin === sd && url.pathname === sdPath) return fixture + "/sdwebui" + sdPath;
  if (url?.origin === comfy && ["/", artifactPath].includes(url.pathname)) return fixture + "/comfyui" + url.pathname + url.search;
  return null;
}

export function isLocalImageMock(request) {
  const path = new URL(request.url, fixture).pathname;
  return path.startsWith("/sdwebui/") || path.startsWith("/comfyui/");
}

function json(response, status, object) {
  return response.writeHead(status, { "content-type":"application/json" }).end(JSON.stringify(object));
}

export async function handleLocalImageMock(request, response) {
  const url = new URL(request.url, fixture);
  const path = url.pathname;
  const fail = message => json(response, 422, { error:{ message } });
  if (process.env.DATA_DIR) appendFileSync(join(process.env.DATA_DIR, "fixture-local-image-requests"),
    JSON.stringify({ method:request.method, path, query:url.search }) + "\n");
  if (request.headers.authorization || request.headers["x-api-key"] || request.headers["x-goog-api-key"] || request.headers.cookie) {
    return fail("unexpected fixture credential");
  }
  if (path === "/comfyui" + artifactPath && request.method === "GET" && url.search === "?name=demo") {
    return response.writeHead(200, { "content-type":"application/octet-stream" }).end(Buffer.from("fixture staged artifact\0"));
  }
  if (request.method !== "POST" || (path !== "/sdwebui" + sdPath && path !== "/comfyui/") ||
      request.headers["content-type"] !== "application/json" || url.search) return fail("unexpected stage or endpoint");
  let raw = "";
  for await (const chunk of request) raw += chunk;
  let payload;
  try { payload = JSON.parse(raw); } catch { return fail("expected structured fixture input"); }

  if (path.startsWith("/sdwebui")) {
    if (Object.keys(payload).sort().join() !== "batch_size,height,prompt,steps,width" || payload.steps !== 20) {
      return fail("unexpected SD input shape");
    }
    const prompt = payload.prompt;
    if (prompt === "Fixture SD defaults" && payload.width === 1024 && payload.height === 1024 && payload.batch_size === 1) {
      return json(response, 200, { images:[encoded("first fake SD image"), encoded("second fake SD image")], info:"ignored" });
    }
    if (prompt === "Fixture SD binary" && payload.width === 512 && payload.height === 720 && payload.batch_size === null) {
      return json(response, 200, { images:[encoded("fake SD binary\0")] });
    }
    if (prompt === "Fixture SD empty") return json(response, 200, { images:null, info:"ignored" });
    if (prompt === "Fixture SD rejection") return json(response, 429, { error:{ message:"fixture generation is busy" } });
    return fail("unexpected SD fixture parameters");
  }

  if (Object.keys(payload).join() !== "prompt") return fail("unexpected Comfy input shape");
  const prompt = payload.prompt;
  if (prompt?.["3"]?.class_type === "KSampler" && prompt["3"].inputs.seed === 7) {
    return json(response, 202, { prompt_id:"fixture-queued", number:2, node_errors:{} });
  }
  const created = 1795046400;
  if (prompt === "Fixture Comfy inline") {
    return json(response, 200, { created, data:[{ b64_json:encoded("fake inline Comfy image"), revised_prompt:"synthetic" }], receipt:"opaque" });
  }
  if (prompt === "Fixture Comfy artifact") {
    return json(response, 200, { created, data:[{ url:comfy + artifactPath + "?name=demo" }] });
  }
  if (prompt === "Fixture Comfy off-origin") {
    return json(response, 200, { created, data:[{ url:"https://example.invalid/fixture-only" }] });
  }
  if (prompt === "Fixture Comfy invalid JSON") {
    return response.writeHead(200, { "content-type":"application/json" }).end("");
  }
  return fail("unexpected Comfy fixture prompt");
}
