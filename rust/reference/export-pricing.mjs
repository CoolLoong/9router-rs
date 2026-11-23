import {
  FREE_MODEL_NAMESPACES, ZERO_PRICING, MODEL_PRICING, PROVIDER_PRICING, PATTERN_PRICING,
} from "../../open-sse/providers/pricing.js";

process.stdout.write(JSON.stringify({
  free: FREE_MODEL_NAMESPACES, zero: ZERO_PRICING, model: MODEL_PRICING,
  provider: PROVIDER_PRICING, patterns: PATTERN_PRICING,
}) + "\n");
