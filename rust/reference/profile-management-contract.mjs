import assert from "node:assert/strict";
import { DOMParser } from "@xmldom/xmldom";
import vm from "node:vm";

const [origin, cookie, issuer, password] = process.argv.slice(2);
const requests = [];
const messages = [];
const downloads = [];
const objects = new Map();
const redirects = [];
const initial = "offline-profile-secret";
const rejected = "offline-rejected-secret";

function localFetch(path, options = {}) {
  assert(path.startsWith("/") && !path.startsWith("//"));
  requests.push({ path, method: options.method || "GET", headers: options.headers || {},
    body: options.body ? JSON.parse(options.body) : undefined });
  return fetch(origin + path, { ...options, headers: { cookie, ...options.headers }, redirect: "manual" });
}

async function request(path, method = "GET", body) {
  const response = await localFetch(path, {
    method, cache: "no-store",
    ...(body === undefined ? {} : { headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) }),
  });
  const data = await response.json();
  if (!response.ok) {
    const error = new Error(data.error || `Request failed (${response.status})`);
    error.status = response.status;
    throw error;
  }
  return data;
}

class Control {
  constructor(id) {
    this.id = id;
    this.value = "";
    this.textContent = "";
    this.hidden = false;
    this.disabled = id.endsWith("-fields") || id === "profile-import";
    this.open = false;
    this.files = [];
    this.listeners = new Map();
  }

  addEventListener(type, callback) { this.listeners.set(type, callback); }
  emit(type) { return this.listeners.get(type)?.({ currentTarget: this, preventDefault() {} }); }
  querySelector() { return new Control("submit"); }
  showModal() { this.open = true; }
  close() { this.open = false; this.emit("close"); }
  focus() {}
  reset() {}
  remove() {}
  click() { downloads.push({ blob: objects.get(this.href), filename: this.download }); }
}

const response = await localFetch("/dashboard/profile");
assert.equal(response.status, 200);
assert.equal(response.headers.get("cache-control"), "no-store");
const html = await response.text();
assert(html.includes('/__9router-ui/profile.js') && !html.includes('/dashboard/profile?panel=legacy'));
assert(!html.includes(initial));
const fields = new Map(Array.from(html.matchAll(/\bid="([^"]+)"/g)).map(match => [match[1], new Control(match[1])]));
for (const provider of ["AWS IAM Identity Center", "Microsoft Entra ID", "Okta / Auth0", "Keycloak / Authentik"]) {
  assert(html.includes(provider));
}
assert.match(html, /href="\/api\/auth\/saml\/metadata" download="9router-sp-metadata\.xml"/);
const select = id => {
  assert(fields.has(id), `Missing control ${id}`);
  return fields.get(id);
};
const script = await localFetch("/__9router-ui/profile.js");
assert.equal(script.status, 200);
assert(script.headers.get("content-type").startsWith("text/javascript"));
const code = await script.text();
assert(!code.includes(initial));

class FixtureURL extends URL {
  static createObjectURL(blob) {
    const url = `blob:fixture-${objects.size}`;
    objects.set(url, blob);
    return url;
  }
  static revokeObjectURL(url) { objects.delete(url); }
}

const context = {
  Blob, URL: FixtureURL, DOMParser,
  fetch: localFetch,
  document: { body: { append() {} }, createElement: () => new Control("anchor") },
  window: {
    location: { origin, replace: path => redirects.push(path) },
    setTimeout,
    nineRouterUi: {
      select, request, theme: () => "system", setTheme() {},
      notice(output, message = "", failed = false) {
        output.textContent = message;
        messages.push({ id: output.id, message, failed });
      },
    },
  },
};
vm.runInNewContext(code, context, { filename: "profile.js" });

async function until(predicate) {
  for (let attempt = 0; attempt < 250; attempt++) {
    if (predicate()) return;
    await new Promise(resolve => setTimeout(resolve, 20));
  }
  throw new Error("Profile UI event did not complete");
}

const value = (id, value) => select(id).value = value;
const emit = (id, event) => select(id).emit(event);
const lastMessage = id => messages.findLast(message => message.id === id && message.message);
const ssoStatus = "profile-sso-status";
const awaitSso = () => until(() => !select("profile-sso-fields").disabled);
const profileForm = "profile-sso-form";

await awaitSso();
assert.equal(select("profile-sso-type").value, "saml");
assert.equal(select("profile-auth-mode").value, "both");
assert.equal(select("profile-oidc-secret").value, "");
assert.match(select("profile-oidc-secret").placeholder, /keep the stored secret/);
assert.equal(select("profile-oidc-callback").textContent, `${origin}/api/auth/oidc/callback`);
assert.equal(select("profile-saml-acs").textContent, `${origin}/api/auth/saml/acs`);
assert.equal(select("profile-saml-audience").textContent, "urn:fixture:sp");
value("profile-saml-issuer", " urn:fixture:draft ");
emit("profile-saml-issuer", "input");
assert.equal(select("profile-saml-audience").textContent, "urn:fixture:draft");
value("profile-saml-issuer", "");
emit("profile-saml-issuer", "input");
assert.equal(select("profile-saml-audience").textContent, "urn:9router:sp");
value("profile-saml-issuer", "urn:fixture:sp");
emit("profile-saml-issuer", "input");
assert(!select("profile-database-fields").disabled);

value("profile-sso-type", "oidc");
emit("profile-sso-type", "change");
assert.match(select("profile-sso-test").textContent, /Save and test OIDC/);
value("profile-oidc-issuer", issuer);
value("profile-oidc-client", "fixture-client");
value("profile-oidc-label", "Verified OIDC label");
let before = requests.length;
emit("profile-sso-test", "click");
await awaitSso();
let calls = requests.slice(before);
assert.deepEqual(calls.map(call => [call.path, call.method]), [["/api/settings", "PATCH"], ["/api/auth/oidc/test", "POST"]]);
assert(!("oidcClientSecret" in calls[0].body) && !("authMode" in calls[0].body) && !("ssoType" in calls[0].body));
assert(!("clientSecret" in calls[1].body));
assert.match(lastMessage(ssoStatus).message, /secret was validated/);
let settings = await request("/api/settings");
assert.equal(settings.authMode, "both");
assert.equal(settings.ssoType, "saml");
assert.equal(settings.oidcLoginLabel, "Verified OIDC label");
assert(!("oidcClientSecret" in settings));

value("profile-oidc-secret", rejected);
before = requests.length;
emit("profile-sso-test", "click");
await awaitSso();
calls = requests.slice(before);
assert.equal(calls[0].body.oidcClientSecret, rejected);
assert(!("clientSecret" in calls[1].body));
assert.equal(select("profile-oidc-secret").value, "");
assert(lastMessage(ssoStatus).failed && /rejected the client secret/.test(lastMessage(ssoStatus).message));
assert(!messages.some(({ message }) => message.includes(initial) || message.includes(rejected)));

value("profile-oidc-secret", initial);
value("profile-auth-mode", "sso");
emit(profileForm, "submit");
await awaitSso();
settings = await request("/api/settings");
assert.equal(settings.authMode, "oidc");
assert.equal(settings.ssoType, "oidc");
assert(!("oidcClientSecret" in settings));
const start = await localFetch("/api/auth/oidc/start");
assert.equal(start.status, 307);
assert.equal(start.headers.get("location").split("?", 1)[0], `${issuer}/authorize`);

value("profile-sso-type", "saml");
value("profile-auth-mode", "both");
emit("profile-sso-type", "change");
assert.equal(select("profile-sso-test").textContent, "Test SAML configuration");
const metadata = '<md:EntityDescriptor xmlns:md="urn:oasis:names:tc:SAML:2.0:metadata" xmlns:ds="http://www.w3.org/2000/09/xmldsig#" entityID="urn:fixture:idp">'
  + '<md:IDPSSODescriptor><md:SingleSignOnService Location="https://idp.example.com/post" Binding="urn:oasis:names:tc:SAML:2.0:bindings:HTTP-POST"/>'
  + '<md:SingleSignOnService Location="https://idp.example.com/redirect" Binding="urn:oasis:names:tc:SAML:2.0:bindings:HTTP-Redirect"/>'
  + '<ds:X509Certificate>QUJDRA==</ds:X509Certificate></md:IDPSSODescriptor></md:EntityDescriptor>';
select("profile-saml-metadata").files = [new File([metadata], "metadata.xml")];
await emit("profile-saml-metadata", "change");
assert.equal(select("profile-saml-entry").value, "https://idp.example.com/redirect");
assert.equal(select("profile-saml-cert").value, "QUJDRA==");
assert.equal(select("profile-saml-issuer").value, "urn:fixture:sp");
select("profile-saml-cert-file").files = [new File(["  Q0VSVA==\n"], "certificate.pem")];
await emit("profile-saml-cert-file", "change");
before = requests.length;
emit("profile-sso-test", "click");
await awaitSso();
assert.deepEqual(requests.slice(before).map(call => call.path), ["/api/auth/saml/test"]);
assert.equal(requests[before].body.samlCert, "Q0VSVA==");
assert.match(lastMessage(ssoStatus).message, /verified/);
emit(profileForm, "submit");
await awaitSso();
settings = await request("/api/settings");
assert.equal(settings.authMode, "both");
assert.equal(settings.ssoType, "saml");
assert.equal(settings.samlEntryPoint, "https://idp.example.com/redirect");
assert.equal(settings.samlCert, "Q0VSVA==");
const sp = await localFetch("/api/auth/saml/metadata");
assert.equal(sp.status, 200);
assert.match(await sp.text(), /urn:fixture:sp/);

const databaseForm = "profile-database-form";
const databasePassword = "profile-database-password";
const databaseConfirm = "profile-database-confirm";
const databaseDialog = "profile-database-dialog";
const dialogStatus = "profile-database-dialog-status";
const databaseRequests = () => requests.filter(call => call.path === "/api/settings/database");
let count = databaseRequests().length;
emit("profile-export", "click");
assert(select(databaseDialog).open);
assert.equal(databaseRequests().length, count);
value(databasePassword, "wrong-offline-password");
emit(databaseForm, "submit");
await until(() => !select(databaseConfirm).disabled);
assert.equal(lastMessage(dialogStatus).message, "Invalid password");
assert.equal(downloads.length, 0);
assert.equal(select(databasePassword).value, "");
value(databasePassword, password);
emit(databaseForm, "submit");
await until(() => downloads.length === 1 && !select(databaseConfirm).disabled);
assert(!select(databaseDialog).open);
assert.match(downloads[0].filename, /^9router-backup-.+\.json$/);
const backup = JSON.parse(await downloads[0].blob.text());
assert.equal(backup.settings.oidcClientSecret, initial);
assert.equal(databaseRequests().at(-1).headers["x-9r-password"], password);

function selectFile(file) {
  select("profile-backup-file").files = [file];
  emit("profile-backup-file", "change");
}
count = databaseRequests().length;
selectFile(new File(["{}"], "invalid.json"));
assert.equal(databaseRequests().length, count);
emit("profile-import", "click");
assert(select(databaseDialog).open && /replace the current database/.test(select("profile-database-dialog-detail").textContent));
value(databasePassword, password);
emit(databaseForm, "submit");
await until(() => !select(databaseConfirm).disabled);
assert.equal(databaseRequests().length, count);
assert.match(lastMessage(dialogStatus).message, /valid 9Router database backup/);
emit("profile-database-cancel", "click");

backup.settings.requireApiKey = false;
selectFile(new File([JSON.stringify(backup)], "restore.json"));
emit("profile-import", "click");
emit("profile-database-cancel", "click");
assert.equal(databaseRequests().length, count);
emit("profile-import", "click");
value(databasePassword, password);
emit(databaseForm, "submit");
await until(() => !select(databaseConfirm).disabled && !select(databaseDialog).open);
assert.equal(databaseRequests().length, count + 1);
assert.equal(databaseRequests().at(-1).method, "POST");
assert.equal(select("settings-api-key").checked, false);
assert.equal(select("profile-oidc-secret").value, "");
assert(!messages.some(({ message }) => message.includes(initial) || message.includes(rejected) || message.includes(password)));
assert.equal(redirects.length, 0);
console.log("Profile VM exercised both SSO protocols, local metadata and certificate uploads, secret omission, and password-confirmed backup actions against the native gateway.");
