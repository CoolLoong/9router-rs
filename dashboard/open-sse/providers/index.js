import registry from "./registry/index.js";
import { normalizeModel } from "./models/schema.js";
import { buildTtsProviderModels } from "../config/ttsModels.js";

export const PROVIDER_MODELS = Object.assign(
  Object.fromEntries(
    registry.filter(entry => entry.models !== undefined)
      .map(entry => [entry.alias || entry.id, entry.models.map(normalizeModel)])
  ),
  buildTtsProviderModels(),
);

export const PROVIDERS = Object.fromEntries(
  registry.map(({ id, thinkingFormat, thinkingTransports }) => [id, {
    thinkingFormat,
    transports: thinkingTransports,
  }]),
);
