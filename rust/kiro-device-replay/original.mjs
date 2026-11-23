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
        return next(info?.isDirectory()
            ? new URL(target.href + "/index.js").href
            : info ? target.href : target.href + ".js", context);
    },
});
const localFetch = globalThis.fetch;
const origin = "http://127.0.0.1:52989/kiro-device/";
const fixtureFetch = (input, options) => {
    const url = new URL(input);
    if (url.protocol !== "https:" || url.username || url.password || url.port || url.search || url.hash) {
        throw new Error("Fixture refused an unknown provider URL");
    }
    if (url.hostname === "codewhisperer.us-east-1.amazonaws.com" && url.pathname === "/ListAvailableProfiles") {
        return localFetch(origin + "profile", { ...options, redirect: "manual" });
    }
    const device = /^oidc\.([a-z]{2}-[a-z]+-[0-9]{1,2})\.amazonaws\.com$/.exec(url.hostname);
    const allowed = ["/client/register", "/device_authorization", "/token"];
    if (!device || !allowed.includes(url.pathname)) throw new Error("Fixture refused an unknown provider URL");
    return localFetch(origin + device[1] + url.pathname, { ...options, redirect: "manual" });
};

const route = await import("../../src/app/api/oauth/[provider]/[action]/route.js");
globalThis.fetch = fixtureFetch;
const server = http.createServer(async (req, res) => {
    if (req.headers["x-9r-cli-token"] !== "68977b6fd9422fbb") {
        res.writeHead(401, { "content-type": "application/json" }).end('{"error":"Unauthorized"}');
        return;
    }
    const address = new URL(req.url, "http://127.0.0.1");
    const action = address.pathname.split("/").at(-1);
    const chunks = [];
    for await (const chunk of req) chunks.push(chunk);
    const response = await route[req.method](new Request(address, {
        method: req.method,
        headers: { "content-type": "application/json" },
        body: req.method === "GET" ? undefined : Buffer.concat(chunks),
    }), { params: Promise.resolve({ provider: "kiro", action }) });
    res.writeHead(response.status, { "content-type": "application/json" }).end(await response.text());
});
process.prependOnceListener("SIGTERM", () => process.exit(0));
server.listen(0, "127.0.0.1", () => console.log("ready", server.address().port));
