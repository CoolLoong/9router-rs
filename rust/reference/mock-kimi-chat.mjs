const endpoint = "https://api.kimi.com";
const paths = new Set(["/coding/v1/chat/completions", "/coding/v1/messages", "/coding/v1/responses"]);
const device = "0139b5c9-506c-48fb-bef1-956bfdc9880b";

export function kimiChatFixture(url) {
  return url?.origin === endpoint && paths.has(url.pathname)
    ? `http://127.0.0.1:52989/kimi-chat${url.pathname.slice("/coding/v1".length)}${url.search}` : null;
}

export function isKimiChatMock(request) {
  return request.url.startsWith("/kimi-chat/") || request.url === "/kimi-device/token";
}

export async function handleKimiChatMock(request, response) {
  let raw = "";
  for await (const chunk of request) raw += chunk;
  const reply = (status, value) => response.writeHead(status, {"content-type":"application/json"}).end(JSON.stringify(value));
  if (request.url === "/kimi-device/token") {
    const args = new URLSearchParams(raw);
    if (request.method !== "POST" || request.headers.authorization || request.headers["x-msh-device-id"] !== device ||
        args.get("grant_type") !== "refresh_token" || args.get("refresh_token") !== "offline-kimi-translator-refresh") {
      return reply(422,{error:"unexpected Kimi Translator refresh profile"});
    }
    return reply(200,{access_token:"offline-kimi-translator-new",refresh_token:"offline-kimi-translator-rotated",expires_in:3600});
  }
  const body = JSON.parse(raw);
  const label = body.messages?.[0]?.content ?? body.input;
  const prompt = typeof label === "string" ? label
    : label?.[0]?.text ?? label?.[0]?.content?.[0]?.text;
  const apiKey = prompt === "fixture Kimi API key";
  const key = apiKey ? "offline-kimi-api" : "offline-kimi-code-token";
  const messages = request.url === "/kimi-chat/messages?beta=true";
  const responses = request.url === "/kimi-chat/responses";
  const chat = request.url === "/kimi-chat/chat/completions";
  if (messages && prompt === "fixture Kimi Translator refresh") {
    if (request.headers["x-msh-device-id"] !== device || request.headers["x-msh-platform"] !== "9router") {
      return reply(422,{error:{message:"unexpected Kimi Translator device binding"}});
    }
    if (request.headers.authorization || request.headers["anthropic-version"] !== "2023-06-01") {
      return reply(422,{error:{message:"unexpected Kimi Translator authorization scheme"}});
    }
    if (request.headers["x-api-key"] === "offline-kimi-translator-old") {
      return reply(401,{error:{message:"synthetic rejected token offline-kimi-translator-old"}});
    }
    if (request.headers["x-api-key"] !== "offline-kimi-translator-new") {
      return reply(422,{error:{message:"unexpected rotated Kimi Translator token"}});
    }
    return reply(200,{content:[{type:"text",text:"Kimi refresh echoed offline-kimi-translator-old and offline-kimi-translator-new"}]});
  }
  const auth = messages
    ? request.headers["x-api-key"] === key && !request.headers.authorization &&
      request.headers["anthropic-version"] === "2023-06-01"
    : request.headers.authorization === `Bearer ${key}` && !request.headers["x-api-key"];
  const valid = request.method === "POST" && auth && body.model === (apiKey ? "kimi-k3" : "k3") &&
    (apiKey || (request.headers["x-msh-platform"] === "9router" && request.headers["x-msh-device-id"] === device)) &&
    ((chat && ["fixture Kimi chat", "fixture Kimi SSE", "fixture Kimi API key"].includes(prompt)) ||
      (messages && ["fixture Kimi Messages", "fixture Kimi Messages stream"].includes(prompt)) ||
      (responses && prompt === "fixture Kimi Responses"));
  if (!valid) return reply(422, {error:{message:"Unexpected Kimi transport, input, or device binding"}});
  const text = "Kimi fixture answer";
  if (messages) {
    if (body.stream) {
      response.writeHead(200,{"content-type":"text/event-stream"});
      response.write(`event: content_block_delta\ndata: ${JSON.stringify({type:"content_block_delta",index:0,delta:{type:"text_delta",text}})}\n\n`);
      return response.end('event: message_stop\ndata: {"type":"message_stop"}\n\n');
    }
    return reply(200,{id:"msg_kimi_fixture", type:"message", role:"assistant", model:body.model,
      content:[{type:"text",text}], stop_reason:"end_turn", stop_sequence:null,
      usage:{input_tokens:6,output_tokens:2}});
  }
  if (responses) {
    return reply(200,{id:"resp_kimi_fixture", object:"response", model:body.model,
      created_at:1795046400, status:"completed", output:[{id:"msg_kimi_fixture",type:"message",
        status:"completed",role:"assistant",content:[{type:"output_text",text,annotations:[]}]}],
      usage:{input_tokens:6,output_tokens:2,total_tokens:8}});
  }
  const base = {id:"chatcmpl-kimi-fixture",created:1795046400,model:body.model};
  const usage = {prompt_tokens:6,completion_tokens:2,total_tokens:8};
  if (body.stream) {
    response.writeHead(200, {"content-type":"text/event-stream"});
    for (const [choices, extra] of [
      [[{index:0,delta:{role:"assistant",content:text},finish_reason:null}], {}],
      [[{index:0,delta:{},finish_reason:"stop"}], {usage}],
    ]) {
      response.write(`data: ${JSON.stringify({...base,object:"chat.completion.chunk",choices,...extra})}\n\n`);
    }
    return response.end("data: [DONE]\n\n");
  }
  return reply(200,{...base,object:"chat.completion",choices:[{index:0,
    message:{role:"assistant",content:text},finish_reason:"stop"}],usage});
}
