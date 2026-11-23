const net = require("node:net");
const {
  getPidFilePath, readPidRecord, writePidRecord, removePidRecordIfOwner, stopRecordedInstance, waitForPortFree,
} = require("../../cli/src/cli/utils/launcherProcess");

async function run() {
  const [root, portValue] = process.argv.slice(2);
  const port = Number(portValue);
  const file = getPidFilePath(root);
  const previous = readPidRecord(file);
  if (previous && stopRecordedInstance(previous).length) await waitForPortFree(port);
  const listener = net.createServer();
  await new Promise((resolve, reject) => {
    listener.once("error", reject);
    listener.listen(port, "127.0.0.1", resolve);
  });
  const actual = listener.address().port;
  writePidRecord(file, {cliPid: process.pid, serverPid: null, port: actual, exe: require("node:path").basename(process.execPath)});
  console.log("ready", actual);
  process.on("SIGTERM", () => {
    removePidRecordIfOwner(file, process.pid);
    listener.close(() => process.exit(0));
  });
}

run().catch((error) => { console.error(error.message); process.exitCode = 1; });
