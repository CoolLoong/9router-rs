import { registerHooks } from "node:module";

const root = new URL("../../", import.meta.url);
registerHooks({
  resolve(specifier, context, next) {
    if (specifier.startsWith("open-sse/")) {
      return next(new URL(specifier, root).href, context);
    }
    if (specifier.startsWith("@/")) {
      const file = specifier.slice(2);
      return next(new URL(`src/${file.endsWith(".js") ? file : file + ".js"}`, root).href, context);
    }
    return next(specifier, context);
  },
});
