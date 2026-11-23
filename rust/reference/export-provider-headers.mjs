import "./provider-modules.mjs";

const { default: registry } = await import("../../open-sse/providers/registry/index.js");
const { PROVIDERS } = await import("../../open-sse/providers/index.js");
const { resolveProviderAlias } = await import("../../open-sse/services/model.js");
const names = new Set(["el", "jina", "jina-ai", "polly", "aws-polly"]);
for (const { id, alias, aliases = [] } of registry) {
  names.add(id);
  if (alias) names.add(alias);
  for (const name of aliases) names.add(name);
}
const aliases = Object.fromEntries([...names].sort().map(name => [name, resolveProviderAlias(name)]));
const builtin = Object.fromEntries(Object.entries(PROVIDERS)
  .filter(([, value]) => Object.keys(value?.headers ?? {}).length)
  .map(([id, value]) => [id, value.headers]));
process.stdout.write(JSON.stringify({ aliases, builtin }, null, 2) + "\n");
