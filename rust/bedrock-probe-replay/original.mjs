import fs from "node:fs";
import http from "node:http";
import { registerHooks } from "node:module";
import { fileURLToPath } from "node:url";

const [directory] = process.argv.slice(2);
process.env.DATA_DIR = directory;
registerHooks({
    resolve(specifier, context, next) {
        if (specifier === "node-machine-id") {
            return next("data:text/javascript,export const machineIdSync = () => 'fixture-machine';", context);
        }
        if (specifier === "next/server") return next("next/server.js", context);
        if (!specifier.startsWith("@/") && !specifier.startsWith("open-sse/")) return next(specifier, context);
        const relative = specifier.startsWith("@/") ? "src/" + specifier.slice(2) : specifier;
        const target = new URL("../../" + relative, import.meta.url);
        const info = fs.statSync(fileURLToPath(target), { throwIfNoEntry: false });
        return next(info?.isDirectory() ? new URL(target.href + "/index.js").href :
            info ? target.href : target.href + ".js", context);
    },
});
const localFetch = globalThis.fetch;
const fixture = (input, options) => {
    const url = new URL(input);
    const host = /^bedrock\.([a-z0-9][a-z0-9-]{0,30})\.amazonaws\.com$/.exec(url.hostname);
    if (!host || url.protocol !== "https:" || url.pathname !== "/foundation-models" ||
        url.search || options?.method !== "GET" || options?.body) {
        throw new Error("Fixture refused an unrecognized provider call");
    }
    return localFetch("http://127.0.0.1:52989/bedrock-probe/" + host[1] + url.pathname,
        { ...options, redirect: "error" });
};
globalThis.fetch = fixture;
const validate = await import("../../src/app/api/providers/validate/route.js");
const saved = await import("../../src/app/api/providers/[id]/test/route.js");
const batch = await import("../../src/app/api/providers/test-batch/route.js");
const models = await import("../../src/app/api/providers/[id]/models/route.js");
globalThis.fetch = fixture;
const server = http.createServer(async (req, res) => {
    if (req.headers["x-9r-cli-token"] !== "68977b6fd9422fbb") {
        res.writeHead(401, { "content-type": "application/json" }).end('{"error":"Unauthorized"}');
        return;
    }
    const address = new URL(req.url, "http://127.0.0.1");
    const leaf = address.pathname.split("/");
    const action = leaf.at(-1);
    const selected = { validate, test: saved, "test-batch": batch, models }[action];
    const payload = [];
    for await (const chunk of req) payload.push(chunk);
    const result = await selected[req.method](new Request(address, {
        method: req.method,
        headers: { "content-type": "application/json" },
        body: req.method === "GET" ? undefined : Buffer.concat(payload),
    }), {params: Promise.resolve({id: leaf.at(-2)})});
    res.writeHead(result.status, { "content-type": "application/json" }).end(await result.text());
});
process.prependOnceListener("SIGTERM", () => process.exit(0));
server.listen(0, "127.0.0.1", () => console.log("ready", server.address().port));
