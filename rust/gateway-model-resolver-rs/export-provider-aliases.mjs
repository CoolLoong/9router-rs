import registry from "../../open-sse/providers/registry/index.js";
import { resolveProviderAlias } from "../../open-sse/services/model.js";

const aliases = new Set(["el", "jina", "jina-ai", "polly", "aws-polly"]);
for (const provider of registry) {
  for (const alias of [provider.id, provider.alias, ...(provider.aliases || [])]) {
    if (alias) aliases.add(alias);
  }
}
const values = [...aliases].sort().map(alias => [alias, resolveProviderAlias(alias)]);
process.stdout.write(JSON.stringify(Object.fromEntries(values), null, 2) + "\n");
