const { run } = require("../../cli/src/cli/commands/xaiVideo.js");
run(process.argv.slice(2)).then((code) => { process.exitCode = code; }).catch((err) => {
  console.error(`❌ ${err?.message || err}`);
  process.exitCode = 1;
});
