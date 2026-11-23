const prefix = "/codex/backend-api/codex/responses";
let overloaded = false;

export function isCodexMock(request) {
  return request.method === "POST" && [prefix, prefix + "/compact"].includes(request.url);
}

export async function handleCodexMock(request, response) {
  const fail = reason => response.writeHead(422, { "Content-Type": "application/json" })
    .end(JSON.stringify({ error: { message: reason } }));
  const bearer = request.headers.authorization;
  const account = bearer?.replace("Bearer offline-codex-", "");
  if (!["primary","secondary","raw"].includes(account) ||
    request.headers["chatgpt-account-id"] !== `fixture-workspace-${account}` ||
    request.headers.originator !== "codex_cli_rs" || request.headers.session_id !== "fixture-codex-session" ||
    request.headers["user-agent"] !== `codex_cli_rs/${request.headers.version}` ||
    request.headers.accept !== "text/event-stream") {
    return fail("unexpected Codex headers");
  }
  let raw = "";
  for await (const bytes of request) raw += bytes;
  const body = JSON.parse(raw);
  if (!Array.isArray(body.input) || !body.input.length || body.stream !== true || body.store !== false ||
    body.prompt_cache_key !== "fixture-codex-session" || "max_output_tokens" in body || "previous_response_id" in body ||
    "unsupported_fixture" in body || !body.reasoning?.effort) return fail("unexpected Codex body");
  const label = body.input.find(item => item?.role === "user")?.content?.[0]?.text;
  if ((account === "raw") !== (label === "fixture raw Codex token")) {
    return fail("wrong Codex credential selection");
  }
  if (label === "fixture native Codex" &&
    (body.model !== "gpt-5.6-sol" || body.service_tier !== "priority" || body.reasoning.effort !== "medium" ||
      request.headers["x-openai-internal-codex-responses-lite"])) {
    return fail("unexpected regular Codex model or tier");
  }
  if (label === "fixture Chat Codex" &&
    (body.reasoning.effort !== "high" || body.tools?.[0]?.parameters?.properties?.token?.pattern !== undefined ||
      body.tools?.[0]?.parameters?.properties?.pattern?.pattern !== "^ok$")) {
    return fail("unexpected translated thinking or schema constraints");
  }
  if (label === "fixture Lite Codex" &&
    (request.headers["x-openai-internal-codex-responses-lite"] !== "true" || body.model !== "gpt-6.1-sol" ||
     body.instructions !== "" || body.tools !== null || body.tool_choice !== "auto" || body.parallel_tool_calls !== false ||
     body.reasoning.context !== "all_turns" || body.reasoning.effort !== "medium" ||
     body.input[0]?.type !== "additional_tools" || body.input[0]?.tools?.[0]?.name !== "fixture_find" ||
     !body.input.some(item => item.id === "msg_fixture_keep" && item.role === "developer") ||
     body.input.some(item => item.type === "item_reference"))) {
    return fail("unexpected Responses Lite representation");
  }
  if (label === "fixture Codex web search" &&
    (request.headers["x-openai-internal-codex-responses-lite"] || body.parallel_tool_calls !== undefined ||
      body.input.some(item => item.type === "additional_tools") ||
      !body.tools?.some(tool => tool.type === "web_search"))) {
    return fail("hosted web search must not use Responses Lite");
  }
  if (label === "fixture Codex account capacity" && account === "primary") {
    response.writeHead(200, { "Content-Type": "text/event-stream" })
      .end('event: error\ndata: {"type":"error","error":{"code":"model_at_capacity","message":"Selected model is at capacity. Please try a different model."}}\n\n');
    return;
  }
  if (label === "fixture Codex server overload" && !overloaded) {
    overloaded = true;
    response.writeHead(200, { "Content-Type": "text/event-stream" })
      .end('event: error\ndata: {"type":"error","error":{"code":"server_is_overloaded","message":"fixture temporary failure"}}\n\n');
    return;
  }

  const message = label === "fixture Codex compact transport check"
    ? (request.url.endsWith("/compact") ? "compact endpoint" : "regular endpoint")
    : `Codex ${account}`;
  const item = { id:"msg_codex_fixture",type:"message",role:"assistant",content:[{type:"output_text",text:message,annotations:[]}] };
  const result = { id:"resp_codex_fixture",object:"response",created_at:1795046400,model:body.model,status:"completed",
    output:[item], usage:{input_tokens:7,output_tokens:3,total_tokens:10,input_tokens_details:{cached_tokens:2}} };
  response.writeHead(200, { "Content-Type":"text/event-stream" });
  const send = (type,data) => response.write(`event: ${type}\ndata: ${JSON.stringify({type,...data})}\n\n`);
  send("response.created",{response:{...result,status:"in_progress",output:[],usage:null}});
  send("response.output_text.delta",{output_index:0,item_id:item.id,delta:message});
  send("response.output_item.done",{output_index:0,item});
  send("response.completed",{response:result});
  response.end("data: [DONE]\n\n");
}
