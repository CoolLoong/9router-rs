const OPENAI = "/openai/v1/images/generations";
const XAI = "/xai/v1/images/generations";
const GEMINI = "/gemini/v1beta/models/gemini-2.5-flash-image:generateContent";
const CLOUDFLARE_ACCOUNT = "0123456789abcdef0123456789abcdef";
const CLOUDFLARE_ROUTE = `/client/v4/accounts/${CLOUDFLARE_ACCOUNT}/ai/run/`;
const CLOUDFLARE = `/cloudflare-image${CLOUDFLARE_ROUTE}`;
const FAL = "/fal-image";
const FAL_ORIGIN = "https://queue.fal.run";
const falJobs = new Map();
const falLedger = [];
const STATIC = new Map([
  ["https://api.openai.com/v1/images/generations", OPENAI],
  ["https://api.x.ai/v1/images/generations", XAI],
]);

export function imageGenerationFixture(url) {
  if (url?.origin === FAL_ORIGIN) return "http://127.0.0.1:52989" + FAL + url.pathname + url.search;
  if (url?.origin === "https://api.cloudflare.com" && url.pathname.startsWith(CLOUDFLARE_ROUTE)) {
    return "http://127.0.0.1:52989/cloudflare-image" + url.pathname + url.search;
  }
  const path = STATIC.get(url?.href);
  return path ? "http://127.0.0.1:52989" + path : null;
}

export function isImageGenerationMock(request) {
  const url = new URL(request.url, "http://127.0.0.1");
  if (url.pathname.startsWith(`${FAL}/`)) return ["GET","POST"].includes(request.method);
  if (request.method !== "POST") return false;
  if (url.pathname === GEMINI) return url.searchParams.has("key");
  if (url.pathname.startsWith(CLOUDFLARE)) return true;
  return url.pathname === OPENAI || url.pathname === XAI;
}

function send(response, status, body) {
  return response.writeHead(status, { "Content-Type": "application/json" }).end(JSON.stringify(body));
}

const b64 = (text) => Buffer.from(text).toString("base64");

async function falMock(request, response, pathname) {
  const path = pathname.slice(FAL.length);
  if (path === "/ledger") return send(response,200,falLedger);
  if (path === "/capture") {
    falLedger.push({ method:"CAPTURE", carriesCredential:Boolean(request.headers.authorization) });
    return send(response,403,{});
  }
  const account = { "Key offline-fal-image":"primary", "Key offline-fal-backup":"backup" }[request.headers.authorization];
  const fail = (status, message) => send(response,status,{message});
  if (!account || request.headers.cookie || request.headers["x-gateway-secret"]) {
    return fail(401,"unexpected Fal fixture credential");
  }
  if (request.method === "POST") {
    let raw = "";
    for await (const chunk of request) raw += chunk;
    const body = JSON.parse(raw);
    falLedger.push({ method:"POST", path, account, body });
    if (!["/fal-ai/flux/schnell", "/fal-ai/flux/dev"].includes(path) || body.num_images !== 2) {
      return fail(422,"unexpected Fal fixture request");
    }
    if (body.image_size !== "2:3" && !(body.image_size?.width === 1024 && body.image_size?.height === 1536)) {
      return fail(422,"unexpected Fal request dimensions");
    }
    if (body.prompt === "Fal reject before submit" && account === "primary") return fail(401,"synthetic credential rejected");
    const id = `image-${falJobs.size + 1}`;
    const resultPath = `/fal-ai/flux/requests/${id}`;
    falJobs.set(id,{prompt:body.prompt,account,polls:0});
    return send(response,200,{
      request_id:id,
      status_url:FAL_ORIGIN + resultPath + "/status",
      response_url:(body.prompt === "Fal foreign job" ? "https://foreign.example.invalid" : FAL_ORIGIN) + resultPath,
    });
  }
  const match = path.match(/^\/fal-ai\/flux\/requests\/(image-\d+)(\/status)?$/);
  const id = match?.[1];
  const job = falJobs.get(id);
  if (!job || account !== job.account) return fail(403,"unexpected Fal fixture job identity");
  falLedger.push({ method:"GET", path, account });
  if (match[2]) {
    if (job.prompt === "Fal status redirect") return response.writeHead(307,{Location:`${FAL}/capture`}).end();
    job.polls += 1;
    return send(response,200,{ request_id:id,status:job.polls === 1 ? "IN_QUEUE" : "COMPLETED" });
  }
  if (job.polls < 2) return fail(409,"Fal fixture result requested before completion");
  return send(response,200,{images:job.prompt === "Fal accepted empty" ? [] : [{url:"https://image.example.invalid/fal-finished.png"}]});
}

export async function handleImageGenerationMock(request, response) {
  const url = new URL(request.url, "http://127.0.0.1");
  const fail = (status, message) => send(response, status, { error: { message } });
  if (url.pathname.startsWith(`${FAL}/`)) return falMock(request,response,url.pathname);
  if (url.pathname.startsWith(CLOUDFLARE)) {
    if (request.headers.authorization !== "Bearer offline-cloudflare-image" || url.search) {
      return fail(401, "unexpected Cloudflare image credential");
    }
    const model = url.pathname.slice(CLOUDFLARE.length);
    const contentType = request.headers["content-type"] || "";
    const chunks = [];
    for await (const chunk of request) chunks.push(chunk);
    const raw = Buffer.concat(chunks);
    if (model === "@cf/black-forest-labs/flux-2-dev") {
      if (!contentType.startsWith("multipart/form-data;")) return fail(422,"unexpected Cloudflare form content type");
      const form = await new Request("http://image.fixture.invalid", { method:"POST", headers:{"content-type":contentType}, body:raw }).formData();
      if (form.get("prompt") !== "offline flux" || form.get("width") !== "768" || form.get("height") !== "896"
        || form.get("steps") !== "25" || form.get("seed") !== "0" || Array.from(form).length !== 5) {
        return fail(422,"unexpected Cloudflare multipart payload");
      }
      return send(response,200,{result:{responses:[{success:false,result:"exclude"},
        {success:true,result:{image:`data:image/png;base64,${b64("Cloudflare Flux image")}`}}]}});
    }
    if (contentType !== "application/json" || model !== "@cf/runwayml/stable-diffusion-v1-5-inpainting") {
      return fail(422,"unexpected Cloudflare JSON transport");
    }
    const body = JSON.parse(raw);
    if (body.prompt === "offline workers text") {
      if (body.width !== 640 || body.height !== 768 || body.num_steps !== 4 || Object.keys(body).length !== 4) {
        return fail(422,"unexpected Cloudflare image dimensions");
      }
      return send(response,200,{result:{image:b64("Cloudflare text image")}});
    }
    if (body.prompt === "offline workers mask") {
      const equals = (entry, expected) => JSON.stringify(entry) === JSON.stringify(expected);
      if (!equals(body.image,[7,8,9]) || body.image_b64 !== "BwgJ" || !equals(body.mask,[0,1,255])
        || !equals(body.mask_image,[0,1,255]) || body.mask_b64 !== "AAH/" || body.strength !== 0.5) {
        return fail(422,"unexpected Cloudflare image or mask body");
      }
      return send(response,200,{result:{image:b64("Cloudflare edited image")}});
    }
    if (body.prompt === "offline workers binary") {
      return response.writeHead(200,{"Content-Type":"image/png"}).end("Cloudflare image bytes");
    }
    if (body.prompt === "offline workers error") {
      return fail(403,"synthetic Cloudflare permission denied");
    }
    return fail(422,"unexpected Cloudflare image request");
  }
  const gemini = url.pathname === GEMINI;
  const key = url.pathname === XAI ? "offline-xai-image" : "offline-openai-speech";
  if (gemini ? url.searchParams.get("key") !== "offline-provider-placeholder" || request.headers.authorization
    : request.headers.authorization !== "Bearer " + key) {
    return fail(401, "unexpected fixture image credential");
  }
  let raw = "";
  for await (const chunk of request) raw += chunk;
  let body;
  try { body = JSON.parse(raw); } catch {
    return fail(400, "invalid fixture image JSON");
  }
  if (url.pathname === OPENAI && body.prompt === "Image combo rotation fixture") {
    return send(response, 200, {
      created: 1784419200, data: [{ b64_json: b64(body.model) }],
    });
  }

  if (gemini) {
    const parts = body.contents?.[0]?.parts;
    const prompt = parts?.at(-1)?.text;
    const image = parts?.[0]?.inlineData;
    if (!parts || JSON.stringify(body.generationConfig) !== '{"responseModalities":["TEXT","IMAGE"]}'
      || Object.keys(body).sort().join() !== "contents,generationConfig") {
      return fail(422, "unexpected Gemini image fixture shape");
    }
    const uri = prompt === "Gemini image with data URI" && parts.length === 2 &&
      image?.mimeType === "image/jpeg" && image.data === b64("fake uploaded jpeg");
    const raw = prompt === "Gemini inline base64 array" && parts.length === 2 &&
      image?.mimeType === "image/png" && image.data === b64("fake-image-reference-".repeat(10));
    const textual = prompt === "Gemini image text only" && parts.length === 1 && !image;
    if (!uri && !raw && !textual) return fail(422, "unexpected Gemini fixture input precedence");
    return send(response, 200, {
      candidates: [{ content: { parts: [
        { text: "fixture caption ignored" },
        { inlineData: { mimeType: "image/png", data: b64(uri ? "fixture Gemini webp bytes" : "fixture Gemini png bytes") } },
        { inlineData: { mimeType: "image/jpeg", data: b64("second fixture image ignored") } },
      ] } }],
      usageMetadata: { promptTokenCount: 5, candidatesTokenCount: 7, totalTokenCount: 12 },
    });
  }

  if (url.pathname === XAI) {
    if (Object.keys(body).join() !== "model,prompt,n,response_format" || body.model !== "grok-imagine-image"
      || body.prompt !== "xAI whitelist fixture" || body.n !== 2 || body.response_format !== "b64_json") {
      return fail(422, "unexpected xAI body field whitelist");
    }
    return send(response, 200, {
      created: 1784419200, data: [{ b64_json: b64("fake xAI fixture image") }],
    });
  }

  if (body.model === "fixture-image-failure") {
    return fail(422, "image provider rejected fixture prompt");
  }
  if (body.model === "gpt-image-2.5" && body.prompt === "OpenAI fixture image with usage" &&
      body.n === 2 && body.size === "512x512" && body.quality === "high" && body.style === "vivid" &&
      body.output_format === "png" && body.response_format === "b64_json" &&
      Object.keys(body).sort().join() === "model,n,output_format,prompt,quality,response_format,size,style") {
    return send(response, 200, {
      created: 1784419200,
      data: [
        { b64_json: b64("fake OpenAI image 1"), revised_prompt: "fixture revision 1" },
        { b64_json: b64("fake OpenAI image 2"), revised_prompt: "fixture revision 2" },
      ],
      usage: { input_tokens: 7, output_tokens: 11, total_tokens: 18, input_tokens_details: { image_tokens: 0, text_tokens: 7 } },
    });
  }
  if (body.model === "gpt-image-2.5" && body.prompt === "OpenAI fixture JPEG" &&
      body.n === 1 && body.size === "1024x1024" && body.output_format === "JpEg" &&
      body.response_format === "b64_json" && !Object.hasOwn(body, "quality") && !Object.hasOwn(body, "style")) {
    return send(response, 200, { created: 1784419200, data: [{ b64_json: b64("fixture OpenAI jpeg bytes") }] });
  }
  if (body.model === "dall-e-3" && body.prompt === "OpenAI fixture URL" && body.response_format === "url" &&
      body.n === null && body.size === null && !Object.hasOwn(body, "output_format")) {
    return send(response, 200, {
      created: 1784419200, data: [{ url: "https://example.invalid/fixture.png", revised_prompt: "fixture URL revision" }],
    });
  }
  return fail(422, "unexpected OpenAI fixture image payload");
}
