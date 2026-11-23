import { registerHooks } from "node:module";

const constants = new URL("../../src/shared/constants/keyAccess.js", import.meta.url);
const hooks = registerHooks({
  resolve(specifier, context, nextResolve) {
    return nextResolve(
      specifier === "@/shared/constants/keyAccess.js" ? constants.href : specifier,
      context,
    );
  },
});
const original = await import("../../src/shared/utils/keyAccess.js");
hooks.deregister();

export const {
  normalizeAllowList,
  keyAccessFromColumns,
  keyAccessToColumns,
  validateKeyAccessInput,
} = original;
