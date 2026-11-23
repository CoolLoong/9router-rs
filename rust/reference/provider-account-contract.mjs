import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { runInNewContext } from "node:vm";

const source = readFileSync(new URL("../app/src/dashboard_assets/provider-account.js", import.meta.url), "utf8");
const copy = (value) => value === undefined ? undefined : JSON.parse(JSON.stringify(value));
const settle = () => new Promise(setImmediate);

class Element {
  constructor(tag, text, className) {
    this.tag = tag;
    this.textContent = text ?? "";
    this.className = className;
    this.dataset = {};
    this.children = [];
    this.events = new Map();
    this.value = "";
  }

  append(...nodes) {
    this.children.push(...nodes);
  }

  insertBefore(node, before) {
    this.children.splice(this.children.indexOf(before), 0, node);
  }

  setAttribute(name, value) {
    this[name] = value;
  }

  removeAttribute(name) {
    delete this[name];
  }

  descendants() {
    return this.children.flatMap(child => [child, ...child.descendants()]);
  }

  querySelectorAll(tag) {
    return this.descendants().filter(node => node.tag === tag);
  }

  addEventListener(name, listener) {
    this.events.set(name, listener);
  }

  reset() {
    for (const child of this.descendants().filter(node => "defaultValue" in node)) {
      child.value = child.defaultValue;
    }
  }

  async fire(name) {
    this.events.get(name)?.({ currentTarget: this, preventDefault() {} });
    await settle();
  }
}

function rig(provider, local, answer, origin = "http://localhost:20128") {
  const output = new Element("p");
  const controls = new Element("div");
  const section = new Element("section");
  section.dataset = { provider, local: String(local) };
  const ids = new Map([["provider-account", section], ["provider-account-status", output],
                       ["provider-account-forms", controls]]);
  const calls = [];
  const updates = [];
  const windowEvents = new Map();
  const timers = new Map();
  let timerId = 0;
  let tick = 1000;
  const element = (tag, text, className) => new Element(tag, text, className);
  const action = (text, listener) => {
    const result = element("button", text);
    result.addEventListener("click", listener);
    return result;
  };
  const request = async (route, method = "GET", data) => {
    calls.push({ route, method, data: copy(data) });
    return copy(await answer(route, data));
  };
  const notice = (node, value, failed) => {
    node.textContent = value || "";
    node.dataset.error = String(Boolean(failed));
  };
  const window = {
    nineRouterUi: { select: (id) => ids.get(id), request, notice, element, action },
    location: new URL("/dashboard/providers/" + provider, origin),
    open() { throw new Error("The UI opened a browser window"); },
    addEventListener(name, listener) { windowEvents.set(name, listener); },
  };
  runInNewContext(source, {
    window, URL, URLSearchParams, Event,
    Date: { now: () => tick },
    document: { dispatchEvent: (event) => updates.push(event.type) },
    setTimeout: (fn, delay) => {
      const id = ++timerId;
      timers.set(id, { fn, delay });
      return id;
    },
    clearTimeout: (id) => timers.delete(id),
  }, { filename: "provider-account.js" });
  return {
    output, controls, calls, updates, timers, windowEvents,
    button: (label) => controls.querySelectorAll("button").find(node => node.textContent === label),
    field: (name, index = 0) => controls.descendants().filter(node => node.name === name)[index],
    form: (index = 0) => controls.querySelectorAll("form")[index],
    async advance(expected) {
      assert.equal(timers.size, 1);
      const [[id, timer]] = timers;
      assert.equal(timer.delay, expected);
      timers.delete(id);
      tick += timer.delay;
      timer.fn();
      await settle();
    },
  };
}

const deviceCode = {
  device_code: "offline-private-device-code", user_code: "ABCD-EFGH",
  codeVerifier: "offline-private-verifier", verification_uri: "https://github.com/login/device",
  interval: 5, expires_in: 100,
};
const pollReplies = [
  { pending: true, error: "authorization_pending" },
  { pending: true, error: "slow_down" },
  { success: true, connection: { id: "fixture" } },
];
const github = rig("github", true, (route) =>
  route === "/api/oauth/github/device-code" ? deviceCode : pollReplies.shift());
assert.equal(github.calls.length, 0);
await github.button("Generate GitHub device code").fire("click");
const code = github.controls.querySelectorAll("code")[0];
const link = github.controls.querySelectorAll("a")[0];
assert.equal(code.textContent, deviceCode.user_code);
assert.equal(link.href, "https://github.com/login/device");
assert.equal(link.rel, "noopener noreferrer");
assert.deepEqual(github.calls.map(row => row.route), ["/api/oauth/github/device-code"]);
await github.advance(5000);
await github.advance(5000);
await github.advance(10000);
assert.equal(code.textContent, "");
assert.equal(link.href, undefined);
assert.equal(github.timers.size, 0);
assert.deepEqual(github.updates, ["provider-accounts-updated"]);
assert.deepEqual(github.calls.slice(1).map(({ route, method }) => [route, method]),
  Array(3).fill(["/api/oauth/github/poll", "POST"]));
assert.deepEqual(github.calls.slice(1).map(row => row.data), Array(3).fill({
  deviceCode: deviceCode.device_code, codeVerifier: deviceCode.codeVerifier,
}));

const cancel = rig("github", true, () => deviceCode);
await cancel.button("Generate GitHub device code").fire("click");
await cancel.button("Cancel").fire("click");
assert.equal(cancel.timers.size, 0);
assert.equal(cancel.calls.length, 1);
let completeStart;
const late = rig("github", true, () => new Promise((resolve) => { completeStart = resolve; }));
await late.button("Generate GitHub device code").fire("click");
await late.button("Cancel").fire("click");
completeStart(deviceCode);
await settle();
assert.equal(late.timers.size, 0);
assert.equal(late.controls.querySelectorAll("a")[0].href, undefined);
const expired = rig("github", true, () => ({ ...deviceCode, expires_in: 2 }));
await expired.button("Generate GitHub device code").fire("click");
await expired.advance(2000);
assert.match(expired.output.textContent, /expired/);
assert.equal(expired.calls.length, 1);
const badUrl = rig("github", true, () => ({ ...deviceCode, verification_uri: "https://github.com.invalid/login/device" }));
await badUrl.button("Generate GitHub device code").fire("click");
assert.equal(badUrl.timers.size, 0);
assert.equal(badUrl.controls.querySelectorAll("a")[0].href, undefined);
assert.equal(badUrl.output.dataset.error, "true");

let denied = true;
const gitlab = rig("gitlab", false, (_route, body) => {
  if (denied) throw new Error(`Rejected ${body.token}`);
  return { success: true };
});
assert.equal(gitlab.field("token").type, "password");
gitlab.field("token").value = "offline-fixture-pat";
gitlab.field("baseUrl").value = "https://gitlab.fixture.invalid";
await gitlab.form().fire("submit");
assert.equal(gitlab.output.textContent, "Rejected [redacted]");
assert.equal(gitlab.field("token").value, "offline-fixture-pat");
assert.equal(gitlab.updates.length, 0);
denied = false;
await gitlab.form().fire("submit");
assert.equal(gitlab.field("token").value, "");
assert.deepEqual(gitlab.calls.at(-1), {
  route: "/api/oauth/gitlab/pat", method: "POST",
  data: { baseUrl: "https://gitlab.fixture.invalid", token: "offline-fixture-pat" },
});
assert.deepEqual(gitlab.updates, ["provider-accounts-updated"]);

const grok = rig("grok-cli", false, () => ({
  total: 2, success: 1, failed: 1,
  results: [{ index: 0, ok: true }, { index: 1, ok: false, error: "Rejected offline-grok-2" }],
}));
grok.field("accounts").value = '{"access_token":"offline-grok-1"}\n{"access_token":"offline-grok-2"}';
await grok.form().fire("submit");
assert.match(grok.output.textContent, /Imported 1 of 2 accounts.*Account 2.*\[redacted\]/);
assert.equal(grok.field("accounts").value, "");
assert.deepEqual(grok.calls[0].data, { accounts: [
  { access_token: "offline-grok-1" }, { access_token: "offline-grok-2" },
] });
const file = grok.controls.querySelectorAll("input").find(node => node.type === "file");
file.files = [{ size: 100, text: async () => '{"accounts":[{"accessToken":"offline-file-grok"}]}' }];
await file.fire("change");
assert.deepEqual(JSON.parse(grok.field("accounts").value), [{ accessToken: "offline-file-grok" }]);

const zed = rig("zed", false, () => ({ success: true }));
zed.field("userId").value = "42";
zed.field("accessToken").value = "offline-zed";
zed.field("systemId").value = "fixture-system";
await zed.form().fire("submit");
assert.deepEqual(zed.calls[0], { route: "/api/oauth/zed/import", method: "POST",
  data: { userId: "42", accessToken: "offline-zed", systemId: "fixture-system" } });
assert.equal(zed.field("accessToken").value, "");
const localZed = rig("zed", true, () => ({
  found: true, accessToken: "offline-local-zed", userId: 43, systemId: "fixture-local-system",
}));
await localZed.button("Detect local credentials").fire("click");
assert.equal(localZed.calls[0].route, "/api/oauth/zed/auto-import");
assert.equal(localZed.field("userId").value, "43");
assert.equal(localZed.field("accessToken").value, "offline-local-zed");

const mimo = rig("xiaomi-mimo", true, (route) => route.endsWith("auto-import")
  ? { found: true, apiKey: "sk-offline-mimo", uid: 7, baseUrl: "https://mimo.fixture.invalid/v1" }
  : { success: true, validated: true });
await mimo.button("Detect local credentials").fire("click");
assert.equal(mimo.calls.length, 1);
assert.equal(mimo.field("apiKey").type, "password");
assert.equal(mimo.field("uid").value, "7");
await mimo.form().fire("submit");
assert.equal(mimo.calls[1].route, "/api/oauth/xiaomi-mimo/api-key");
assert.equal(mimo.calls[1].data.apiKey, "sk-offline-mimo");
assert.equal(mimo.field("apiKey").value, "");
mimo.field("mimoPassToken").value = "offline-mimo-session";
mimo.field("region").value = "cn";
await mimo.form().fire("submit");
assert.equal(mimo.calls[2].data.mimoPassToken, "offline-mimo-session");
assert.equal(mimo.calls[2].data.apiKey, "");

const command = rig("commandcode", true, (route) => route.endsWith("auto-import")
  ? { found: true, apiKey: "offline-command-key", userName: "Fixture user" }
  : { connection: { id: "fixture-command" } });
await command.button("Detect local credentials").fire("click");
assert.equal(command.field("apiKey").value, "offline-command-key");
assert.equal(command.updates.length, 0);
await command.form().fire("submit");
assert.deepEqual(command.calls[1], { route: "/api/providers", method: "POST",
  data: { provider: "commandcode", apiKey: "offline-command-key", name: "Fixture user" } });

const kiro = rig("kiro", true, (route) => route.endsWith("auto-import")
  ? { found: true, refreshToken: "offline-kiro-refresh", clientId: "fixture-id", clientSecret: "offline-kiro-secret" }
  : { success: true });
await kiro.button("Detect local credentials").fire("click");
assert.equal(kiro.field("clientSecret").type, "password");
await kiro.form().fire("submit");
assert.deepEqual(kiro.calls[1], { route: "/api/oauth/kiro/import", method: "POST",
  data: { refreshToken: "offline-kiro-refresh", clientId: "fixture-id", clientSecret: "offline-kiro-secret",
          region: "us-east-1", profileArn: "" } });
assert.equal(kiro.field("refreshToken").value, "");
kiro.field("apiKey").value = "offline-kiro-api";
await kiro.form(1).fire("submit");
assert.deepEqual(kiro.calls[2], { route: "/api/oauth/kiro/api-key", method: "POST",
  data: { apiKey: "offline-kiro-api", region: "us-east-1" } });
const cliProxyAuth = JSON.stringify({ refresh_token: "synthetic-kiro-refresh" });
kiro.field("cliProxyAuth").value = cliProxyAuth;
await kiro.form(2).fire("submit");
assert.deepEqual(kiro.calls[3], { route: "/api/oauth/kiro/import-cli-proxy", method: "POST",
  data: { cliProxyAuth } });
assert.equal(kiro.field("cliProxyAuth").value, "");

const social = rig("kiro", true, (route) => route.includes("social-authorize")
  ? { authUrl: "https://prod.us-east-1.auth.desktop.kiro.dev/login?state=synthetic",
      provider: "google", state: "synthetic", codeVerifier: "offline-kiro-verifier" }
  : { success: true });
await social.button("Generate Kiro sign-in link").fire("click");
const socialLink = social.controls.descendants().find(node => node.id === "kiro-social-link");
assert.equal(socialLink.hidden, false);
social.field("callbackUrl").value = "kiro://kiro.kiroAgent/authenticate-success?code=sample&state=wrong";
await social.form(3).fire("submit");
assert.equal(social.calls.length, 1);
social.field("callbackUrl").value = "kiro://kiro.kiroAgent/authenticate-success?code=sample&state=synthetic";
await social.form(3).fire("submit");
assert.deepEqual(social.calls[1], { route: "/api/oauth/kiro/social-exchange", method: "POST",
  data: { code: "sample", state: "synthetic", provider: "google", codeVerifier: "offline-kiro-verifier" } });
assert.equal(socialLink.hidden, true);
assert.equal(social.field("callbackUrl").value, "");

let kiroPolls = 0;
const kiroDevice = rig("kiro", true, route => {
  if (route.includes("/device-code?")) {
    const query = new URL(route, "http://localhost:20128").searchParams;
    assert.equal(query.get("auth_method"), "idc");
    assert.equal(query.get("region"), "eu-central-1");
    assert.equal(query.get("start_url"), "https://team-fixture.awsapps.com/start");
    return { device_code:"fixture-opaque-device", codeVerifier:"fixture-local-verifier", user_code:"ABCD-EFGH",
      verification_uri:"https://device.sso.eu-central-1.amazonaws.com/", interval:5, expires_in:60 };
  }
  return ++kiroPolls === 1 ? { pending:true,error:"slow_down" } : { success:true };
});
for (const [id,value] of [["kiro-device-method","idc"],["kiro-device-region","eu-central-1"],["kiro-device-start-url","https://team-fixture.awsapps.com/start"]]) {
  kiroDevice.controls.descendants().find(node => node.id === id).value = value;
}
await kiroDevice.button("Generate Kiro device code").fire("click");
assert.equal(kiroDevice.controls.descendants().find(node => node.id === "provider-device-verify").href,
  "https://device.sso.eu-central-1.amazonaws.com/");
await kiroDevice.advance(5000);
await kiroDevice.advance(10000);
assert.deepEqual(kiroDevice.calls[1], { route:"/api/oauth/kiro/poll",method:"POST",
  data:{deviceCode:"fixture-opaque-device",codeVerifier:"fixture-local-verifier"} });
assert.deepEqual(kiroDevice.updates, ["provider-accounts-updated"]);

for (const [id, name, verification, linked] of [
  ["kilocode", "Kilo Code", "https://app.kilo.ai/device"],
  ["nous", "Nous Portal", "https://portal.nousresearch.com/device"],
  ["kimi", "Kimi Code", "https://www.kimi.com/code/authorize_device"],
  ["muse", "Muse Code", "https://auth.meta.com/device"],
  ["qoder", "Qoder", "https://qoder.com/device/selectAccounts?nonce=fixture", true],
  ["qoder-cn", "Qoder China", "https://qoder.com.cn/device/selectAccounts?nonce=fixture", true],
  ["glm", "GLM Coding Plan", "https://chat.z.ai/auth/device?flow=fixture", true],
  ["codebuddy-cn", "CodeBuddy China", "https://copilot.tencent.com/authorize?state=fixture", true],
]) {
  const interval = linked ? 2 : 3;
  const codeVerifier = linked ? "fixture-proof" : undefined;
  const button = `Generate ${name} ${linked ? "sign-in link" : "device code"}`;
  const device = rig(id, false, route => route.endsWith("/device-code")
    ? { device_code: "fixture-device-handle", user_code: linked ? undefined : "TEST-CODE", verification_uri: verification,
        interval, expires_in: 60, codeVerifier }
    : { success: true });
  await device.button(button).fire("click");
  assert.equal(device.controls.descendants().find(node => node.id === "provider-device-verify").href, verification);
  assert.equal(device.controls.descendants().find(node => node.id === "provider-device-user-code").hidden, Boolean(linked));
  assert.equal(device.controls.descendants().some(node => node.id === "kiro-device-region"), false);
  await device.advance(interval * 1000);
  assert.deepEqual(device.calls, [
    { route: `/api/oauth/${id}/device-code`, method: "GET", data: undefined },
    { route: `/api/oauth/${id}/poll`, method: "POST", data: copy({ deviceCode: "fixture-device-handle", codeVerifier }) },
  ]);
  assert.deepEqual(device.updates, ["provider-accounts-updated"]);
  const origin = new URL(verification).origin;
  const foreign = rig(id, false, () => ({ device_code: "fixture-device-handle", user_code: "TEST-CODE",
    verification_uri: `${origin}.invalid${verification.slice(origin.length)}` }));
  await foreign.button(button).fire("click");
  assert.equal(foreign.output.dataset.error, "true");
  assert.equal(foreign.timers.size, 0);
}

for (const help of ["https://www.meta.ai/settings/subscription", "https://www.meta.ai.invalid/private"]) {
  const muse = rig("muse", true, route => route.endsWith("/device-code")
    ? {device_code:"fixture-meta-handle",user_code:"OFFLINE",verification_uri:"https://auth.meta.com/device",
       codeVerifier:"fixture-meta-proof",interval:5,expires_in:60}
    : {success:false,error:"exchange_failed",fatal:true,actionUrl:help});
  await muse.button("Generate Muse Code device code").fire("click");
  await muse.advance(5000);
  const link = muse.controls.descendants().find(node => node.id === "provider-device-action");
  assert.equal(link.href, new URL(help).hostname === "www.meta.ai" ? help : undefined);
  assert.equal(link.hidden, new URL(help).hostname !== "www.meta.ai");
}

const kimchiRedirect = "https://dashboard.fixture.invalid/callback";
const kimchiGrant = { state: "fixture-kimchi-state", codeVerifier: "fixture-kimchi-proof", redirectUri: kimchiRedirect };
const kimchi = rig("kimchi", false, (route, data) => {
  if (!data) {
    const query = new URL(route, kimchiRedirect).searchParams;
    assert.equal(query.get("redirect_uri"), kimchiRedirect);
    return { authUrl: "https://app.kimchi.dev/cli-auth?state=fixture-kimchi-state", ...kimchiGrant };
  }
  assert.equal(route, "/api/oauth/kimchi/exchange");
  assert.deepEqual(copy(data), { code: "fixture-private-kimchi-token", ...kimchiGrant });
  return { success: true };
}, new URL(kimchiRedirect).origin);
kimchi.field("callbackUrl").value = "fixture-private-kimchi-token";
await kimchi.form().fire("submit");
assert.deepEqual(kimchi.calls, []);
await kimchi.button("Generate sign-in link").fire("click");
kimchi.field("callbackUrl").value = "https://foreign.invalid/callback?state=fixture-kimchi-state&token=fixture-private-kimchi-token";
await kimchi.form().fire("submit");
assert.equal(kimchi.calls.length, 1);
kimchi.field("callbackUrl").value = `${kimchiRedirect}?state=${kimchiGrant.state}#token=fixture-private-kimchi-token`;
await kimchi.form().fire("submit");
assert.equal(kimchi.field("callbackUrl").value, "");
await kimchi.button("Generate sign-in link").fire("click");
kimchi.field("callbackUrl").value = "fixture-private-kimchi-token";
await kimchi.form().fire("submit");
assert.equal(kimchi.field("callbackUrl").value, "");
assert.deepEqual(kimchi.updates, ["provider-accounts-updated", "provider-accounts-updated"]);

const cursor = rig("cursor", true, (route) => route.endsWith("auto-import")
  ? { found: true, accessToken: "offline-cursor-token", machineId: "fixture-machine" }
  : { success: true });
await cursor.button("Detect local credentials").fire("click");
assert.equal(cursor.field("accessToken").value, "offline-cursor-token");
await cursor.form().fire("submit");
assert.deepEqual(cursor.calls[1], { route: "/api/oauth/cursor/import", method: "POST",
  data: { accessToken: "offline-cursor-token", machineId: "fixture-machine" } });
assert.equal(cursor.field("accessToken").value, "");
const windsurf = rig("windsurf", false, () => ({ success: true }));
windsurf.field("code").value = "Bearer sk-ws-fixture";
await windsurf.form().fire("submit");
assert.deepEqual(windsurf.calls, [{ route: "/api/oauth/windsurf/exchange", method: "POST",
  data: { code: "Bearer sk-ws-fixture" } }]);
assert.equal(windsurf.field("code").value, "");
const iflow = rig("iflow", true, () => ({ success: true }));
iflow.field("cookie").value = "BXAuth=synthetic-private-cookie";
assert.equal(iflow.field("cookie").type, "password");
await iflow.form().fire("submit");
assert.deepEqual(iflow.calls[0], { route: "/api/oauth/iflow/cookie", method: "POST",
  data: { cookie: "BXAuth=synthetic-private-cookie" } });
assert.equal(iflow.field("cookie").value, "");

for (const [provider, issuer] of [
  ["codex", "https://auth.openai.com"], ["claude", "https://claude.ai"],
  ["gemini-cli", "https://accounts.google.com"], ["cline", "https://api.cline.bot"],
  ["clinepass", "https://api.cline.bot"],
]) {
  const redirectUri = provider === "codex" ? "http://localhost:1455/auth/callback" : "http://localhost:20128/callback";
  const oauth = rig(provider, true, route => {
    if (!route.includes("/authorize?")) return { success: true };
    const actual = new URL(route, "http://localhost:20128");
    assert.equal(actual.searchParams.get("redirect_uri"), redirectUri);
    return { authUrl: issuer + "/sign-in", redirectUri, codeVerifier: "synthetic-verifier", state: "synthetic-state" };
  });
  await oauth.button("Generate sign-in link").fire("click");
  const started = provider === "codex" ? 2 : 1;
  const site = oauth.controls.descendants().find(node => node.id === "provider-authorization-link");
  assert.equal(site.href, issuer + "/sign-in");
  oauth.field("callbackUrl").value = "https://different.example.invalid/callback?code=synthetic-code&state=synthetic-state";
  await oauth.form().fire("submit");
  assert.equal(oauth.calls.length, started);
  if (provider === "clinepass") {
    const onMessage = oauth.windowEvents.get("message");
    onMessage({ origin: "https://different.example.invalid", data: {
      type: "oauth_callback", data: { code: "synthetic-code", state: "synthetic-state" },
    } });
    await settle();
    assert.equal(oauth.calls.length, started);
    onMessage({ origin: "http://localhost:20128", data: {
      type: "oauth_callback", data: { code: "synthetic-code", state: "synthetic-state" },
    } });
    await settle();
  } else {
    oauth.field("callbackUrl").value = provider === "claude" ? "synthetic-code#synthetic-state"
      : redirectUri + "?code=synthetic-code&state=synthetic-state";
    await oauth.form().fire("submit");
  }
  assert.deepEqual(oauth.calls[started], { route: `/api/oauth/${provider}/exchange`, method: "POST", data: {
    code: "synthetic-code", state: "synthetic-state", codeVerifier: "synthetic-verifier", redirectUri,
  } });
  assert.equal(site.hidden, true);
  assert.equal(oauth.field("callbackUrl").value, "");
}

let proxyPolls = 0;
const codexProxy = rig("codex", true, route => {
  if (route.includes("/authorize?")) {
    return { authUrl:"https://auth.openai.com/oauth/authorize", redirectUri:"http://localhost:1455/auth/callback",
      codeVerifier:"synthetic-proxy-verifier", state:"synthetic-proxy-state" };
  }
  if (route.includes("/start-proxy?")) return { success:true, serverSide:true };
  if (route.includes("/poll-status?")) {
    return { status: ++proxyPolls === 1 ? "pending" : "done" };
  }
  throw new Error("Unexpected Codex OAuth request");
});
await codexProxy.button("Generate sign-in link").fire("click");
await codexProxy.advance(1000);
await codexProxy.advance(1000);
assert.deepEqual(codexProxy.updates, ["provider-accounts-updated"]);
assert.equal(codexProxy.timers.size, 0);

const xaiManual = rig("xai", true, route => {
  if (route.includes("/authorize?")) {
    const uri = new URL(route, "http://localhost:20128");
    assert.equal(uri.searchParams.get("redirect_uri"), "http://127.0.0.1:56121/callback");
    return { authUrl: "https://login.x.ai/oauth/authorize", redirectUri: uri.searchParams.get("redirect_uri"),
      codeVerifier: "synthetic-xai-verifier", state: "synthetic-xai-state" };
  }
  if (route.includes("/start-proxy?")) return { success: true, serverSide: true };
  assert.equal(route, "/api/oauth/xai/manual-code");
  return { success: true };
});
await xaiManual.button("Generate sign-in link").fire("click");
xaiManual.field("callbackUrl").value = "synthetic-xai-code";
await xaiManual.form().fire("submit");
assert.deepEqual(xaiManual.calls[2], { route: "/api/oauth/xai/manual-code", method: "POST",
  data: { code: "synthetic-xai-code", state: "synthetic-xai-state" } });
assert.deepEqual(xaiManual.updates, ["provider-accounts-updated"]);
assert.equal(xaiManual.timers.size, 0);

const xaiUntrusted = rig("xai", true, () => ({ authUrl: "https://x.ai.example.invalid/oauth/authorize",
  redirectUri: "http://127.0.0.1:56121/callback", codeVerifier: "synthetic-xai-verifier", state: "synthetic-xai-state" }));
await xaiUntrusted.button("Generate sign-in link").fire("click");
assert.equal(xaiUntrusted.calls.length, 1);
assert.equal(xaiUntrusted.controls.descendants().find(node => node.id === "provider-authorization-link").hidden, true);

const gitlabOAuth = rig("gitlab", true, route => {
  if (!route.includes("/authorize?")) return { success:true };
  const params = new URL(route, "http://localhost:20128").searchParams;
  assert.equal(params.get("baseUrl"), "https://gitlab-fixture.invalid");
  assert.equal(params.get("clientId"), "fixture-application");
  assert.equal(params.has("clientSecret"), false);
  return { authUrl:"https://gitlab-fixture.invalid/oauth/authorize", redirectUri:params.get("redirect_uri"),
    codeVerifier:"synthetic-verifier", state:"synthetic-state" };
});
gitlabOAuth.field("instanceUrl").value = "https://gitlab-fixture.invalid";
gitlabOAuth.field("clientId").value = "fixture-application";
gitlabOAuth.field("clientSecret").value = "synthetic-client-secret";
await gitlabOAuth.button("Generate sign-in link").fire("click");
gitlabOAuth.field("callbackUrl").value = "http://localhost:20128/callback?code=synthetic-code&state=synthetic-state";
await gitlabOAuth.form(1).fire("submit");
assert.deepEqual(gitlabOAuth.calls[1], {
  route:"/api/oauth/gitlab/exchange", method:"POST", data:{
    code:"synthetic-code",state:"synthetic-state",codeVerifier:"synthetic-verifier",redirectUri:"http://localhost:20128/callback",
    meta:{ baseUrl:"https://gitlab-fixture.invalid", clientId:"fixture-application", clientSecret:"synthetic-client-secret" },
  },
});
assert.equal(gitlabOAuth.field("clientSecret").value, "");

const callbackScript = readFileSync(new URL("../app/src/dashboard_assets/oauth-callback.js", import.meta.url), "utf8");
const relays = [];
runInNewContext(callbackScript, {
  window: {
    location: new URL("http://localhost:20128/callback?code=synthetic-code&state=synthetic-state"),
    opener: { postMessage: (data, origin) => relays.push({ data: copy(data), origin }) },
  },
  BroadcastChannel: class {
    constructor(name) { this.name = name; }
    postMessage(data) { relays.push({ data: copy(data), origin: this.name }); }
    close() {}
  },
  document: { getElementById: () => ({ textContent: "" }) },
  URLSearchParams,
});
assert.deepEqual(relays.map(entry => entry.origin), ["http://localhost:20128", "oauth_callback"]);
assert.equal(relays[0].data.data.state, "synthetic-state");

for (const provider of ["commandcode", "cursor", "kiro", "xiaomi-mimo", "zed"]) {
  const remote = rig(provider, false, () => { throw new Error("Remote credentials were requested"); });
  assert.equal(remote.button("Detect local credentials"), undefined);
  assert.deepEqual(remote.calls, []);
}

console.log("Provider account forms passed GitHub lifecycle, all import payloads, credential clearing and local-only detection.");
