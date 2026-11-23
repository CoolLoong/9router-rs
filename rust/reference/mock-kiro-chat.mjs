import pkg from "../../package.json" with { type: "json" };
import { encodeEventFrame } from "../../open-sse/utils/awsEventStream.js";

const loopback = "http://127.0.0.1:52989";
const secret = "ksk_fixture_kiro_chat_only";
const oauth = new Map([
  ["offline-fixture-kiro-idc", { region: "eu-central-1" }],
  ["offline-fixture-kiro-social", { region: "us-east-1" }],
  ["offline-fixture-kiro-external", { region: "us-east-1", tokenType: "EXTERNAL_IDP" }],
]);
const sessionStarts = new Map();

export function isKiroChatMock(request) {
  const path = new URL(request.url, loopback).pathname;
  return request.method === "POST" &&
    /^\/kiro\/(q|codewhisperer)\/(us-east-1|eu-central-1)\/generateAssistantResponse$/.test(path);
}

function json(response, status, value) {
  return response.writeHead(status, { "content-type": "application/json" }).end(JSON.stringify(value));
}

function event(kind, data) {
  return Buffer.from(encodeEventFrame({
    ":message-type": "event", ":event-type": kind, ":content-type": "application/json",
  }, data));
}

export async function handleKiroChatMock(request, response) {
  const headers = request.headers;
  const token = headers.authorization?.replace(/^Bearer /, "");
  const account = oauth.get(token);
  const region = account?.region || "us-east-1";
  const profile = account ? `arn:aws:codewhisperer:${region}:123456789012:profile/chat-fixture` : undefined;
  const tokenType = account ? account.tokenType : "API_KEY";
  const path = new URL(request.url, loopback).pathname;
  const alternate = path.startsWith("/kiro/codewhisperer/");
  if ((token !== secret && !account) || headers.authorization !== "Bearer " + token ||
      path !== `/kiro/${alternate ? "codewhisperer" : "q"}/${region}/generateAssistantResponse` ||
      alternate && (!account || region !== "us-east-1") ||
      headers.tokentype !== tokenType ||
      headers.accept !== "application/vnd.amazon.eventstream" || headers["content-type"] !== "application/json" ||
      headers["user-agent"] !== "9Router/" + pkg.version ||
      headers["x-amz-sso-bearer"] !== (account ? token : undefined) ||
      headers["x-amzn-codewhisperer-profile-arn"] !== profile ||
      headers["x-amz-target"] !== (alternate ? "AmazonCodeWhispererStreamingService.GenerateAssistantResponse" : undefined) ||
      ["x-amz-user-agent", "x-amzn-kiro-agent-mode", "x-amzn-codewhisperer-machine-id"].some(name => headers[name])) {
    return json(response, 422, { error: { message: "Unexpected Kiro request identity" } });
  }
  const bytes = [];
  for await (const chunk of request) bytes.push(chunk);
  const input = JSON.parse(Buffer.concat(bytes));
  const conversation = input.conversationState;
  const message = conversation?.currentMessage?.userInputMessage;
  const content = message?.content.match(/^\[Context: Current time is [^\]]+\]\n\n(.+)$/s)?.[1];
  const tools = message?.userInputMessageContext?.tools;
  const toolResult = content === "Tool results provided.";
  const expectedModel = content === "fixture GPT reasoning" ? "gpt-5.6-luna" : "auto";
  const scoped = content?.startsWith("fixture session ");
  const id = conversation?.conversationId;
  if (input.profileArn !== profile || input.tools || message?.modelId !== expectedModel || message.origin !== "AI_EDITOR" ||
      !(scoped ? /^[a-f0-9-]{36}$/.test(id) : /^[a-f0-9-]{36}\d+$/.test(id)) ||
      conversation.chatTriggerType !== "MANUAL" || input._nativeKiroSession !== undefined ||
      input.inferenceConfig?.maxTokens !== 32000 || !conversation.history ||
      (!content?.startsWith("fixture ") && !toolResult)) {
    return json(response, 422, { error: { message: "Unexpected synthetic Kiro chat envelope" } });
  }
  if (content === "fixture tool calls" || toolResult) {
    const definition = tools?.[0]?.toolSpecification;
    const call = conversation.history.at(-1)?.assistantResponseMessage?.toolUses?.[0];
    const result = message.userInputMessageContext?.toolResults?.[0];
    if (tools?.length !== 1 || definition?.name !== "lookup_ticket" ||
        definition.inputSchema?.json?.properties?.ticket?.type !== "string" ||
        (toolResult && (call?.toolUseId !== "fixture_ticket_call" || call?.input?.ticket !== "T3" ||
          result?.toolUseId !== "fixture_ticket_call" || result?.content?.[0]?.text !== "Ticket T3 is open"))) {
      return json(response, 422, {error:{message:"Unexpected synthetic Kiro tool request"}});
    }
    if (!toolResult) {
      response.writeHead(200, {"content-type":"application/vnd.amazon.eventstream"});
      response.write(event("toolUseEvent", {name:"lookup_ticket",toolUseId:"fixture_ticket_call",input:'{"ticket":'}));
      response.write(event("toolUseEvent", {name:"lookup_ticket",toolUseId:"fixture_ticket_call",input:'"T3"}'}));
      return response.end(event("messageStopEvent", {stopReason:"tool_use"}));
    }
  }
  if (content === "fixture image" && (message.images?.length !== 1 || message.images[0]?.format !== "png" ||
      message.images[0].source?.bytes !== "ZmFrZS1pbWFnZQ==")) {
    return json(response, 422, {error:{message:"Unexpected Kiro image content"}});
  }
  if (content === "fixture GPT reasoning" && input.additionalModelRequestFields?.reasoning?.effort !== "xhigh") {
    return json(response, 422, {error:{message:"Unexpected Kiro GPT effort"}});
  }
  if (scoped) {
    const prior = sessionStarts.get(id);
    if (content === "fixture session first" || content === "fixture session another owner" || content === "fixture session alternate") {
      if (conversation.history.length !== 0 || (prior && !(content === "fixture session alternate" && alternate))) {
        return json(response, 422, {error:{message:"Shared or duplicated Kiro session"}});
      }
      sessionStarts.set(id, {...message});
      if (content === "fixture session alternate" && !alternate) {
        return json(response, 403, {error:{message:"Fixture primary authentication surface rejected"}});
      }
    } else {
      const first = conversation.history[0]?.userInputMessage;
      const assistant = conversation.history[1]?.assistantResponseMessage;
      if (!prior || first?.content !== prior.content || first?.origin !== undefined || !assistant?.content) {
        return json(response, 422, {error:{message:"Unexpected replayed Kiro session start"}});
      }
    }
  }
  if (content === "fixture unauthorized") {
    return json(response, 401, { error: { message: ["Fixture provider rejected", token, profile].filter(Boolean).join(" ") } });
  }
  if (content === "fixture external alternate" && !alternate) {
    return json(response, 403, { error: { message: "Fixture primary authentication surface rejected" } });
  }
  if (content === "fixture bad payload" && !alternate) {
    return json(response, 400, { error: { message: "Fixture payload rejected" } });
  }
  const refusal = content === "fixture refusal";
  const frame = Buffer.concat([
    event("assistantResponseEvent", { content: refusal ? "Buffered private response" : alternate ? "Kiro alternate " : "Kiro fixture " }),
    event("assistantResponseEvent", { content: "世界" }),
    event("metricsEvent", { inputTokens: 12, outputTokens: 2 }),
    event("messageStopEvent", { stopReason: refusal ? "guardrail_blocked" : "endTurn" }),
  ]);
  response.writeHead(200, { "content-type": "application/vnd.amazon.eventstream" });
  response.write(frame.subarray(0, 27));
  return response.end(frame.subarray(27));
}
