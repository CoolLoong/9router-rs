import { createServer, request } from "node:http";
import { connect } from "node:net";

const intendedHost = "127.0.0.1:52985";
const backend = 52989;

const proxy = createServer((req, res) => {
  const url = URL.canParse(req.url) ? new URL(req.url) : null;
  if (!url || url.host !== intendedHost || req.method !== "POST" || url.pathname !== "/v1/chat/completions") {
    res.writeHead(400).end();
    return;
  }
  const headers = { ...req.headers };
  delete headers["proxy-authorization"];
  const outgoing = request({ host: "127.0.0.1", port: backend, method: req.method,
    path: url.pathname, headers }, upstream => {
    res.writeHead(upstream.statusCode, upstream.headers);
    upstream.pipe(res);
  });
  outgoing.on("error", () => res.destroy());
  req.pipe(outgoing);
});

proxy.on("connect", (req, client, head) => {
  if (req.url !== intendedHost) {
    client.end("HTTP/1.1 400 Bad Request\r\n\r\n");
    return;
  }
  const server = connect(backend, "127.0.0.1", () => {
    client.write("HTTP/1.1 200 Connection Established\r\n\r\n");
    if (head.length) server.write(head);
    client.pipe(server);
    server.pipe(client);
  });
  client.on("error", () => server.destroy());
  server.on("error", () => client.destroy());
});

proxy.listen(52987, "127.0.0.1").unref();
