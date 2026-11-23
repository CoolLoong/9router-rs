import { forward } from "../lib/relay.mjs";
import { pinnedHttps } from "../lib/pinned-https.mjs";

const hostPattern = /^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?\.vercel\.app$/;

export function createHandler(env, send = pinnedHttps()) {
  const host = env.VERCEL_PROJECT_PRODUCTION_URL;
  const bindings = env.VERCEL_ENV === "production" && typeof host === "string" && hostPattern.test(host)
    ? { RELAY_AUTH: env.RELAY_AUTH, RELAY_URL: "https://" + host + "/api/relay" }
    : {};
  return request => forward(request, bindings, send);
}

const handle = createHandler(process.env);
export default {
  fetch(request) {
    return handle(request);
  },
};
