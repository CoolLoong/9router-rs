const fixture = "http://127.0.0.1:52989";
const prefix = "/static-chat/messages/";
const urls = new Map([
  ["https://api.deepseek.com/anthropic/v1/messages", "deepseek"],
  ["https://api.z.ai/api/anthropic/v1/messages?beta=true", "glm"],
  ["https://api.minimax.io/anthropic/v1/messages?beta=true", "minimax"],
  ["https://api.minimaxi.com/anthropic/v1/messages?beta=true", "minimax-cn"],
]);

export function staticMessagesFixture(url) {
  const provider = urls.get(url?.href);
  return provider ? fixture + prefix + provider + url.search : null;
}

export async function handleStaticMessages(request, response) {
  const target = new URL(request.url, fixture);
  const provider = target.pathname.slice(prefix.length);
  const fail = message => response.writeHead(422, {"content-type":"application/json"})
    .end(JSON.stringify({error: {message}}));
  let raw = "";
  for await (const bytes of request) raw += bytes;
  const body = JSON.parse(raw);
  const message = body.messages?.[0]?.content;
  const label = typeof message === "string" ? message : message?.[0]?.text;
  const key = provider === "glm" && label === "fixture glm authorized"
    ? "fixture-glm-id.fixture-glm-secret" : `offline-static-${provider}`;
  const upstreamBeta = "claude-code-20250219,interleaved-thinking-2025-05-14";
  if (request.headers["x-api-key"] !== key ||
      request.headers.authorization || request.headers["anthropic-version"] !== "2023-06-01" ||
      request.headers["anthropic-beta"] !== upstreamBeta ||
      target.search !== (provider === "deepseek" ? "" : "?beta=true")) {
    return fail("unexpected static Messages transport auth or URL suffix");
  }
  if ("context_management" in body || !label) return fail("unexpected Messages context or missing prompt");
  if (label === "fixture ds registry") {
    if (provider !== "deepseek" || body.model !== "deepseek-flash" ||
        body.thinking?.type !== "enabled" || body.reasoning_effort !== "max" ||
        body.tools?.map(tool => tool.type).join(",") !== "web_search_20250305,web_search_20260209" ||
        body.tools.at(-1)?.cache_control?.ttl !== "1h") {
      return fail("unexpected DeepSeek alias, reasoning, or built-in tool whitelist");
    }
  } else if (label === "fixture ds history") {
    const history = body.messages?.find(message => message.role === "assistant");
    const thought = history?.content?.[0];
    if (provider !== "deepseek" || body.model !== "deepseek-v4-pro" ||
        body.thinking?.type !== "enabled" || thought?.type !== "thinking" || thought.thinking !== "." ||
        "signature" in thought || history.reasoning_content !== " ") {
      return fail("expected unsigned DeepSeek tool-loop placeholder");
    }
  } else if (label === "fixture glm registry") {
    if (provider !== "glm" || body.model !== "glm-5.3" ||
        body.thinking?.type !== "enabled" || body.reasoning_effort !== "high" ||
        body.stream !== true || request.headers.accept !== "text/event-stream") {
      return fail("unexpected GLM thinking or streaming mode");
    }
  } else if (label === "fixture glm tool image") {
    const blocks = body.messages?.at(-1)?.content;
    if (provider !== "glm" || body.model !== "glm-5.3-flash" ||
        blocks?.[0]?.type !== "tool_result" || blocks[0].content?.[0]?.text !== "(image attached below)" ||
        blocks[1]?.text !== "[Image from tool result toolu_image]" || blocks[2]?.type !== "image" ||
        blocks[2]?.source?.data !== "ZmFrZQ==") {
      return fail("expected GLM inline image hoisted from tool result");
    }
  } else if (label === "fixture glm authorized") {
    if (provider !== "glm" || body.model !== "glm-5.3" || body.stream !== false) {
      return fail("unexpected GLM OAuth Messages request");
    }
  } else if (label === "fixture minimax cross") {
    if (provider !== "minimax-cn" || body.model !== "MiniMax-M3" || body.stream !== false ||
        body.system?.[0]?.type !== "text") {
      return fail("unexpected MiniMax Responses-to-Messages request");
    }
  } else if (label === "fixture minimax registry" || label === "fixture minimax cn registry") {
    const expected = label === "fixture minimax registry" ? "minimax" : "minimax-cn";
    if (provider !== expected || body.model !== "MiniMax-M3" || body.thinking?.type !== "adaptive" ||
        "output_config" in body || body.tools?.length !== 2 ||
        body.tools.some(tool => tool.type !== "custom") || body.tools.at(-1)?.cache_control?.ttl !== "1h") {
      return fail("unexpected MiniMax typed tools or thinking");
    }
  } else {
    return fail("unrecognized static Messages fixture prompt");
  }

  const text = "fixture " + provider + " Messages";
  const base = {id:"msg_static_messages_fixture", type:"message", role:"assistant", model:body.model, stop_sequence:null};
  if (body.stream === true) {
    response.writeHead(200, {"content-type":"text/event-stream", "cache-control":"no-cache"});
    const emit = (type, value) => response.write("event: " + type + "\ndata: " + JSON.stringify({type,...value}) + "\n\n");
    emit("message_start", {message:{...base, content:[], stop_reason:null, usage:{input_tokens:6}}});
    emit("content_block_start", {index:0, content_block:{type:"text",text:""}});
    emit("content_block_delta", {index:0, delta:{type:"text_delta",text}});
    emit("content_block_stop", {index:0});
    emit("message_delta", {delta:{stop_reason:"end_turn",stop_sequence:null}, usage:{output_tokens:2}});
    emit("message_stop", {});
    return response.end();
  }
  return response.writeHead(200, {"content-type":"application/json"}).end(JSON.stringify({
    ...base, content:[{type:"text",text}], stop_reason:"end_turn",
    usage:{input_tokens:6,output_tokens:2},
  }));
}
