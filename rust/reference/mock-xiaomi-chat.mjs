const regions = new Set(["sgp", "cn", "ams"]);
const routes = new Set(["/v1/chat/completions", "/anthropic/v1/messages"]);

export function xiaomiChatFixture(url) {
  if (url?.origin === "https://api.xiaomimimo.com" && routes.has(url.pathname)) {
    return `http://127.0.0.1:52989/xiaomi-chat/cloud${url.pathname}`;
  }
  const region = url?.hostname.match(/^token-plan-([a-z]+)\.xiaomimimo\.com$/)?.[1];
  return url?.protocol === "https:" && regions.has(region) && routes.has(url.pathname)
    ? `http://127.0.0.1:52989/xiaomi-chat/${region}${url.pathname}` : null;
}

export function isXiaomiChatMock(request) {
  return request.url.startsWith("/xiaomi-chat/");
}

export async function handleXiaomiChatMock(request, response) {
  const [, , region, path] = request.url.split("/");
  const messages = path === "anthropic";
  const wire = request.url.slice(`/xiaomi-chat/${region}`.length);
  const reply = (status, body) => response.writeHead(status,{"content-type":"application/json"}).end(JSON.stringify(body));
  const cloud = region === "cloud";
  const secret = cloud ? "sk-offline-mimo-cloud" : "tp-offline-tokenplan-fixture";
  const auth = messages
    ? request.headers["x-api-key"] === secret && !request.headers.authorization &&
      request.headers["anthropic-version"] === "2023-06-01"
    : request.headers.authorization === `Bearer ${secret}` && !request.headers["x-api-key"];
  let raw = "";
  for await (const bytes of request) raw += bytes;
  const body = JSON.parse(raw);
  const input = body.messages?.[0]?.content;
  const text = typeof input === "string" ? input : input?.[0]?.text;
  const model = cloud ? messages ? "mimo-v2.5-pro" : "xiaomi/mimo-v2.6-pro" : "mimo-v2.6-pro";
  const prompt = cloud ? "fixture MiMo cloud" : `fixture Token Plan ${region}`;
  if (request.method !== "POST" || (!cloud && !regions.has(region)) || !routes.has(wire) || !auth ||
      body.model !== model || text !== prompt) {
    return reply(422, {error:{message:"Unexpected Xiaomi region, request or auth"}});
  }
  const answer = `Xiaomi ${region} fixture`;
  if (messages) {
    return reply(200, {id:"msg-xiaomi-fixture",type:"message",role:"assistant",model:body.model,
      content:[{type:"text",text:answer}],stop_reason:"end_turn",stop_sequence:null,
      usage:{input_tokens:9,output_tokens:3}});
  }
  return reply(200, {id:"chatcmpl-xiaomi-fixture",object:"chat.completion",created:1795046400,model:body.model,
    choices:[{index:0,message:{role:"assistant",content:answer},finish_reason:"stop"}],
    usage:{prompt_tokens:9,completion_tokens:3,total_tokens:12}});
}
