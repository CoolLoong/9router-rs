import go from "../../open-sse/providers/registry/opencode-go.js";
import zen from "../../open-sse/providers/registry/opencode-zen.js";

const providers = Object.fromEntries([go, zen].map(provider => [
  provider.id,
  {
    transports: Object.fromEntries(provider.transports.map(transport => [transport.format, {
      url: transport.baseUrl,
      thinkingFormat: transport.thinkingFormat,
    }])),
    models: Object.fromEntries(provider.models.map(model => [model.id, {
      supported: model.supportedFormats,
      target: model.targetFormat,
      kind: model.kind,
    }])),
  },
]));
process.stdout.write(JSON.stringify(providers) + "\n");
