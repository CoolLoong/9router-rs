import registry from "../../open-sse/providers/registry/codex.js";
import { CODEX_DEFAULT_INSTRUCTIONS } from "../../open-sse/config/codexInstructions.js";

process.stdout.write(JSON.stringify({
  endpoint: registry.transport.baseUrl,
  version: registry.transport.cliVersion,
  models: Object.fromEntries(registry.models.map(m => [m.id, {
    upstream: m.upstreamModelId, lite: m.responsesLite || false, levels: m.thinkingLevels,
  }])),
  instructions: CODEX_DEFAULT_INSTRUCTIONS,
}, null, 2) + "\n");
