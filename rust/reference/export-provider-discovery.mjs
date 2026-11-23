import { readFileSync } from "node:fs";
const source = readFileSync(new URL("../../src/app/api/providers/[id]/models/route.js", import.meta.url), "utf8");
const models = Object.fromEntries([...source.matchAll(/^\s*(?:"([^"]+)"|([\w-]+)):\s*createOpenAIModelsConfig\("([^"]+)"\),/gm)]
  .map(([, quoted, bare, url]) => [quoted || bare, url]));
process.stdout.write(JSON.stringify(models, null, 2) + "\n");
