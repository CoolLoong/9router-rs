import { readFileSync } from "node:fs";
import { createHash } from "node:crypto";
import { inflateSync } from "node:zlib";
import { pathToFileURL } from "node:url";
import { join } from "node:path";
import { compressWithPxpipe } from "../../open-sse/rtk/pxpipe.js";

const input = JSON.parse(readFileSync(0, "utf8"));
const library = (entry) => import(pathToFileURL(join(process.env.PXPIPE_LIBRARY, "dist/core", entry)));
function pageEvidence(png, width, height) {
    const bytes = Buffer.from(png);
    const length = bytes.readUInt32BE(33);
    const filtered = inflateSync(bytes.subarray(41, 41 + length));
    const channels = bytes[25] === 2 ? 3 : 1;
    const rowBytes = width * channels;
    const raster = Buffer.alloc(rowBytes * height);
    const stride = rowBytes + 1;
    for (let y = 0; y < height; y++) {
        if (filtered[y * stride] !== 3) throw new Error("unexpected PNG filter");
        for (let x = 0; x < rowBytes; x++) {
            const i = y * rowBytes + x;
            const left = x >= channels ? raster[i - channels] : 0;
            const above = y ? raster[i - rowBytes] : 0;
            raster[i] = filtered[y * stride + x + 1] + ((left + above) >> 1);
        }
    }
    return {width, height, pixelSha256: createHash("sha256").update(raster).digest("hex")};
}
if (input.render) {
    const {renderTextToImages} = await library("library.js");
    const {text, repeat, rowSpacing, style, ...options} = input.render;
    const result = await renderTextToImages(text.repeat(repeat ?? 1), {
        ...options, style: {aa: true, cellHBonus: rowSpacing ?? 0, ...style},
    });
    const pages = result.pages.map(({width, height, png}) => pageEvidence(png, width, height));
    console.log(JSON.stringify({pages, droppedChars:result.droppedChars}));
    process.exit(0);
}
if (input.factsheet) {
    const {factSheetText} = await library("factsheet.js");
    const {text, repeat} = input.factsheet;
    console.log(JSON.stringify(factSheetText(text.repeat(repeat ?? 1))));
    process.exit(0);
}
if (input.actual) {
    const {transformAnthropicMessages} = await library("library.js");
    const {model, body, repeat, scope, overrides, minChars, resultText, resultRepeat} = input.actual;
    if (scope === undefined) delete process.env.PXPIPE_MODELS;
    else process.env.PXPIPE_MODELS = scope;
    if (overrides === undefined) delete process.env.PXPIPE_GPT_PROFILES;
    else process.env.PXPIPE_GPT_PROFILES = overrides;
    if (typeof body.system === "string") body.system = body.system.repeat(repeat ?? 1);
    else if (Array.isArray(body.system)) {
        for (const block of body.system) {
            block.text = block.text.replaceAll("{fixture}", () => "Fixture repetition with docs and ids. ".repeat(repeat ?? 1));
        }
    }
    for (const tool of body.tools ?? []) {
        if (typeof tool.description === "string") {
            tool.description = tool.description.replaceAll("{fixture}", () => "Fixture repetition with docs and ids. ".repeat(repeat ?? 1));
        }
    }
    if (typeof resultText === "string") {
        const substitute = value => {
            if (typeof value === "string") return value.replaceAll("{result}", () => resultText.repeat(resultRepeat ?? 1));
            if (Array.isArray(value)) return value.map(substitute);
            if (value && typeof value === "object") {
                return Object.fromEntries(Object.entries(value).map(([key, field]) => [key, substitute(field)]));
            }
            return value;
        };
        body.messages = substitute(body.messages);
    }
    const originalTimer = setTimeout;
    let timer;
    globalThis.setTimeout = (handler, delay) => (timer = originalTimer(handler, delay));
    const result = await compressWithPxpipe(body, {
        enabled: true, format: "claude", model, minChars, transform: transformAnthropicMessages,
    });
    clearTimeout(timer);
    delete result.summary.durationMs;
    delete result.summary.imageBytes;
    delete result.summary.compressedBodyChars;
    if (result.body) {
        const evidence = block => {
            if (Array.isArray(block)) block.forEach(evidence);
            else if (block && typeof block === "object") {
                if (block.type !== "image" || block.source?.media_type !== "image/png" || typeof block.source?.data !== "string") {
                    Object.values(block).forEach(evidence);
                    return;
                }
                const bytes = Buffer.from(block.source.data, "base64");
                block.source.data = pageEvidence(bytes, bytes.readUInt32BE(16), bytes.readUInt32BE(20));
            }
        };
        evidence(result.body.messages);
    }
    console.log(JSON.stringify(result));
    process.exit(0);
}
const { body, outcome, available, ...options } = input;
const timeout = globalThis.setTimeout;
let timer;
globalThis.setTimeout = (handler, delay) => (timer = timeout(handler, delay));
let request;
const result = await compressWithPxpipe(body, {
    ...options,
    transform: available ? async ({ body, model, options }) => {
        request = { body: JSON.parse(new TextDecoder().decode(body)), model, options };
        if (outcome?.error) throw new Error(outcome.error);
        if (outcome?.timeout) return new Promise(() => {});
        if (!outcome?.applied) return outcome ?? {};
        return { ...outcome, body: new TextEncoder().encode(JSON.stringify(outcome.body)) };
    } : null,
});
clearTimeout(timer);
delete result.summary.durationMs;
console.log(JSON.stringify({ ...result, ...(request && { request }) }));
