import * as source from "../../open-sse/providers/capabilities.js";

function withCatalog(catalog, resolve) {
  if (!catalog) return resolve();
  const base = (model) => model.split("/").pop().toLowerCase().split(":")[0];
  source.setCatalogSource({
    getModalities(provider, model) {
      return provider && catalog.models?.[provider + ":" + base(model)] || null;
    },
    getLimits(provider, model) {
      const byProvider = provider && catalog.providers?.[provider];
      return byProvider && (byProvider[model] || byProvider[base(model)]) || null;
    },
  });
  try {
    return resolve();
  } finally {
    source.setCatalogSource(null);
  }
}

export const capabilitiesFromServiceKind = source.capabilitiesFromServiceKind;
export const isGemini4Model = source.isGemini4Model;

export function getCapabilitiesForModel(provider, model, catalog) {
  return withCatalog(catalog, () => source.getCapabilitiesForModel(provider, model));
}

export function aggregateComboCapabilities(models, lookup, fixtures, depth) {
  if (fixtures && Object.hasOwn(fixtures, "fixtureOverrides")) {
    return withCatalog(fixtures.catalog, () =>
      source.aggregateComboCapabilities(models, lookup, (id) => fixtures.fixtureOverrides?.[id], depth)
    );
  }
  return source.aggregateComboCapabilities(models, lookup, fixtures, depth);
}
