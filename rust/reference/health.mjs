import { registerHooks } from "node:module";

const hooks = registerHooks({
  resolve(specifier, context, nextResolve) {
    return nextResolve(specifier === "next/server" ? "next/server.js" : specifier, context);
  },
});
const health = await import("../../src/app/api/health/route.js");
hooks.deregister();

async function observe(response) {
  return {
    status: response.status,
    headers: Object.fromEntries(response.headers),
    body: await response.text(),
  };
}

export async function GET() {
  return observe(await health.GET());
}

export async function OPTIONS() {
  return observe(await health.OPTIONS());
}
