const Module = require("node:module");
const fs = require("node:fs");
const path = require("node:path");
const load = Module._load;
const scenario = process.argv[2];
let menu, updated, killed;
class Tray {
  constructor({ menu: input }) { menu = input; }
  onClick(f) { this.click = f; }
  ready() {
    return scenario === "no-watcher" ? Promise.reject(new Error("watcher unavailable")) : Promise.resolve();
  }
  sendAction(update) { updated = update.item; }
  kill() { killed = true; }
}
Module._load = function(name, parent, main) {
  if (name === "systray2") return { default: Tray };
  if (name === "../../../hooks/sqliteRuntime") return { getRuntimeNodeModules: () => path.join(process.env.HOME, "no-runtime") };
  return load.call(this, name, parent, main);
};
const original = require("../../cli/src/cli/tray/tray");
if (scenario === "hide") {
  require("../../cli/src/cli/tray/autostart").enableAutoStart();
}
let result = { ready: false };
const handle = original.initTray({
  port: 20128,
  onReady: () => {
    result.ready = true;
    const icon = Buffer.from(menu.icon, "base64");
    const width = icon.readUInt32BE(16);
    const height = icon.readUInt32BE(20);
    result.tooltip = menu.tooltip;
    result.icon = [[width, height, width * height * 4]];
    result.menu = menu.items.map(({ title, enabled }) => [title, enabled]);
    if (scenario === "autostart" || scenario === "pre-enabled") {
      handle.click({ seq_id: 2 });
      result.autostart = updated.title;
    }
    if (scenario === "dashboard-quit") {
      handle.click({ seq_id: 1 });
      handle.click({ seq_id: 3 });
    } else {
      original.killTray();
    }
    result.removed = killed;
    console.log(JSON.stringify(result));
  },
  onError: () => {
    result.quit = false;
    console.log(JSON.stringify(result));
  },
  onOpenDashboard: () => {
    result.opened = "http://localhost:20128/dashboard";
  },
  onQuit: () => { result.quit = true; },
});
