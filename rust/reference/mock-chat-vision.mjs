const image = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+/l7kAAAAASUVORK5CYII=";
const inline = `data:image/png;base64,${image}`;
const remote = "https://example.invalid/fixture.webp?kind=vision";

const imagePart = (url, detail) => ({ type: "image_url", image_url: { url, ...(detail ? { detail } : {}) } });
const responsePart = (url, detail) => ({ type: "input_image", image_url: url, detail });
const chatCases = new Map([
  ["fixture vision native Chat", [imagePart(inline, "high"), imagePart(remote)]],
  ["fixture vision Responses to Chat", [imagePart(inline, "auto")]],
  ["fixture vision Messages to Chat", [imagePart(inline)]],
  ["fixture vision static OpenAI", [imagePart(inline, "low")]],
]);
const responseCases = new Map([
  ["fixture vision Chat to native Responses", [responsePart(remote, "auto"), responsePart(inline, "high")]],
  ["fixture vision native Responses", [{ type: "input_image", file_id: "file_fixture_image", detail: "low" }]],
]);
const geminiCases = new Set([
  "fixture vision Chat to Gemini", "fixture vision Messages to Gemini", "fixture vision Responses to Gemini",
]);

function compareParts(parts, wanted) {
  if (parts.length !== wanted.length) return false;
  return wanted.every((expected, index) => {
    const actual = parts[index];
    return actual && Object.keys(actual).length === Object.keys(expected).length &&
      Object.entries(expected).every(([key, value]) => JSON.stringify(actual[key]) === JSON.stringify(value));
  });
}

export function verifyChatVision(body) {
  const parts = body.messages?.at(-1)?.content;
  const expected = chatCases.get(parts?.[0]?.text);
  return !expected || compareParts(parts.slice(1), expected);
}

export function verifyResponsesVision(body) {
  const parts = body.input?.[0]?.content;
  const expected = responseCases.get(parts?.[0]?.text);
  return !expected || compareParts(parts.slice(1), expected);
}

export function verifyGeminiVision(body) {
  const parts = body.contents?.[0]?.parts;
  if (!geminiCases.has(parts?.[0]?.text)) return true;
  return compareParts(parts.slice(1), [{ inlineData: { mime_type: "image/png", data: image } }]);
}
