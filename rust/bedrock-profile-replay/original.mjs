import fs from "node:fs";
import { resolveAwsCredentials } from "../../open-sse/shared/awsCredentials.js";

const cases = JSON.parse(fs.readFileSync(0, "utf8"));
const outcomes = [];
for (const { name, account, expected } of cases) {
    const credentials = await resolveAwsCredentials(account);
    if (credentials.mode !== "profile" ||
        credentials.accessKeyId !== expected[0] ||
        credentials.secretAccessKey !== expected[1] ||
        credentials.sessionToken !== (expected[2] || undefined)) {
        throw new Error("Profile did not resolve the expected fixture credential");
    }
    outcomes.push({ name, mode: credentials.mode, region: credentials.region, accepted: true });
}
console.log(JSON.stringify(outcomes));
