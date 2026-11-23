import { CAVEMAN_PROMPTS } from "../../open-sse/rtk/cavemanPrompts.js";
import { PONYTAIL_PROMPTS } from "../../open-sse/rtk/ponytailPrompt.js";

process.stdout.write(JSON.stringify({ caveman: CAVEMAN_PROMPTS, ponytail: PONYTAIL_PROMPTS }, null, 2) + "\n");
