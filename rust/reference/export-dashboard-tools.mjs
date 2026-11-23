import { CLI_TOOLS } from "../../src/shared/constants/cliTools.js";

const tools = Object.values(CLI_TOOLS).map(({ id, name, description }) => ({ id, name, description }));
process.stdout.write(JSON.stringify(tools, null, 2) + "\n");
