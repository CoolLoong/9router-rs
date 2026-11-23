export function isEmbeddingMock(request) {
  if (request.method !== "POST") return false;
  const path = request.url.split("?")[0];
  return path === "/v1/embeddings" ||
    /^\/gemini\/v1beta\/models\/[^/]+:(?:embedContent|batchEmbedContents)$/.test(path);
}

function send(response, status, body) {
  response.writeHead(status, { "Content-Type": "application/json" });
  response.end(JSON.stringify(body));
}

export async function handleEmbeddingMock(request, response) {
  const url = new URL(request.url, "http://127.0.0.1");
  const gemini = url.pathname.startsWith("/gemini/");
  if (gemini ? url.searchParams.get("key") !== "offline-provider-placeholder"
    : request.headers.authorization !== "Bearer offline-models-provider") {
    return send(response, 401, { error: { message: "unexpected fixture credential" } });
  }

  let text = "";
  for await (const chunk of request) text += chunk;
  let body;
  try { body = JSON.parse(text); } catch {
    return send(response, 400, { error: { message: "invalid embedding fixture JSON" } });
  }
  const unexpected = (reason) => send(response, 422, { error: { message: reason } });
  if (gemini) {
    const model = "models/gemini-embedding-001";
    if (url.pathname.endsWith(":batchEmbedContents")) {
      const expected = ["batch α", "3", "false", "嵌套,2", "[object Object]", "null"];
      if (!Array.isArray(body.requests) || body.requests.length !== expected.length ||
        body.requests.some((item, index) => item.model !== model ||
          item.content?.parts?.[0]?.text !== expected[index] || item.outputDimensionality !== 3)) {
        return unexpected("unexpected Gemini batch fixture payload");
      }
      return send(response, 200, {
        embeddings: expected.map((_, index) => ({ values: [index, -(index + 1), index + 0.125] })),
      });
    }

    if (body.model !== model) return unexpected("unexpected Gemini embedding model");
    const input = body.content?.parts?.[0]?.text;
    if (input === "fixture passthrough" && !Object.hasOwn(body, "outputDimensionality")) {
      return send(response, 200, {
        object: "list",
        data: [{ object: "embedding", index: 0, embedding: [2, 3.5] }],
        model: "fixture-passthrough",
        usage: { prompt_tokens: 2, total_tokens: 2 },
      });
    }
    if (input !== "embedding fixture 中文" || body.outputDimensionality !== 4) {
      return unexpected("unexpected Gemini embedding fixture payload");
    }
    return send(response, 200, { embedding: { values: [0.25, -0.5, 0, 0.875] } });
  }

  if (body.model === "fixture-embedding-error") return unexpected("fixture embedding rejection");
  if (body.model !== "fixture-embedding" || Object.hasOwn(body, "user")) {
    return unexpected("unexpected OpenAI embedding fixture model or extension");
  }
  if (body.input === "fixture embedding" && body.encoding_format === "float" && body.dimensions === 3) {
    return send(response, 200, {
      object: "list",
      data: [{ object: "embedding", index: 0, embedding: [0.125, -0.25, 0.5] }],
      model: "fixture-embedding-upstream",
      usage: { prompt_tokens: 5, total_tokens: 5 },
    });
  }
  if (JSON.stringify(body.input) === JSON.stringify(["fixture embedding", "数组 🛰️"]) &&
    body.encoding_format === "base64" && !Object.hasOwn(body, "dimensions")) {
    return send(response, 200, {
      object: "list",
      data: [
        { object: "embedding", index: 0, embedding: "AACAPw==" },
        { object: "embedding", index: 1, embedding: "AAAAQA==" },
      ],
      model: "fixture-embedding-upstream",
      usage: { prompt_tokens: 7, completion_tokens: 0, total_tokens: 7 },
    });
  }
  return unexpected("unexpected OpenAI embedding fixture payload");
}
