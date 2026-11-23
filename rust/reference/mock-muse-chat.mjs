const origin = "https://api.meta.ai";
const routes = new Set(["/v1/chat/completions", "/v1/responses", "/v1/models"]);

export function museChatFixture(url) {
  return url?.origin === origin && routes.has(url.pathname)
    ? `http://127.0.0.1:52989/muse-chat${url.pathname}` : null;
}

export function isMuseChatMock(request) {
  return request.url.startsWith("/muse-chat/");
}

export async function handleMuseChatMock(request, response) {
  const match = request.url.slice("/muse-chat".length);
  const headers = request.headers;
  const oauth = headers.authorization === "Bearer LLM|fixture-muse-seat" && headers["x-api-version"] === "1.0.0";
  const apiKey = headers.authorization === "Bearer offline-muse-api" && !headers["x-api-version"];
  const reply = (status, value) => response.writeHead(status, {"content-type":"application/json"}).end(JSON.stringify(value));
  const fail = () => reply(422,{error:{message:"Unexpected Muse transport, key or request"}});
  if (!oauth && !apiKey) return fail();
  if (match === "/v1/models" && request.method === "GET") {
    return reply(200, {data:[{id:"muse-spark-1.3",object:"model"}]});
  }
  let raw = "";
  for await (const chunk of request) raw += chunk;
  const body = JSON.parse(raw);
  if (request.method !== "POST") return fail();
  if (match === "/v1/responses") {
    if (body.model !== "muse-spark-1.3" || body.stream !== false || body.reasoning?.effort !== "low" ||
        !body.include?.includes("reasoning.encrypted_content") ||
        body.input?.[0]?.encrypted_content !== "fixture-encrypted-response-input") return fail();
    return reply(200, {id:"resp-muse-shared",object:"response",created_at:1795046400,
      status:"completed",model:body.model,output:[{type:"reasoning",id:"rs-fixture-muse",summary:[],
        encrypted_content:"fixture-encrypted-response-output"}],usage:{input_tokens:7,output_tokens:3,total_tokens:10}});
  }
  if (match !== "/v1/chat/completions" || body.model !== "fixture-muse-chat" ||
      body.messages?.[0]?.content !== "Muse shared Chat") return fail();
  const base = {id:"chatcmpl-muse-shared",created:1795046400,model:body.model};
  const usage = {prompt_tokens:7,completion_tokens:3,total_tokens:10};
  const content = "Muse fixture response";
  if (!body.stream) {
    return reply(200, {...base,object:"chat.completion",choices:[
      {index:0,message:{role:"assistant",content},finish_reason:"stop"}], usage});
  }
  response.writeHead(200,{"content-type":"text/event-stream"});
  for (const [choices, extra] of [
    [[{index:0,delta:{role:"assistant",content},finish_reason:null}],{}],
    [[{index:0,delta:{},finish_reason:"stop"}],{usage}],
  ]) {
    response.write(`data: ${JSON.stringify({...base,object:"chat.completion.chunk",choices,...extra})}\n\n`);
  }
  response.end("data: [DONE]\n\n");
}
