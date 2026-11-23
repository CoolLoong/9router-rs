const tokenPattern = /^[A-Za-z0-9_-]{43}$/;
const label = /^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$/;
const internalSuffixes = new Set(["localhost", "local", "internal", "invalid", "test", "example", "onion", "arpa"]);
const methods = new Set(["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"]);
const hopHeaders = [
  "host", "connection", "keep-alive", "proxy-connection", "proxy-authorization",
  "proxy-authenticate", "te", "trailer", "transfer-encoding", "upgrade", "http2-settings",
  "forwarded", "cdn-loop",
];

function authorized(request, expected) {
  const token = request.headers.get("x-relay-auth");
  if (!token || !tokenPattern.test(token) || typeof expected !== "string" || !tokenPattern.test(expected)) {
    return false;
  }
  let different = 0;
  for (let i = 0; i < token.length; i++) {
    different |= token.charCodeAt(i) ^ expected.charCodeAt(i);
  }
  return different === 0;
}

function destination(request) {
  const target = request.headers.get("x-relay-target");
  const path = request.headers.get("x-relay-path");
  if (!target || !path || !path.startsWith("/") || path.startsWith("//") || /[\\#\x00-\x20\x7f]/.test(path)) {
    return null;
  }
  try {
    const origin = new URL(target);
    const labels = origin.hostname.split(".");
    const suffix = labels.at(-1);
    if (origin.protocol !== "https:" || origin.port || origin.username || origin.password ||
        origin.pathname !== "/" || origin.search || origin.hash ||
        labels.length < 2 || !labels.every(part => label.test(part)) ||
        !/[a-z]/.test(suffix) || internalSuffixes.has(suffix)) {
      return null;
    }
    const url = new URL(path, origin);
    return url.origin === origin.origin && url.origin !== new URL(request.url).origin ? url : null;
  } catch {
    return null;
  }
}

function headers(source) {
  const result = new Headers(source);
  for (const name of (result.get("connection") || "").split(",")) {
    if (/^[!#$%&'*+.^_`|~0-9A-Za-z-]+$/.test(name.trim())) result.delete(name.trim());
  }
  for (const name of hopHeaders) result.delete(name);
  for (const name of [...result.keys()]) {
    if (name.startsWith("x-relay-") || name.startsWith("x-forwarded-") ||
        name.startsWith("cf-") || name.startsWith("x-vercel-")) {
      result.delete(name);
    }
  }
  return result;
}

function reject(status, error) {
  return new Response(JSON.stringify({ error }), {
    status,
    headers: { "content-type": "application/json", "cache-control": "no-store" },
  });
}

export async function forward(request, env, send = fetch) {
    if (!authorized(request, env.RELAY_AUTH) || !URL.canParse(env.RELAY_URL) ||
        request.url !== new URL(env.RELAY_URL).href) {
      return reject(403, "Relay authorization required");
    }
    if (!methods.has(request.method)) return reject(405, "Relay method is not supported");
    const url = destination(request);
    if (!url) return reject(400, "A public HTTPS destination is required");

    try {
      const init = {
        method: request.method,
        headers: headers(request.headers),
        redirect: "manual",
        cache: "no-store",
        signal: request.signal,
      };
      if (request.method !== "GET" && request.method !== "HEAD" && request.body) {
        init.body = request.body;
        init.duplex = "half";
      }
      const response = await send(url, init);
      if (response.status >= 300 && response.status < 400) {
        await response.body?.cancel().catch(() => {});
        return reject(502, "Relay upstream returned a redirect");
      }
      const forwarded = headers(response.headers);
      for (const name of ["cdn-cache-control", "cloudflare-cdn-cache-control", "surrogate-control"]) {
        forwarded.delete(name);
      }
      forwarded.set("cache-control", "no-store");
      return new Response(response.body, { status: response.status, headers: forwarded });
    } catch {
      return reject(502, "Relay upstream request failed");
    }
}

export default {
  fetch(request, env) {
    return forward(request, env);
  },
};
