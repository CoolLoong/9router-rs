import fs from "node:fs";
import { compressMessages } from "../../open-sse/rtk/index.js";
const body = JSON.parse(fs.readFileSync(0,"utf8"));
compressMessages(body, true);
process.stdout.write(JSON.stringify(body) + "\n");
