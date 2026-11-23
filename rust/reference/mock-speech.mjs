const GEMINI_SPEECH = /^\/gemini\/v1beta\/models\/(gemini-3\.1-flash-tts-preview|gemini-2\.5-(?:flash|pro)-preview-tts):generateContent$/;

export function isSpeechMock(request) {
  if (request.method !== "POST") return false;
  const path = request.url.split("?")[0];
  return path === "/v1/audio/speech" || path === "/openai/v1/audio/speech" || GEMINI_SPEECH.test(path);
}

function sendJson(response, status, body) {
  response.writeHead(status, { "Content-Type": "application/json" });
  response.end(JSON.stringify(body));
}

function sendAudio(response, bytes, contentType, fixedLength) {
  const headers = { "Content-Type": contentType };
  if (fixedLength) headers["Content-Length"] = String(bytes.length);
  response.writeHead(200, headers);
  response.write(bytes.subarray(0, 5));
  setImmediate(() => response.end(bytes.subarray(5)));
}

export async function handleSpeechMock(request, response) {
  const url = new URL(request.url, "http://127.0.0.1");
  const geminiModel = url.pathname.match(GEMINI_SPEECH)?.[1];
  const selfhosted = url.pathname === "/v1/audio/speech";
  const expected = selfhosted ? "offline-selfhosted-speech" : "offline-openai-speech";
  if (geminiModel ? url.searchParams.get("key") !== "offline-provider-placeholder"
    : request.headers.authorization !== "Bearer " + expected) {
    return sendJson(response, 401, { error: { message: "unexpected speech fixture credential" } });
  }

  let raw = "";
  for await (const chunk of request) raw += chunk;
  let body;
  try { body = JSON.parse(raw); } catch {
    return sendJson(response, 400, { error: { message: "invalid speech fixture JSON" } });
  }
  const fail = (status, message) => sendJson(response, status, { error: { message } });
  if (body.input === "Speech combo rotation fixture" && !geminiModel) {
    return sendAudio(response, Buffer.from(selfhosted ? "kokoro rotation" : "openai rotation"), "audio/mpeg", true);
  }
  if (geminiModel) {
    const prompt = body.contents?.[0]?.parts?.[0]?.text;
    const audio = body.generationConfig;
    const voice = audio?.speechConfig?.voiceConfig?.prebuiltVoiceConfig?.voiceName;
    if (JSON.stringify(audio?.responseModalities) !== '["AUDIO"]' || Object.hasOwn(body, "model")) {
      return fail(422, "unexpected Gemini speech fixture shape");
    }
    const known = geminiModel === "gemini-2.5-flash-preview-tts" &&
      voice === "Puck" && prompt === "Say in zh-CN: Gemini voice fixture";
    const preset = geminiModel === "gemini-3.1-flash-tts-preview" &&
      voice === "Kore" && prompt === "Speak steadily: Gemini inline fixture";
    if (!known && !preset) return fail(422, "unexpected Gemini speech fixture prompt or voice");
    return sendJson(response, 200, {
      candidates: [{ content: { parts: [{ text: "fixture only" }, {
        inlineData: { mimeType: "audio/L16;rate=24000", data: Buffer.from([1, 0, 2, 0, 255, 127]).toString("base64") },
      }] } }],
    });
  }

  if (selfhosted) {
    if (body.model === "fixture-tts-outage") return fail(400, "fixture audio service rejection");
    if (body.model !== "kokoro" || Object.hasOwn(body, "speed") || Object.hasOwn(body, "style")) {
      return fail(422, "unexpected self-hosted speech fixture shape");
    }
    if (body.input === "Kokoro binary fixture" && body.voice === "region/voice" && body.response_format === "wav") {
      return sendAudio(response, Buffer.from("fixture kokoro WAV audio"), "audio/wav", true);
    }
    if (body.input === "Kokoro JSON fixture" && body.voice === "af_heart" && body.response_format === "json") {
      return sendAudio(response, Buffer.from("fixture kokoro JSON audio"), "audio/mpeg", false);
    }
    return fail(422, "unexpected Kokoro voice or format");
  }

  if (Object.hasOwn(body, "response_format") || Object.hasOwn(body, "speed") || Object.hasOwn(body, "style")) {
    return fail(422, "unexpected direct OpenAI speech extension");
  }
  if (body.model === "tts-1" && body.voice === "echo" && body.input === "OpenAI binary fixture") {
    return sendAudio(response, Buffer.from("fixture OpenAI MP3 audio"), "audio/mpeg", false);
  }
  if (body.model === "gpt-4o-mini-tts" && body.voice === "alloy" && body.input === "OpenAI JSON fixture") {
    return sendAudio(response, Buffer.from("fixture OpenAI JSON audio"), "audio/mpeg", true);
  }
  return fail(422, "unexpected direct OpenAI voice or model");
}
