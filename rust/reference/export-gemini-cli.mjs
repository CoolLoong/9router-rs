import registry from "../../open-sse/providers/registry/gemini-cli.js";
import { DEFAULT_THINKING_GEMINI_CLI_SIGNATURE } from "../../open-sse/config/defaultThinkingSignature.js";

const { baseUrl, cliVersion, apiClient } = registry.transport;
process.stdout.write(JSON.stringify({
  baseUrl, cliVersion, apiClient, signature: DEFAULT_THINKING_GEMINI_CLI_SIGNATURE,
}, null, 2) + "\n");
