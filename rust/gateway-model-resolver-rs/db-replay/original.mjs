
import { registerHooks } from "node:module";
const source = new URL("../../../", import.meta.url);
const databaseModules = new URL("src/lib/db/", source).href;
registerHooks({
  resolve(specifier, context, next) {
    if (specifier === "@/lib/localDb") {
      return next(new URL("./local-db.mjs", import.meta.url).href, context);
    }
    if (specifier.startsWith("open-sse/")) {
      return next(new URL(specifier, source).href, context);
    }
    if (specifier === "../driver.js" && context.parentURL?.startsWith(databaseModules)) {
      return next(new URL("./adapter.mjs", import.meta.url).href, context);
    }
    return next(specifier, context);
  },
});
const { getModelInfo, getComboModels } = await import(new URL("src/sse/services/model.js", source).href);
const { getComboByName } = await import("./local-db.mjs");
const model = process.argv[2];
const { provider, model: id } = await getModelInfo(model);
console.log(JSON.stringify({
  info: provider == null ? null : [provider, id],
  seats: await getComboModels(model),
  comboExists: (await getComboByName(model)) != null,
}));
