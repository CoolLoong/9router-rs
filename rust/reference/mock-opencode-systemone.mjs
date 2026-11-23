const observedSessions = new Set();
const seatA = "fixture-live/fixture-combo-a";
const seatB = "fixture-live/fixture-combo-b";
const classifierPrompt = "Which of these configured models is the most appropriate for this request?";

function send(response, status, data) {
  return response.writeHead(status, { "content-type": "application/json" }).end(JSON.stringify(data));
}

export async function handleOpenCodeSystemOneMock(request, response) {
  const fail = (status, message) => send(response, status, { error: { message } });
  const session = request.headers["x-opencode-session"];
  if (request.headers["x-opencode-client"] !== "desktop" ||
    request.headers["user-agent"] !== "opencode/1.18.31" ||
    request.headers["content-type"] !== "application/json" ||
    !/^ses_[0-9a-f]{12}[0-9A-Za-z]{14}$/.test(session) || observedSessions.has(session)) {
    return fail(422, "unexpected OpenCode fixture headers");
  }
  const time = (~BigInt("0x" + session.slice(4, 16)) & 0xffffffffffffn) >> 12n;
  const current = BigInt(Date.now()) & 0xfffffffffn;
  if (time < current - 20000n || time > current + 20000n) {
    return fail(422, "OpenCode session timestamp outside fixture run");
  }
  observedSessions.add(session);

  let raw = "";
  for await (const chunk of request) raw += chunk;
  let body;
  try { body = JSON.parse(raw); } catch { return fail(422, "unexpected OpenCode fixture body"); }
  const anonymous = body.model === "jev-1.13-free";
  const key = anonymous ? "public" : "offline-systemone-opencode-zen";
  if ((!anonymous && body.model !== "jev-1.13") || request.headers.authorization !== "Bearer " + key) {
    return fail(401, "unexpected OpenCode fixture credentials or model");
  }

  if (body.questions?.route?.instructions === classifierPrompt) {
    if (!anonymous || JSON.stringify(body.questions.route.criteria) !== JSON.stringify({ [seatA]: null, [seatB]: null })) {
      return fail(422, "unexpected OpenCode Smart router criteria");
    }
    if (body.state === "OpenCode Smart fallback fixture") return fail(503, "fixture classifier unavailable");
    if (body.state !== "OpenCode Smart decision fixture") return fail(422, "unexpected OpenCode Smart prompt");
    return send(response, 200, { answers: { route: { choice: seatB, confidence: .98, probabilities: { [seatA]: .02, [seatB]: .98 } } },
      usage: { input_tokens: 4, output_tokens: 1 } });
  }
  if (body.state === "OpenCode direct quota fixture") return fail(429, "fixture public decision quota");
  if (body.state !== "OpenCode direct fixture" || body.questions?.route?.type !== "choice") return fail(422, "unexpected direct OpenCode request");
  return send(response, 200, { answers: { route: { choice: "b", confidence: .97 } },
    receipt: anonymous ? "anonymous" : "stored-key",
    ...(body.fixtureUsage === true ? { usage: { input_tokens: 8, output_tokens: 2 } } : {}) });
}
