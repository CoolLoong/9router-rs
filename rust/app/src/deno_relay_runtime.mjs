import { pinnedHttps } from "./pinned-https.mjs";
import { forward } from "./relay.mjs";

export { pinnedHttps };

export function createHandler(env, send = pinnedHttps()) {
  return request => forward(request, env, send);
}

if (import.meta.main) {
  const env = {
    RELAY_AUTH: Deno.env.get("RELAY_AUTH"),
    RELAY_URL: Deno.env.get("RELAY_URL"),
  };
  Deno.serve(createHandler(env));
}
