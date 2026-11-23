import { createServer } from "node:http";
import WebSocket, { WebSocketServer } from "ws";

const local = "ws://127.0.0.1:52988";
const modelId = "gemini-2.5-flash-native-audio-preview-09-17";
const path = /^\/ws\/api\/v1beta\/models\/([^/]+):bidiGenerateContent$/;
const Original = globalThis.WebSocket;
if (Original) {
  globalThis.WebSocket = class FixtureGemini extends Original {
    constructor(input, protocols) {
      let url = new URL(String(input));
      if (url.protocol === "wss:" && url.hostname === "generativelanguage.googleapis.com" && path.test(url.pathname)) {
        url = new URL(url.pathname + url.search, local);
      }
      super(url, protocols);
    }
  };
}

const syntheticAudio = size => Buffer.from(Array.from({ length: size }, (_, offset) => offset % 113));

const cases = new Map([
  ["Fixture live basics Language: en-GB.", { model: modelId, mime: "audio/pcm", audio: syntheticAudio(20011) }],
  ["Fixture live override", { model: "fixture-live-custom", mime: "audio/wav", audio: syntheticAudio(500) }],
  ["Fixture live partial", { model: modelId, mime: "audio/pcm", audio: syntheticAudio(260) }],
  ["Fixture live goaway", { model: "fixture-live-custom", mime: "audio/pcm", audio: syntheticAudio(17200) }],
  ["Fixture live setup timeout", { model: "fixture-live-timeout-setup" }],
  ["Fixture live turn timeout", { model: "fixture-live-timeout-turn", mime: "audio/pcm", audio: syntheticAudio(320) }],
  ["Fixture live rejected", { model: "fixture-live-rejected", mime: "audio/pcm", audio: syntheticAudio(240) }],
  ["Fixture live silence", { model: "fixture-live-silence", mime: "audio/pcm", audio: syntheticAudio(230) }],
  ["Fixture live standard ack", { model: "fixture-live-custom", mime: "audio/pcm", audio: syntheticAudio(120) }],
  ["Fixture live redaction", { model: "fixture-live-redaction", mime: "audio/pcm", audio: syntheticAudio(190) }],
  ["Fixture live shared", { model: modelId, mime: "audio/pcm", audio: Buffer.from("synthetic PCM bytes") }],
]);

const rotations = new Map();
const server = createServer();
const sockets = new WebSocketServer({ server });
sockets.on("connection", (socket, request) => {
  const url = new URL(request.url, local);
  const match = path.exec(url.pathname);
  const key = url.searchParams.get("key");
  const fail = message => {
    if (socket.readyState === WebSocket.OPEN) socket.send(JSON.stringify({ error: { status: "FAILED_PRECONDITION", message } }));
  };
  if (!match || !["offline-live-stt", "offline-provider-placeholder"].includes(key) ||
    url.searchParams.size !== 1 || request.headers.authorization || request.headers["x-goog-api-key"]) {
    fail("unexpected fixture WebSocket handshake");
    return;
  }
  const model = decodeURIComponent(match[1]);
  let instruction, expected, turn = 0, chunks = [];
  const send = frame => { if (socket.readyState === WebSocket.OPEN) socket.send(JSON.stringify(frame)); };
  const delta = text => send({ serverContent: { inputTranscription: { text } } });
  const done = () => send({ serverContent: { turnComplete: true } });
  socket.on("message", bytes => {
    let frame;
    try { frame = JSON.parse(bytes.toString("utf8")); } catch { return fail("invalid fixture client frame"); }
    if (!expected) {
      const setup = frame.setup;
      instruction = setup?.systemInstruction?.parts?.[0]?.text;
      expected = cases.get(instruction);
      if (!expected || expected.model !== model || setup.model !== "models/" + model ||
        JSON.stringify(setup.generationConfig) !== '{"responseModalities":["TEXT"],"inputAudioTranscription":{}}') {
        expected = null;
        return fail("unexpected fixture setup instruction, model or config");
      }
      if (instruction === "Fixture live goaway") {
        turn = (rotations.get(instruction) || 0) + 1;
        if (turn > 2) return fail("too many fixture reconnects");
        rotations.set(instruction, turn);
      }
      if (instruction === "Fixture live setup timeout") {
        socket.send("not-json");
      } else if (instruction === "Fixture live standard ack") {
        send({ setupComplete: {} });
      } else {
        send({ serverContent: { setupComplete: true } });
      }
      return;
    }
    if (frame.realtimeInput) {
      const media = frame.realtimeInput.mediaChunks;
      if (media?.length !== 1 || media[0].mimeType !== expected.mime || typeof media[0].data !== "string") {
        return fail("unexpected fixture audio MIME or chunk shape");
      }
      const data = Buffer.from(media[0].data, "base64");
      if (!data.length || data.length > 16384 || (chunks.length && chunks.at(-1).length !== 16384)) {
        return fail("unexpected fixture audio chunk boundaries");
      }
      chunks.push(data);
      return;
    }
    if (frame.clientContent?.turnComplete !== true || frame.clientContent.turns?.[0]?.parts?.[0]?.text !== instruction) {
      return fail("unexpected fixture flush frame");
    }
    const audio = Buffer.concat(chunks);
    if (turn === 2 ? audio.length !== 0 : !expected.audio?.equals(audio)) return fail("fixture audio repeated or changed");
    if (instruction === "Fixture live turn timeout") return;
    if (instruction === "Fixture live rejected") return send({ error: { status: "INVALID_ARGUMENT", message: "fixture media rejected" } });
    if (instruction === "Fixture live redaction") return send({ error: { status: "INVALID_ARGUMENT", message: "fixture echoed " + key + " and Bearer offline-live-synthetic" } });
    if (instruction === "Fixture live silence") {
      delta(" \t\u00a0");
      return socket.close(1000);
    }
    if (instruction === "Fixture live partial") {
      delta("partial ");
      return socket.close(1000);
    }
    if (instruction === "Fixture live goaway") {
      if (turn === 1) {
        delta("first ");
        return send({ goAway: { time: new Date(Date.now() + 25).toISOString() } });
      }
      send({ goAway: {} });
      delta("pass");
      return done();
    }
    if (instruction === "Fixture live standard ack") {
      socket.send(Buffer.from(JSON.stringify({ serverContent: { inputTranscription: { text: "binary delta" } } })));
      return done();
    }
    if (instruction === "Fixture live override") {
      delta("override speech");
      return done();
    }
    socket.send("ignored fixture frame");
    delta(" \t\u00a0");
    delta("fixture ");
    delta("speech");
    done();
  });
});
server.listen(52988, "127.0.0.1").unref();
