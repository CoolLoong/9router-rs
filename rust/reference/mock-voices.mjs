const endpoints = new Map([
  ["https://speech.platform.bing.com/consumer/speech/synthesize/readaloud/voices/list", "/edge/consumer/voices/list?fixture=voices"],
  ["https://api.elevenlabs.io/v1/voices", "/elevenlabs/v1/voices"],
  ["https://api.deepgram.com/v1/models", "/deepgram/v1/models"],
  ["https://api.inworld.ai/tts/v1/voices", "/inworld/tts/v1/voices"],
  ["https://api.minimax.io/v1/get_voice", "/minimax/v1/get_voice"],
  ["https://api.minimaxi.com/v1/get_voice", "/minimax-cn/v1/get_voice"],
]);
const paths = new Set([...endpoints.values()].map(value => value.split("?")[0]));
const fetchedElevenLabsKeys = new Set();

export function voiceFixture(url) {
  const path = url && endpoints.get(url.origin + url.pathname);
  return path ? "http://127.0.0.1:52989" + path : null;
}

export function isVoiceMock(request) {
  return paths.has(new URL(request.url, "http://127.0.0.1").pathname);
}

function send(response, status, data) {
  return response.writeHead(status, { "content-type": "application/json" }).end(JSON.stringify(data));
}

export async function handleVoiceMock(request, response) {
  const provider = new URL(request.url, "http://127.0.0.1").pathname.split("/")[1];
  const fail = (status, message) => send(response, status, { error: message });
  if (request.method !== (provider.startsWith("minimax") ? "POST" : "GET")) return fail(400, "unexpected voice fixture method");
  if (provider === "edge") {
    if (!request.headers["user-agent"]?.includes("Chrome/146.0.0.0") || request.headers.authorization) return fail(401, "unexpected Edge voice headers");
    return send(response, 200, [
      { Locale: "en-US", FriendlyName: "Microsoft Alice Online (Natural) - English)", ShortName: "en-US-AliceNeural", Gender: "Female" },
      { Locale: "pt-BR", ShortName: "pt-BR-BrunoNeural", Gender: "Male" },
      { Locale: "en-GB", FriendlyName: "Microsoft Quinn", ShortName: "en-GB-QuinnNeural", Gender: "Male" },
    ]);
  }
  if (provider === "elevenlabs") {
    const key = request.headers["xi-api-key"];
    if (!["offline-voices-elevenlabs", "offline-voices-explicit"].includes(key) ||
      request.headers["content-type"] !== "application/json" || request.headers.authorization) return fail(401, "unexpected ElevenLabs fixture credential");
    if (fetchedElevenLabsKeys.has(key)) return fail(503, "fixture expected a cached voice list");
    fetchedElevenLabsKeys.add(key);
    const prefix = key === "offline-voices-explicit" ? "explicit" : "saved";
    return send(response, 200, { voices: [
      { voice_id: prefix + "-el-primary", name: "Priya", labels: { language: "en-US", gender: "female" }, category: "premade",
        verified_languages: [{ language: "fr" }, { language: "en-US" }, { language: "fr" }] },
      { voice_id: prefix + "-el-alt", name: "Another", is_owner: true },
      { voice_id: prefix + "-el-custom", name: "Forge", labels: { language: "es", gender: "male" }, category: "cloned" },
    ] });
  }
  if (provider === "deepgram") {
    if (request.headers.authorization === "Token offline-voices-deepgram-error") return fail(503, "synthetic quota unavailable");
    if (request.headers.authorization === "Token offline-voices-deepgram-redaction") return fail(503, "synthetic credential offline-voices-deepgram-redaction");
    if (request.headers.authorization !== "Token offline-voices-deepgram") return fail(401, "unexpected Deepgram fixture credential");
    return send(response, 200, { stt: [{ name: "ignored voice" }], tts: [
      { canonical_name: "aura-fixture-en", name: "Sample", languages: ["en", "fr", "en"], metadata: { tags: ["featured", "feminine"] } },
      { canonical_name: "aura-regional-es", metadata: { tags: ["masculine"] } },
    ] });
  }
  if (provider === "inworld") {
    if (request.headers.authorization !== "Basic offline-voices-inworld") return fail(401, "unexpected Inworld fixture credential");
    return send(response, 200, { voices: [
      { voiceId: "fixture-oak", displayName: "Twin", gender: "MALE", languages: ["fr", "en", "fr"] },
      { voiceId: "fixture-fern", languages: [] },
    ] });
  }
  if (request.headers.authorization !== "Bearer offline-voices-" + provider) return fail(401, "unexpected MiniMax fixture credential");
  if (request.headers["content-type"] !== "application/json") return fail(422, "unexpected MiniMax fixture content type");
  let raw = "";
  for await (const chunk of request) raw += chunk;
  let body;
  try { body = JSON.parse(raw); } catch { return fail(422, "unexpected MiniMax fixture body"); }
  if (body.voice_type === "fixture-error") {
    return send(response, 200, { baseResp: { statusCode: "1001", statusMsg: "synthetic voice error" } });
  }
  if (body.voice_type !== "all" && body.voice_type !== "system") return fail(422, "unexpected fixture voice type");
  if (provider === "minimax-cn") {
    return send(response, 200, { system_voice: [{ voiceId: "Chinese_Ada", voiceName: "Ada" }] });
  }
  return send(response, 200, {
    base_resp: { status_code: 0, status_msg: "" },
    system_voice: [
      { voice_id: "English_Zelda", voice_name: "Zelda" }, { voiceId: "English_Ada", voiceName: "Ada" },
      { voice_id: "Mandarin_Kai", voice_name: "Kai" }, { voiceId: "English_Ada", voiceName: "duplicate" },
    ],
    voice_cloning: [{ voice_id: "fixture-local-cloned", voice_name: "Studio" }],
    voice_generation: [{ voice_id: "generated", voice_name: "Beta" }],
    music_generation: [{ voiceId: "music", voiceName: "Augmented" }],
  });
}
