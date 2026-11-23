import { readFileSync, writeFileSync } from "node:fs";
import { registerHooks } from "node:module";

const settingsPath = process.env.TAILSCALE_FIXTURE_SETTINGS;
const argumentsPath = process.env.TAILSCALE_FIXTURE_COMMANDS;
const readSettings = () => JSON.parse(readFileSync(settingsPath, "utf8"));
const deny = 'throw new Error("unexpected Tailscale operation")';
const mocks = new Map(Object.entries({
  "@/lib/dataDir.js": "export const DATA_DIR = process.env.DATA_DIR;",
  "@/mitm/manager": `
    export const initDbHooks = () => {};
    export const getCachedPassword = () => { ${deny} };
    export const loadEncryptedPassword = () => { ${deny} };
  `,
  "@/lib/localDb": `
    import { readFileSync, writeFileSync } from "node:fs";
    const path = ${JSON.stringify(settingsPath)};
    export const getSettings = async () => JSON.parse(readFileSync(path, "utf8"));
    export const updateSettings = async (patch) => writeFileSync(path, JSON.stringify({...await getSettings(), ...patch}));
  `,
  "./tailscale.js": `
    import { appendFileSync } from "node:fs";
    export const stopFunnel = () => appendFileSync(${JSON.stringify(argumentsPath)}, "stopFunnel\\n");
    export const startDaemonWithPassword = () => { ${deny} };
    export const startFunnel = () => { ${deny} };
    export const isTailscaleRunning = () => { ${deny} };
    export const isTailscaleRunningStrict = () => { ${deny} };
    export const isTailscaleLoggedIn = () => { ${deny} };
    export const isTailscaleLoggedInStrict = () => { ${deny} };
    export const startLogin = () => { ${deny} };
    export const provisionCert = () => { ${deny} };
  `,
  "./healthCheck.js": `export const waitForHealth = () => { ${deny} };`,
}));

registerHooks({
  resolve(specifier, context, nextResolve) {
    const source = mocks.get(specifier);
    return source === undefined
      ? nextResolve(specifier, context)
      : {url: `data:text/javascript,${encodeURIComponent(source)}`, shortCircuit: true};
  },
});
console.log = () => {};
const {disableTailscale, getTailscaleStatus} = await import("../../src/lib/tunnel/tailscale/manager.js");
const response = await disableTailscale();
const tailscale = await getTailscaleStatus();
if (readFileSync(argumentsPath, "utf8") !== "stopFunnel\n") {
  throw new Error("Expected only the pinned stop helper");
}
process.stdout.write(JSON.stringify({response, tailscale, settings: readSettings()}));
