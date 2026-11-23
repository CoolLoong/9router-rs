import gemini from "../../open-sse/providers/registry/gemini.js";

const models = gemini.models.filter(model => model.kind === "stt" && model.transport === "gemini-live").map(model => model.id);
process.stdout.write(JSON.stringify({ baseUrl: gemini.sttConfig.baseUrl, models }) + "\n");
