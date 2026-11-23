import { PROVIDER_MODELS } from "../../open-sse/providers/index.js";

const catalog = Object.entries(PROVIDER_MODELS).map(([alias, models]) => [
  alias,
  models.map(({ id, name }) => ({ id, name })),
]);
process.stdout.write(`${JSON.stringify(catalog)}\n`);
