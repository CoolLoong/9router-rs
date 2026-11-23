const path = require("node:path");
const fs = require("node:fs");
const Module = require("node:module");
const vm = require("node:vm");
const loader = Module._load;
const home = path.join(process.cwd(), "fixture-home");
const cli = require.resolve("../../cli/cli.js");
if (process.env.ROUTER_CLI_XDG_FIXTURE) {
  process.env.XDG_CONFIG_HOME = path.join(process.cwd(), "fixture-xdg");
} else {
  delete process.env.XDG_CONFIG_HOME;
}
global.fetch = () => { throw new Error("CLI fixture must be offline"); };
Module._load = function (name, parent, main) {
  if (name === "os" || name === "node:os") {
    return { ...loader.call(this, name, parent, main), homedir: () => home };
  }
  if (parent?.filename === cli) {
    if (name === "./hooks/sqliteRuntime") return { ensureSqliteRuntime() {}, buildEnvWithRuntime() {} };
    if (name === "./hooks/trayRuntime") return { ensureTrayRuntime() {} };
  }
  return loader.call(this, name, parent, main);
};
if (process.env.ROUTER_CLI_STARTUP_FIXTURE) {
  const source = fs.readFileSync(cli, "utf8");
  const begin = source.indexOf("// Parse arguments");
  const end = source.indexOf("// Always use Node.js runtime", begin);
  if (begin < 0 || end < begin) throw new Error("Pinned launcher parser was not found");
  const sandbox = {
    args: process.argv.slice(2),
    DEFAULT_PORT: 20128,
    DEFAULT_HOST: "0.0.0.0",
    process: { env: {}, stdin: { isTTY: process.env.ROUTER_CLI_STARTUP_FIXTURE === "tty" } },
  };
  const options = vm.runInNewContext(
    source.slice(begin, end) + "\nJSON.stringify({port,host,noBrowser,skipUpdate,showLog,trayMode});",
    sandbox,
  );
  console.log(options);
} else {
  require(cli);
}
