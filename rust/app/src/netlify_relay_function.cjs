const limit = 4 * 1024 * 1024;

function error(statusCode, text) {
  return {
    statusCode,
    headers: { "content-type": "application/json", "cache-control": "no-store" },
    body: JSON.stringify({ error: text }),
  };
}

async function buffered(response) {
  if (!response.body) return "";
  const reader = response.body.getReader();
  try {
    const chunks = [];
    let length = 0;
    while (true) {
      const entry = await reader.read();
      if (entry.done) break;
      length += entry.value.byteLength;
      if (length > limit) {
        await reader.cancel().catch(() => {});
        return null;
      }
      chunks.push(Buffer.from(entry.value));
    }
    return Buffer.concat(chunks, length).toString("base64");
  } finally {
    reader.releaseLock();
  }
}

function createHandler(env, outbound) {
  return async event => {
    try {
      const [{ forward }, { pinnedHttps }] = await Promise.all([
        import("./relay.mjs"), import("./pinned-https.mjs"),
      ]);
      const method = event.httpMethod;
      if (typeof event.rawUrl !== "string" || typeof method !== "string") {
        return error(400, "Invalid relay request");
      }
      const body = method !== "GET" && method !== "HEAD" && event.body
        ? Buffer.from(event.body, event.isBase64Encoded ? "base64" : "utf8")
        : undefined;
      if (body && body.length > limit) return error(413, "Relay request is too large");
      const request = new Request(event.rawUrl, {
        method, headers: event.headers, body, signal: AbortSignal.timeout(55000),
      });
      const response = await forward(request, env, outbound ?? pinnedHttps());
      const content = await buffered(response);
      if (content === null) return error(502, "Relay response is too large");
      return {
        statusCode: response.status,
        headers: Object.fromEntries(response.headers),
        body: content,
        isBase64Encoded: true,
      };
    } catch {
      return error(502, "Relay request could not be completed");
    }
  };
}

exports.createHandler = createHandler;
exports.handler = createHandler(process.env);
