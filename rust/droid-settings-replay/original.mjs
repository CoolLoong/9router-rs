import http from "node:http";
import { registerHooks } from "node:module";

registerHooks({
  resolve(specifier, context, next) {
    return next(specifier === "next/server" ? "next/server.js" : specifier, context);
  },
});
const handlers = await import("../../src/app/api/cli-tools/droid-settings/route.js");
const server = http.createServer(async (request, response) => {
  let result;
  if (request.headers["x-replay-admin"] !== "fixture") {
    result = Response.json({error:"Unauthorized"},{status:401});
  } else {
    const bytes = [];
    for await (const chunk of request) bytes.push(chunk);
    const forwarded = new Request("http://127.0.0.1"+request.url, {
      method:request.method,
      ...(!["GET","HEAD"].includes(request.method) && bytes.length ? {body:Buffer.concat(bytes)} : {}),
    });
    result = await handlers[request.method](forwarded);
  }
  response.writeHead(result.status,{"Content-Type":"application/json"});
  response.end(await result.text());
});
process.prependOnceListener("SIGTERM", () => process.exit(0));
server.listen(0,"127.0.0.1",()=>console.log("ready",server.address().port));
