import { readFile } from "node:fs/promises";

globalThis.fetch = () => { throw new Error("Provider metadata export must be offline"); };
const source = new URL("../../open-sse/providers/capabilities.js", import.meta.url);
const caps = await import(source.href);
const code = await readFile(source, "utf8");
const denylist = code.match(/^const COMMANDCODE_TEXT_ONLY = new Set\(\[([\s\S]*?)^\]\);$/m)?.[1];
if (!denylist) throw new Error("Missing original CommandCode denylist");
const commandCodeTextOnly = JSON.parse("[" + denylist.trim().replace(/,$/, "") + "]");
if (!commandCodeTextOnly.every((model) => typeof model === "string")) {
  throw new Error("Unexpected CommandCode denylist");
}

const visionCode = await readFile(new URL("./visionPatterns.js", source), "utf8");
const { NOT_VISION, VISION_NAME } = await import("data:text/javascript;base64," +
  Buffer.from(visionCode + "\nexport { NOT_VISION, VISION_NAME };").toString("base64"));

const data = {
  defaults: caps.DEFAULT_CAPABILITIES,
  model: caps.MODEL_CAPABILITIES,
  provider: caps.PROVIDER_CAPABILITIES,
  patterns: caps.PATTERN_CAPABILITIES.map(({ pattern, caps }) => {
    if (/[^\x00-\x7f]/.test(pattern)) throw new Error("Expected an ASCII capability glob");
    return { pattern: pattern.toLowerCase(), caps };
  }),
  serviceKind: Object.fromEntries(
    ["imageToText", "image", "stt", "tts", "embedding"].map((kind) => [kind, caps.capabilitiesFromServiceKind(kind)]),
  ),
  commandCodeTextOnly,
  notVision: NOT_VISION.source.replaceAll("\\/", "/"),
  visionName: VISION_NAME.source.replaceAll("\\/", "/"),
};
process.stdout.write(JSON.stringify(data, null, 2) + "\n");
