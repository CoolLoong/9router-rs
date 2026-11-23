import { fetchGeminiVoices } from "../../open-sse/handlers/ttsProviders/gemini.js";

const letters = "abcdefghijklmnopqrstuvwxyz";
const language = new Intl.DisplayNames(["en"], { type: "language", fallback: "none" });
const region = new Intl.DisplayNames(["en"], { type: "region", fallback: "none" });
const script = new Intl.DisplayNames(["en"], { type: "script", fallback: "none" });
const languages = {};
const regions = {};
const scripts = {};

function add(target, names, code) {
  const label = names.of(code);
  if (label) target[code.toLowerCase()] = label;
}

for (const a of letters) {
  for (const b of letters) {
    const code = a + b;
    add(languages, language, code);
    add(regions, region, code.toUpperCase());
    for (const c of letters) add(languages, language, code + c);
  }
}
for (let regionId = 0; regionId < 1000; regionId++) add(regions, region, String(regionId).padStart(3, "0"));
for (const code of ["Arab", "Armn", "Beng", "Cyrl", "Deva", "Ethi", "Geor", "Grek", "Gujr", "Guru", "Hans", "Hant", "Hebr",
  "Jpan", "Khmr", "Knda", "Kore", "Laoo", "Latn", "Mlym", "Mymr", "Orya", "Sinh", "Taml", "Telu", "Thai", "Tibt"]) {
  add(scripts, script, code);
}
for (const code of ["ar-001", "de-AT", "de-CH", "en-AU", "en-CA", "en-GB", "en-US", "en-001", "en-150", "es-419", "es-ES", "es-MX",
  "fa-AF", "fr-CA", "fr-CH", "nl-BE", "pt-BR", "pt-PT", "ro-MD", "sr-Cyrl", "sr-Latn", "zh-Hans", "zh-Hant"]) {
  add(languages, language, code);
}
process.stdout.write(JSON.stringify({ gemini: await fetchGeminiVoices(), languages, regions, scripts }) + "\n");
