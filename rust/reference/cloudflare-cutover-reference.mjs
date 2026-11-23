import { readFileSync, writeFileSync } from "node:fs";
import { registerHooks } from "node:module";
import { join } from "node:path";

const root = process.env.DATA_DIR;
const fixtureUrl = "https://fixture-cutover.trycloudflare.com";
const settingsPath = join(root, "fixture-settings.json");
const statePath = join(root, "tunnel", "state.json");
const readSettings = () => JSON.parse(readFileSync(settingsPath, "utf8"));
const mocks = new Map(Object.entries({
  "@/lib/dataDir.js": "export const DATA_DIR = process.env.DATA_DIR;",
  "@/lib/localDb": `
    import { readFileSync, writeFileSync } from "node:fs";
    const path = ${JSON.stringify(settingsPath)};
    export const getSettings = async () => JSON.parse(readFileSync(path, "utf8"));
    export const updateSettings = async (patch) => writeFileSync(path, JSON.stringify({...await getSettings(), ...patch}));
  `,
  "./cloudflared.js": `
    let running = false;
    export const isCloudflaredRunning = () => running;
    export const spawnQuickTunnel = async () => {
      running = true;
      return {tunnelUrl: ${JSON.stringify(fixtureUrl)}};
    };
    export const killCloudflared = () => { running = false; };
    export const setUnexpectedExitHandler = () => {};
  `,
  "./healthCheck.js": "export const waitForHealth = async () => {}; export const probeUrlAlive = async () => true;",
}));

registerHooks({
  resolve(specifier, context, nextResolve) {
    const source = mocks.get(specifier);
    return source === undefined
      ? nextResolve(specifier, context)
      : {url: `data:text/javascript,${encodeURIComponent(source)}`, shortCircuit: true};
  },
});

globalThis.fetch = async (url, init) => {
  if (url !== "https://abc-tunnel.us/api/tunnel/register" || init?.method !== "POST") {
    throw new Error("Unexpected reference fixture request");
  }
  const request = JSON.parse(init.body);
  if (request.shortId !== "mig123" || request.tunnelUrl !== fixtureUrl) {
    throw new Error("Incorrect registration");
  }
  return new Response(null, {status: 204});
};
console.log = () => {};

const {enableTunnel, disableTunnel, getTunnelStatus} = await import("../../src/lib/tunnel/cloudflare/manager.js");
const output = {};
if (process.argv[2] === "restore") {
  await enableTunnel();
  output.resumed = await getTunnelStatus();
  output.reused = await enableTunnel();
} else {
  output.saved = await getTunnelStatus();
}
output.disabled = await disableTunnel();
output.current = await getTunnelStatus();
output.state = JSON.parse(readFileSync(statePath, "utf8"));
output.settings = readSettings();
process.stdout.write(JSON.stringify(output));
