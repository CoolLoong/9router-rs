const ROOT = "http://127.0.0.1:52989";
const routes = [
  ["https://api.elevenlabs.io", /^\/v1\/text-to-speech\/[\w-]+$/, "/elevenlabs"],
  ["https://api.inworld.ai", /^\/tts\/v1\/voice$/, "/inworld"],
  ["https://api.minimax.io", /^\/v1\/t2a_v2$/, "/minimax"],
  ["https://api.minimaxi.com", /^\/v1\/t2a_v2$/, "/minimax-cn"],
];

export function speechProviderFixture(url) {
  const route = routes.find(([origin, path]) => url.origin === origin && path.test(url.pathname));
  return route ? ROOT + route[2] + url.pathname + url.search : null;
}

export function isSpeechProviderMock(request) {
  const path = request.url.split("?")[0];
  return request.method === "POST" && routes.some(([, matcher, prefix]) =>
    path.startsWith(prefix + "/") && matcher.test(path.slice(prefix.length)));
}

export async function handleSpeechProviderMock(request, response) {
  const path = request.url.split("?")[0];
  const [, provider] = path.split("/");
  const fail = message => response.writeHead(422, { "Content-Type": "application/json" })
    .end(JSON.stringify({ error: { message } }));
  const auth = provider === "elevenlabs" ? request.headers["xi-api-key"] : request.headers.authorization;
  const scheme = provider === "elevenlabs" ? "" : provider === "inworld" ? "Basic " : "Bearer ";
  if (auth !== scheme + `offline-${provider}-speech` ||
      request.headers["content-type"] !== "application/json") {
    return fail("Unexpected fixture speech authorization");
  }
  let raw = "";
  for await (const chunk of request) raw += chunk;
  let body;
  try { body = JSON.parse(raw); } catch { return fail("Invalid speech fixture JSON"); }
  const output = (status, value) => response.writeHead(status, { "Content-Type": "application/json" })
    .end(JSON.stringify(value));
  if (provider === "elevenlabs") {
    if (!path.endsWith("/FixtureVoice") || body.text !== "ElevenLabs fixture" ||
      body.model_id !== "eleven_turbo_v2_5" || body.voice_settings?.stability !== 0.5 ||
      body.voice_settings?.similarity_boost !== 0.75 || Object.keys(body).length !== 3) {
      return fail("Unexpected ElevenLabs request");
    }
    return response.writeHead(200, { "Content-Type": "audio/mpeg" })
      .end(Buffer.from("ElevenLabs fixture bytes ".repeat(50)));
  }
  if (provider === "inworld") {
    if (body.text !== "Inworld fixture" || body.modelId !== "inworld-tts-1.5-max" ||
      body.voiceId !== "Alex" || body.audioConfig?.audioEncoding !== "MP3" ||
      Object.keys(body).length !== 4) {
      return fail("Unexpected Inworld request");
    }
    return output(200, { audioContent: " \n-_8" });
  }
  const global = provider === "minimax";
  if (body.model !== (global ? "speech-2.8-hd" : "speech-2.8-turbo")
      || body.text !== (global ? "MiniMax global fixture" : "MiniMax CN fixture")
      || body.voice_setting?.voice_id !== (global ? "fixture-narrator" : "English_expressive_narrator")
      || body.voice_setting?.speed !== 1 || body.voice_setting?.vol !== 1 || body.voice_setting?.pitch !== 0
      || body.output_format !== "hex" || body.stream !== false || body.language_boost !== "auto"
      || body.audio_setting?.format !== "mp3" || body.audio_setting?.sample_rate !== 32000
      || body.audio_setting?.bitrate !== 128000 || body.audio_setting?.channel !== 1) {
    return fail("Unexpected MiniMax request");
  }
  const audio = Buffer.from(global ? "Fixture MiniMax wav bytes" : "Fixture MiniMax CN bytes").toString("hex");
  return output(200, global
    ? { data: { audio }, base_resp: { status_code: 0 }, extra_info: { audio_format: "wav" } }
    : { data: { audio }, baseResp: { statusCode: "0" }, extraInfo: { audioFormat: "mp3" } });
}
