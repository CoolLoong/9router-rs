import { PROVIDERS } from "../../open-sse/config/providers.js";
import { getTargetFormat } from "../../open-sse/services/provider.js";

const formats = Object.fromEntries(Object.keys(PROVIDERS).sort()
  .map((id) => [id, getTargetFormat(id)])
  .filter(([, format]) => format !== "openai"));

console.log(JSON.stringify(formats, null, 2));
