import "./provider-modules.mjs";
const [{ default: registry }, { CATALOG_VERSION }, { PROVIDER_ALIASES }] = await Promise.all([
  import("../../open-sse/providers/registry/index.js"),
  import("../../open-sse/providers/catalogOverride.js"),
  import("../../src/lib/modelCatalog/sync.js"),
]);

const rows = registry.flatMap(provider => (provider.models || []).map(model => ({
  provider: provider.id,
  model: model.id,
  contextLength: model.contextLength,
})));
process.stdout.write(JSON.stringify({ version: CATALOG_VERSION, aliases: PROVIDER_ALIASES, rows }) + "\n");
