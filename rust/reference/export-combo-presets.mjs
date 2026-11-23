import "./provider-modules.mjs";

const { buildPresetItems } = await import("../../src/lib/comboPresets.js");
process.stdout.write(JSON.stringify({
  cursor: buildPresetItems("cursor").map(({ name, models }) => ({ name, models })),
  claude: buildPresetItems("claude").map(({ name, models }) => ({ name, models })),
}, null, 2) + "\n");
