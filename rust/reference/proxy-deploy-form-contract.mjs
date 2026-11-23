import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { runInNewContext } from "node:vm";

const source = readFileSync(new URL("../app/src/dashboard_assets/optimization.js", import.meta.url), "utf8");
const nodes = new Map();
const calls = [];
const settle = () => new Promise(setImmediate);
let finish;

function select(id) {
  if (!nodes.has(id)) {
    const handlers = new Map();
    const node = {
      value: "", options: [], disabled: false, opened: false,
      addEventListener(event, listener) { handlers.set(event, listener); },
      fire(event) { handlers.get(event)?.({currentTarget: node, preventDefault() {}}); },
      reset() {}, replaceChildren() {}, querySelector() { return select(`${id}-submit`); },
      showModal() { this.opened = true; }, close() { this.opened = false; this.fire("close"); },
    };
    nodes.set(id, node);
  }
  return nodes.get(id);
}

function request(path, method, body) {
  calls.push(body === undefined ? {path, method} : {path, method, body: JSON.parse(JSON.stringify(body))});
  if (["cloudflare", "deno", "netlify", "vercel"].some(kind => path === `/api/proxy-pools/${kind}-deploy`)) {
    return new Promise((resolve, reject) => { finish = (error) => error ? reject(error) : resolve({}); });
  }
  assert.equal(path, "/api/proxy-pools?includeUsage=true");
  return Promise.resolve({proxyPools: []});
}

const notice = (node, message = "", failed = false) => Object.assign(node, {message, failed});
runInNewContext(source, {
  URL,
  document: {body:{dataset:{page:"proxy-pools"}},querySelectorAll:()=>[]},
  window: {nineRouterUi:{select,request,notice,element:()=>({}),action:()=>({})},
    open() { throw new Error("Unexpected browser action"); }},
});
await settle();
assert.equal(calls.length, 1);
for (const [kind,label,accountField,tokenField] of [
  ["cloudflare", "Cloudflare", "accountId", "apiToken"],
  ["deno", "Deno", "orgDomain", "denoToken"],
  ["netlify", "Netlify", null, "netlifyToken"],
  ["vercel", "Vercel", "teamId", "vercelToken"],
]) {
  for (const rejected of [false, true]) {
    const form = select(`pool-${kind}-form`);
    const dialog = select(`pool-${kind}-dialog`);
    const token = select(`pool-${kind}-token`);
    const fake = "fixture-private-relay-control-token";
    select(`pool-${kind}-deploy`).fire("click");
    assert.equal(dialog.opened, true);
    const account = kind === "vercel" && !rejected ? "" : "fixture-account";
    if (accountField) select(`pool-${kind}-account`).value = account;
    select(`pool-${kind}-name`).value = " fixture-relay ";
    token.value = fake;
    form.fire("submit");
    assert.equal(token.value, "");
    assert.deepEqual(calls.at(-1), {path:`/api/proxy-pools/${kind}-deploy`,method:"POST",
      body:{...(accountField && account ? {[accountField]:account} : {}),[tokenField]:fake,projectName:"fixture-relay"}});
    finish(rejected ? new Error(`Denied ${fake}`) : undefined);
    await settle();
    assert.equal(dialog.opened, rejected);
    if (rejected) assert.equal(select(`pool-${kind}-error`).message, "Denied [redacted]");
    else assert.equal(select("optimization-status").message, `${label} relay deployed.`);
    dialog.close();
    assert.equal(token.value, "");
  }
}
console.log("Cloudflare, Deno, Netlify and Vercel relay forms proved authenticated payload shape and token clearing and redaction.");
