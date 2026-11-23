const api = require("../../cli/src/cli/api/client");
const { startTerminalUI } = require("../../cli/src/cli/terminalUI");
const port = Number(process.argv[2].split(":").pop());
api.configure({ host: "127.0.0.1" });
startTerminalUI(port).then(() => process.exit(0)).catch((error) => {
  console.error(error);
  process.exit(1);
});
