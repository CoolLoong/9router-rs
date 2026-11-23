import { registerHooks } from "node:module";

const catalogModule = new URL("../../open-sse/config/providerModels.js", import.meta.url);
const hooks = registerHooks({
  resolve(specifier, context, nextResolve) {
    return nextResolve(
      specifier === "@/shared/constants/models" ? catalogModule.href : specifier,
      context,
    );
  },
});
const routes = await import("../../src/app/api/v1beta/models/route.js");
hooks.deregister();

async function observe(response) {
  return {
    status: response.status,
    headers: Object.fromEntries(response.headers),
    body: await response.text(),
  };
}

export async function GET() {
  return observe(await routes.GET());
}

export async function OPTIONS() {
  return observe(await routes.OPTIONS());
}
