import fs from "node:fs";
import http from "node:http";
import { registerHooks } from "node:module";
import { fileURLToPath } from "node:url";

const [data] = process.argv.slice(2);
process.env.DATA_DIR = data;
registerHooks({
  resolve(specifier, context, next) {
    if (specifier === "node-machine-id") {
      return next("data:text/javascript,export const machineIdSync = () => 'fixture-machine';", context);
    }
    if (specifier === "next/server") return next("next/server.js", context);
    if (!specifier.startsWith("@/") && !specifier.startsWith("open-sse/")) return next(specifier, context);
    const relative = specifier.startsWith("@/") ? `src/${specifier.slice(2)}` : specifier;
    const target = new URL(`../../${relative}`, import.meta.url);
    const info = fs.statSync(fileURLToPath(target), { throwIfNoEntry: false });
    return next(info?.isDirectory() ? new URL(`${target.href}/index.js`).href :
      info ? target.href : `${target.href}.js`, context);
  },
});

const usage = await import("../../src/app/api/usage/[connectionId]/route.js");
const server = http.createServer(async (request, response) => {
  const uri = new URL(request.url, "http://127.0.0.1");
  const connectionId = uri.pathname.split("/").at(-1);
  const result = await usage.GET(new Request(uri, { headers: request.headers }), { params: { connectionId } });
  response.writeHead(result.status, { "content-type": "application/json" });
  response.end(await result.text());
});
process.prependOnceListener("SIGTERM", () => process.exit(0));
server.listen(0, "127.0.0.1", () => console.log("ready", server.address().port));
