const meta = "com.anthropic.api/mcp-registry";

function item(name, url, config = {}, type = "streamable-http") {
  return {
    server: { name, remotes: [{ url, type }] },
    _meta: { [meta]: config },
  };
}

export function coworkRegistryFixture(url) {
  return url?.origin === "https://api.anthropic.com" && url.pathname === "/mcp-registry/v0/servers"
    ? `http://127.0.0.1:52989/cowork-registry${url.search}` : null;
}

export function isCoworkDiscoveryMock(request) {
  return request.url.startsWith("/cowork-registry?") || request.url.startsWith("/cowork-mcp-fixture/");
}

export async function handleCoworkDiscoveryMock(request, response) {
  const url = new URL(request.url, "http://fixture.invalid");
  const json = (status, body, headers = {}) => response.writeHead(status, { "Content-Type": "application/json", ...headers })
    .end(JSON.stringify(body));
  if (url.pathname === "/cowork-registry") {
    if (request.method !== "GET" || request.headers.accept !== "application/json" ||
      url.searchParams.get("limit") !== "500" || url.searchParams.get("visibility") !== "commercial,gsuite,gsuite-google") {
      return json(400, { error: "unexpected registry request" });
    }
    const one = item("fixture/calendar", "https://calendar.example.invalid/mcp",
      { slug: "calendar", displayName: "Calendar", isAuthless: true, toolNames: ["list"], oneLiner: "Find events" });
    if (url.searchParams.has("cursor")) {
      if (url.searchParams.get("cursor") !== "page&2") return json(400, { error: "wrong cursor" });
      return json(200, { servers: [one, item("fixture/docs", "https://docs.example.invalid/sse", {}, "sse")] });
    }
    return json(200, { servers: [
      one,
      item("fixture/insecure", "http://private.example.invalid/"),
      item("fixture/mediated", "https://team.mcp.claude.com/example"),
      item("fixture/placeholder", "https://tenant.example.invalid/{account}"),
      item("fixture/tenant", "https://tenant.example.invalid/mcp", { requiredFields: ["tenant"] }),
    ], metadata: { nextCursor: "page&2" } });
  }
  if (request.method !== "POST" || request.headers.authorization || request.headers.cookie ||
    request.headers["mcp-protocol-version"] !== "2025-06-18") {
    return json(400, { error: "unexpected MCP headers" });
  }
  if (url.pathname.endsWith("/auth")) return json(401, {});
  if (url.pathname.endsWith("/init-fail")) return json(503, {});
  let raw = "";
  for await (const chunk of request) raw += chunk;
  const body = JSON.parse(raw);
  if (body.method === "initialize" && body.params?.clientInfo?.name === "9router" && body.id === 1) {
    return json(200, { jsonrpc: "2.0", id: 1, result: { protocolVersion: "2025-06-18" } },
      { "MCP-Session-Id": "connector-fixture-session" });
  }
  if (request.headers["mcp-session-id"] !== "connector-fixture-session") {
    return json(400, { error: "missing MCP session" });
  }
  if (body.method === "notifications/initialized") return response.writeHead(202).end();
  if (body.method !== "tools/list" || body.id !== 2) return json(400, { error: "unexpected MCP method" });
  const names = { jsonrpc: "2.0", id: 2, result: { tools: [
    { name: "search", description: "Find items", inputSchema: { type: "object" } }, { name: "help" },
  ] } };
  if (url.pathname.endsWith("/sse")) {
    response.writeHead(200, { "Content-Type": "text/event-stream" });
    response.write('data: {"jsonrpc":"2.0","id":1,"result":{}}\n\n');
    response.write('data: invalid\n\n');
    response.write("data: " + JSON.stringify(names).slice(0, 20));
    response.end(JSON.stringify(names).slice(20) + "\n\n");
    return;
  }
  json(200, names);
}
