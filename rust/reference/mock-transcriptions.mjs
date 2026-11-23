const GEMINI_STT = "/gemini/v1beta/models/gemini-2.5-flash-lite:generateContent";
const SELF = "/v1/audio/transcriptions";
const GROQ = "/groq/openai/v1/audio/transcriptions";
const DEEPGRAM = "/deepgram/v1/listen";
const HUGGING = "/hf/models/openai/whisper-large-v3";
const ELEVEN = "/elevenlabs/v1/speech-to-text";
const ASSEMBLY = "/assemblyai/v2/";
const AUDIO = "RIFF migration fake audio bytes";

export function isTranscriptionMock(request) {
  if (request.method === "GET" && request.url === ASSEMBLY + "transcript/fixture-assembly-job") return true;
  if (request.method !== "POST") return false;
  const url = new URL(request.url, "http://127.0.0.1");
  if (url.pathname === GEMINI_STT) return url.searchParams.has("key");
  return [SELF, GROQ, DEEPGRAM, HUGGING, ELEVEN, ASSEMBLY + "upload", ASSEMBLY + "transcript"].includes(url.pathname);
}

function send(response, status, body, type = "application/json") {
  const data = typeof body === "string" ? body : JSON.stringify(body);
  return response.writeHead(status, { "Content-Type": type }).end(data);
}

export async function handleTranscriptionMock(request, response) {
  const url = new URL(request.url, "http://127.0.0.1");
  const fail = (status, message) => send(response, status, { error: { message } });
  const chunks = [];
  for await (const chunk of request) chunks.push(chunk);
  const bytes = Buffer.concat(chunks);
  const contentType = request.headers["content-type"] || "";
  if (url.pathname.startsWith(ASSEMBLY)) {
    if (request.headers.authorization !== "Bearer offline-assemblyai-stt") return fail(401, "unexpected AssemblyAI credential");
    const stage = url.pathname.slice(ASSEMBLY.length);
    if (stage === "upload") {
      if (contentType !== "application/octet-stream" || bytes.toString() !== AUDIO) return fail(422, "unexpected AssemblyAI upload");
      return send(response, 200, { upload_url: "https://example.invalid/fixture-upload" });
    }
    if (stage === "transcript") {
      const body = JSON.parse(bytes.toString());
      if (body.audio_url !== "https://example.invalid/fixture-upload" || body.speech_models?.[0] !== "universal-3-pro" ||
          body.language_detection !== true) return fail(422, "unexpected AssemblyAI transcript request");
      return send(response, 200, { id: "fixture-assembly-job" });
    }
    return send(response, 200, { status: "completed", text: "AssemblyAI fixture transcript" });
  }
  if (url.pathname === ELEVEN) {
    if (request.headers["xi-api-key"] !== "offline-elevenlabs-stt" || request.headers.authorization) {
      return fail(401, "unexpected ElevenLabs STT credential");
    }
    const input = await new Response(bytes, {headers: {"content-type":contentType}}).formData();
    const file = input.get("file");
    if (input.get("model_id") !== "scribe_v2" || file.name !== "migration.wav" || file.type !== "audio/wav" ||
        await file.text() !== AUDIO) return fail(422, "unexpected ElevenLabs audio or model");
    const format = JSON.parse(input.get("additional_formats"))[0]?.format;
    if (format === "seg_json") {
      if (input.get("language_code") !== "fr" || input.get("timestamps_granularity") !== "word" ||
          input.get("tag_audio_events") !== "true" || input.get("diarize") !== "true" || input.has("num_speakers")) {
        return fail(422, "unexpected ElevenLabs transcription options");
      }
      return send(response, 200, { text:"Fixture transcription", language_code:"fra", language_probability:0.95,
        words:[{text:"Fixture",start:0,end:.5}], num_speakers:2,
        additional_formats:[{format,content:'[{"text":"Fixture transcription","start":0,"end":1}]'}] });
    }
    if (format === "srt" && !input.has("language_code") && input.get("num_speakers") === "4") {
      return send(response, 200, {text:"fallback", additional_formats:[
        {format,content:"1\n00:00:00,000 --> 00:00:01,000\nFixture subtitle\n"},
      ]});
    }
    return fail(422, "unexpected ElevenLabs STT output format");
  }

  if (url.pathname === GEMINI_STT) {
    if (url.searchParams.get("key") !== "offline-provider-placeholder" || request.headers.authorization) {
      return fail(401, "unexpected Gemini STT credential");
    }
    let parsed;
    try { parsed = JSON.parse(bytes.toString()); } catch {
      return fail(400, "invalid Gemini STT fixture JSON");
    }
    const parts = parsed.contents?.[0]?.parts;
    const mime = parts?.[1]?.inline_data?.mime_type;
    if (parts?.[1]?.inline_data?.data !== Buffer.from(AUDIO).toString("base64")
        || Object.hasOwn(parsed, "generationConfig") || Object.hasOwn(parsed, "response_format")) {
      return fail(422, "unexpected Gemini STT fixture payload");
    }
    const explicit = parts?.[0]?.text === "Gemini phrase fixture Language: zh-CN." && mime === "audio/mp4";
    const defaults = parts?.[0]?.text === "Generate a transcript of the speech. Return only the transcribed text, no commentary."
      && mime === "audio/wav";
    if (!explicit && !defaults) return fail(422, "unexpected Gemini STT prompt or MIME");
    return send(response, 200, {
      candidates: [{ content: { parts: [{ text: "Gemini " }, { text: "" }, { text: "fixture transcript" }] } }],
    });
  }

  if (url.pathname === DEEPGRAM) {
    if (request.headers.authorization !== "Token offline-deepgram-stt") return fail(401, "unexpected Deepgram STT credential");
    if (url.searchParams.get("model") !== "nova-3" || url.searchParams.get("punctuate") !== "true" ||
        url.searchParams.get("custom") !== "1" || url.searchParams.getAll("smart_format").join() !== "true" ||
        bytes.toString() !== AUDIO) {
      return fail(422, "unexpected Deepgram STT fixture payload");
    }
    const detect = contentType === "audio/opus" && url.searchParams.get("detect_language") === "true"
      && !url.searchParams.has("language");
    const explicit = contentType === "audio/flac" && url.searchParams.get("language") === "de-CH"
      && !url.searchParams.has("detect_language");
    if (!detect && !explicit) return fail(422, "unexpected Deepgram language or MIME");
    return send(response, 200, { results: { channels: [{ alternatives: [{ transcript: detect ? "Deepgram auto" : "Deepgram language" }] }] } });
  }

  if (url.pathname === HUGGING) {
    if (request.headers.authorization !== "Bearer offline-hf-stt") return fail(401, "unexpected HuggingFace STT credential");
    if (bytes.toString() !== AUDIO || contentType !== "audio/wav") return fail(422, "unexpected HuggingFace STT media");
    return send(response, 200, { text: "HuggingFace fixture transcript", chunks: [] });
  }

  const groq = url.pathname === GROQ;
  const token = groq ? "offline-groq-stt" : "offline-selfhosted-stt";
  if (request.headers.authorization !== "Bearer " + token) return fail(401, "unexpected compatible STT credential");
  let form;
  try { form = await new Response(bytes, { headers: { "content-type": contentType } }).formData(); } catch {
    return fail(400, "invalid upstream STT multipart shape");
  }
  const file = form.get("file");
  if (!file?.text || await file.text() !== AUDIO || file.name !== "migration.wav" || file.type !== "audio/wav") {
    return fail(422, "unexpected upstream STT file");
  }
  const model = form.get("model");
  if (model === "fixture-invalid-audio") return fail(422, "fixture upstream STT rejection");
  if (form.has("phrase") || form.has("timestamp_granularities[]") || form.getAll("model").length !== 1) {
    return fail(422, "unexpected unsupported STT field");
  }
  const format = form.get("response_format");
  if (groq && model === "whisper-large-v3-turbo" && format === "verbose_json" &&
      form.get("prompt") === "  Groq phrase  " && !form.has("temperature") && !form.has("language")) {
    return send(response, 200, { text: "Groq fixture transcript", duration: 0.625, task: "transcribe" });
  }
  if (groq || model !== "whisper-local") return fail(422, "unexpected compatible STT model");
  if (format === "text") return send(response, 200, "fixture self-hosted text\n", "text/plain; charset=utf-8");
  if (format === "srt") return send(response, 200, "1\n00:00:00,000 --> 00:00:01,000\nFixture caption\n", "application/x-subrip");
  if (format === "json" && form.get("language") === " en-US " && form.get("temperature") === "0.25"
      && form.get("prompt") === "Original phrase" && form.getAll("prompt").length === 1) {
    return send(response, 200, '{"text":"Self-hosted fixture transcript","extra":{"raw": true}}');
  }
  return fail(422, "unexpected compatible STT fields");
}
