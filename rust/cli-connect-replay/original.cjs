const path = require("node:path");
const Module = require("node:module");
const load = Module._load;
const home = path.join(process.cwd(), "fixture-home");
process.env.DATA_DIR = path.join(process.cwd(), "fixture-data");
Module._load = function(name, parent, main) {
  if (name === "os" || name === "node:os") {
    return { ...load.call(this, name, parent, main), homedir: () => home, hostname: () => "fixture-host" };
  }
  return load.call(this, name, parent, main);
};
require("../../cli/src/cli/commands/connect.js").run(process.argv.slice(2))
  .then((code) => { process.exitCode = code; })
  .catch((error) => {
    console.error(`❌ ${error?.message || error}`);
    process.exitCode = 1;
  });
