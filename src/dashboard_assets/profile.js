(() => {
  "use strict";

  const { select, request, notice, theme, setTheme } = window.nineRouterUi;
  const settingsApi = "/api/settings";
  const databaseApi = `${settingsApi}/database`;
  const status = select("management-status");
  const ssoStatus = select("profile-sso-status");
  const databaseStatus = select("profile-database-status");
  const dialogStatus = select("profile-database-dialog-status");
  const fields = {
    requireLogin: "settings-login",
    requireApiKey: "settings-api-key",
    tunnelDashboardAccess: "settings-tunnel",
    enableObservability: "settings-observability",
  };
  const routing = [
    { toggle: select("routing-accounts"), group: select("routing-account-limit-fields"),
      input: select("routing-account-limit"), mode: "fallbackStrategy", off: "fill-first",
      limit: "stickyRoundRobinLimit", defaultLimit: 3, label: "Account" },
    { toggle: select("routing-combos"), group: select("routing-combo-limit-fields"),
      input: select("routing-combo-limit"), mode: "comboStrategy", off: "fallback",
      limit: "comboStickyRoundRobinLimit", defaultLimit: 1, label: "Combo" },
  ];
  const field = id => select(id).value.trim();
  const currentPassword = select("profile-current-password");
  const newPassword = select("profile-new-password");
  const confirmPassword = select("profile-confirm-password");
  const outboundUrl = select("outbound-url");
  const themeChoice = select("theme-choice");
  const ssoFields = select("profile-sso-fields");
  const ssoType = select("profile-sso-type");
  const authMode = select("profile-auth-mode");
  const oidcFields = select("profile-oidc-fields");
  const samlFields = select("profile-saml-fields");
  const oidcSecret = select("profile-oidc-secret");
  const ssoTest = select("profile-sso-test");
  const backup = select("profile-backup-file");
  const importButton = select("profile-import");
  const dialog = select("profile-database-dialog");
  const dialogPassword = select("profile-database-password");
  const dialogCancel = select("profile-database-cancel");
  const dialogConfirm = select("profile-database-confirm");
  const shutdown = select("profile-shutdown");
  const shutdownDialog = select("profile-shutdown-dialog");
  const shutdownStatus = select("profile-shutdown-status");
  const shutdownCancel = select("profile-shutdown-cancel");
  const shutdownConfirm = select("profile-shutdown-confirm");
  const defaults = {
    oidcScopes: "openid profile email",
    oidcLoginLabel: "Sign in with OIDC",
    samlIssuer: "urn:9router:sp",
    samlLoginLabel: "Sign in with SAML SSO",
    samlAttributeEmail: "email",
    samlAttributeName: "name",
  };
  let oidcConfigured = false;
  let databaseAction = null;
  let routingSettings = {};

  function fail(error, output, message) {
    if (error.status === 401 && error.message === "Unauthorized") {
      window.location.replace("/login");
    } else {
      notice(output, message || error.message, true);
    }
  }

  async function task(control, output, work, message) {
    if (control.disabled) return;
    control.disabled = true;
    notice(output);
    try {
      await work();
    } catch (error) {
      fail(error, output, message);
    } finally {
      control.disabled = false;
    }
  }

  function onForm(id, handler) {
    const form = select(id);
    const button = form.querySelector('[type="submit"]');
    form.addEventListener("submit", event => {
      event.preventDefault();
      void task(button, status, handler);
    });
  }

  function showPassword(hasPassword) {
    select("profile-current-fields").hidden = !hasPassword;
    currentPassword.required = hasPassword;
  }

  function showRoutingLimit({ toggle, group, input }) {
    group.hidden = input.disabled = !toggle.checked;
  }

  function showRouting(settings) {
    routingSettings = settings;
    for (const entry of routing) {
      entry.toggle.checked = settings[entry.mode] === "round-robin";
      entry.input.value = String(settings[entry.limit] || entry.defaultLimit);
      showRoutingLimit(entry);
    }
  }

  function showProtocol() {
    const saml = ssoType.value === "saml";
    oidcFields.hidden = oidcFields.disabled = saml;
    samlFields.hidden = samlFields.disabled = !saml;
    ssoTest.textContent = saml ? "Test SAML configuration" : "Save and test OIDC configuration";
    showSamlAudience();
  }

  function showSamlAudience() {
    select("profile-saml-audience").textContent = field("profile-saml-issuer") || defaults.samlIssuer;
  }

  function secretState(settings) {
    oidcConfigured = settings.oidcConfigured === true;
    oidcSecret.value = "";
    oidcSecret.placeholder = oidcConfigured ? "Leave blank to keep the stored secret" : "Enter client secret";
  }

  function showSso(settings) {
    const mode = settings.authMode;
    authMode.value = mode === "both" ? mode : ["sso", "oidc", "saml"].includes(mode) ? "sso" : "password";
    ssoType.value = (settings.ssoType || (mode === "saml" ? "saml" : "oidc")) === "saml" ? "saml" : "oidc";
    for (const [key, id] of Object.entries({
      oidcIssuerUrl: "profile-oidc-issuer",
      oidcClientId: "profile-oidc-client",
      oidcScopes: "profile-oidc-scopes",
      oidcLoginLabel: "profile-oidc-label",
      samlEntryPoint: "profile-saml-entry",
      samlIssuer: "profile-saml-issuer",
      samlCert: "profile-saml-cert",
      samlAttributeEmail: "profile-saml-email",
      samlAttributeName: "profile-saml-name",
      samlLoginLabel: "profile-saml-label",
    })) select(id).value = typeof settings[key] === "string" && settings[key] ? settings[key] : defaults[key] || "";
    secretState(settings);
    showProtocol();
  }

  async function load() {
    const [settings, profile] = await Promise.all([request(settingsApi), request("/api/auth/status")]);
    for (const [name, id] of Object.entries(fields)) select(id).checked = settings[name] === true;
    select("outbound-enabled").checked = settings.outboundProxyEnabled === true;
    select("outbound-exclusions").value = settings.outboundNoProxy || "";
    outboundUrl.placeholder = settings.outboundProxyUrl ? "Leave blank to keep the stored URL" : "http://proxy:8080";
    select("profile-user").textContent = profile.authenticated ? `${profile.displayName} · ${profile.loginMethod}` : "";
    showPassword(settings.hasPassword === true);
    showSso(settings);
    showRouting(settings);
    for (const id of ["router-settings-fields", "profile-password-fields", "outbound-fields", "profile-sso-fields", "routing-fields"]) {
      select(id).disabled = false;
    }
    select("profile-database-fields").disabled = profile.authenticated !== true;
    select("profile-database-sign-in").hidden = profile.authenticated === true;
    shutdown.hidden = profile.authenticated !== true;
  }

  function validWebUrl(value) {
    try {
      const url = new URL(value);
      return (url.protocol === "https:" || url.protocol === "http:") && !url.username && !url.password;
    } catch {
      return false;
    }
  }

  function oidcDraft() {
    const secret = oidcSecret.value.trim();
    return {
      oidcIssuerUrl: field("profile-oidc-issuer"),
      oidcClientId: field("profile-oidc-client"),
      oidcScopes: field("profile-oidc-scopes") || defaults.oidcScopes,
      oidcLoginLabel: field("profile-oidc-label") || defaults.oidcLoginLabel,
      ...(secret ? { oidcClientSecret: secret } : {}),
    };
  }

  function samlDraft() {
    return {
      samlEntryPoint: field("profile-saml-entry"),
      samlIssuer: field("profile-saml-issuer") || defaults.samlIssuer,
      samlCert: field("profile-saml-cert"),
      samlLoginLabel: field("profile-saml-label") || defaults.samlLoginLabel,
      samlAttributeEmail: field("profile-saml-email") || defaults.samlAttributeEmail,
      samlAttributeName: field("profile-saml-name") || defaults.samlAttributeName,
    };
  }

  function saveSso() {
    const saml = ssoType.value === "saml";
    const draft = saml ? samlDraft() : oidcDraft();
    if (authMode.value !== "password") {
      if (saml && (!validWebUrl(draft.samlEntryPoint) || !draft.samlCert)) {
        notice(ssoStatus, "Enter the SAML sign-on URL and certificate before enabling SSO.", true);
        return;
      }
      if (!saml && (!validWebUrl(draft.oidcIssuerUrl) || !draft.oidcClientId ||
          (!draft.oidcClientSecret && !oidcConfigured))) {
        notice(ssoStatus, "Enter the OIDC issuer URL, client ID and client secret before enabling SSO.", true);
        return;
      }
    }
    void task(ssoFields, ssoStatus, async () => {
      const mode = authMode.value === "sso" && !saml ? "oidc" : authMode.value;
      const saved = await request(settingsApi, "PATCH", { ...draft, authMode: mode, ssoType: ssoType.value });
      secretState(saved);
      notice(ssoStatus, "Sign-in settings saved.");
    }, "Could not save sign-in settings.");
  }

  function testOidc() {
    const draft = oidcDraft();
    if (!validWebUrl(draft.oidcIssuerUrl) || !draft.oidcClientId) {
      notice(ssoStatus, "Enter the OIDC issuer URL and client ID before testing.", true);
      return;
    }
    void task(ssoFields, ssoStatus, async () => {
      const saved = await request(settingsApi, "PATCH", draft);
      secretState(saved);
      const result = await request("/api/auth/oidc/test", "POST", {
        issuerUrl: draft.oidcIssuerUrl, clientId: draft.oidcClientId, scopes: draft.oidcScopes,
      });
      if (result.clientSecretValid === false) {
        notice(ssoStatus, "OIDC discovery succeeded. The token endpoint rejected the client secret.", true);
      } else if (!result.ok) {
        notice(ssoStatus, "OIDC configuration test failed.", true);
      } else {
        notice(ssoStatus, result.clientSecretValid === true
          ? "OIDC discovery succeeded and the client secret was validated."
          : "OIDC discovery succeeded. The client secret could not be verified.");
      }
    }, "Could not save or test OIDC configuration.");
  }

  function testSaml() {
    const { samlEntryPoint, samlIssuer, samlCert } = samlDraft();
    if (!validWebUrl(samlEntryPoint) || !samlCert) {
      notice(ssoStatus, "Enter the SAML sign-on URL and certificate before testing.", true);
      return;
    }
    void task(ssoFields, ssoStatus, async () => {
      const result = await request("/api/auth/saml/test", "POST", { samlEntryPoint, samlIssuer, samlCert });
      if (!result.ok) throw new Error("SAML configuration test failed.");
      notice(ssoStatus, "SAML configuration verified.");
    }, "SAML configuration test failed. Check the service URL and certificate.");
  }

  function readSamlFile(id, handler, errorMessage) {
    select(id).addEventListener("change", async event => {
      const input = event.currentTarget;
      const file = input.files[0];
      input.value = "";
      if (!file) return;
      try {
        handler(await file.text());
      } catch {
        notice(ssoStatus, errorMessage, true);
      }
    });
  }

  function loadMetadata(text) {
    if (/<!DOCTYPE\b/i.test(text)) throw new Error();
    const document = new DOMParser().parseFromString(text, "application/xml");
    if (document.getElementsByTagNameNS("*", "parsererror").length) throw new Error();
    const locations = Array.from(document.getElementsByTagNameNS("*", "SingleSignOnService"))
      .filter(node => validWebUrl(node.getAttribute("Location") || ""));
    const service = locations.find(node => node.getAttribute("Binding")?.endsWith(":HTTP-Redirect")) || locations[0];
    const certificate = document.getElementsByTagNameNS("*", "X509Certificate")[0]?.textContent.trim();
    if (!service && !certificate) throw new Error();
    if (service) select("profile-saml-entry").value = service.getAttribute("Location");
    if (certificate) select("profile-saml-cert").value = certificate;
    notice(ssoStatus, "Identity provider metadata loaded. Review the fields before saving.");
  }

  function openDatabaseAction(action) {
    const file = action === "import" ? backup.files[0] : null;
    if (action === "import" && !file) return;
    databaseAction = { action, file };
    select("profile-database-dialog-title").textContent = action === "import" ? "Import database" : "Export database";
    select("profile-database-dialog-detail").textContent = file
      ? `Import ${file.name} and replace the current database settings, connections, API keys and combos?`
      : "Enter your current password to download the database backup.";
    dialogConfirm.textContent = action === "import" ? "Replace database" : "Download backup";
    dialogPassword.value = "";
    notice(dialogStatus);
    dialog.showModal();
    dialogPassword.focus();
  }

  async function downloadBackup(password) {
    const response = await fetch(databaseApi, { cache: "no-store", headers: { "x-9r-password": password } });
    const data = await response.json();
    if (!response.ok) {
      const error = new Error(data.error === "Invalid password" || data.error === "Unauthorized"
        ? data.error : "Could not export the database.");
      error.status = response.status;
      throw error;
    }
    const url = URL.createObjectURL(new Blob([JSON.stringify(data, null, 2)], { type: "application/json" }));
    const anchor = document.createElement("a");
    anchor.href = url;
    anchor.download = `9router-backup-${new Date().toISOString().replace(/[.:]/g, "-")}.json`;
    document.body.append(anchor);
    anchor.click();
    anchor.remove();
    window.setTimeout(() => URL.revokeObjectURL(url), 0);
  }

  async function restoreBackup(file, password) {
    let backup;
    try {
      backup = JSON.parse(await file.text());
      if (!backup || !backup.settings || typeof backup.settings !== "object" || Array.isArray(backup.settings) ||
          !["providerConnections", "apiKeys", "combos"].every(name => Array.isArray(backup[name]))) throw new Error();
    } catch {
      throw new Error("Select a valid 9Router database backup.");
    }
    try {
      await request(databaseApi, "POST", { ...backup, password });
    } catch (error) {
      if (error.status === 401) throw error;
      throw new Error("Could not import the database backup.");
    }
  }

  async function confirmDatabaseAction(event) {
    event.preventDefault();
    if (!databaseAction || dialogConfirm.disabled || !dialogPassword.value) return;
    const { action, file } = databaseAction;
    const password = dialogPassword.value;
    dialogPassword.value = "";
    dialogConfirm.disabled = dialogCancel.disabled = dialogPassword.disabled = true;
    notice(dialogStatus);
    try {
      if (action === "import") await restoreBackup(file, password);
      else await downloadBackup(password);
      dialog.close();
      if (action === "import") {
        backup.value = "";
        importButton.disabled = true;
        notice(databaseStatus, "Database imported.");
        await load().catch(error => fail(error, databaseStatus, "Database imported. Reload this page to see the restored settings."));
      } else {
        notice(databaseStatus, "Database backup downloaded.");
      }
    } catch (error) {
      fail(error, dialogStatus);
    } finally {
      dialogConfirm.disabled = dialogCancel.disabled = dialogPassword.disabled = false;
    }
  }

  themeChoice.value = theme();
  themeChoice.addEventListener("change", () => {
    setTheme(themeChoice.value);
    notice(status, "Theme saved.");
  });
  onForm("router-settings-form", async () => {
    const saved = await request(settingsApi, "PATCH", Object.fromEntries(Object.entries(fields)
      .map(([name, id]) => [name, select(id).checked])));
    for (const [name, id] of Object.entries(fields)) select(id).checked = saved[name] === true;
    notice(status, "Access settings saved.");
  });
  for (const entry of routing) entry.toggle.addEventListener("change", () => showRoutingLimit(entry));
  onForm("routing-form", async () => {
    const changes = {};
    for (const { toggle, input, mode, off, limit, defaultLimit, label } of routing) {
      if (toggle.checked !== (routingSettings[mode] === "round-robin")) {
        changes[mode] = toggle.checked ? "round-robin" : off;
      }
      if (toggle.checked && input.value !== String(routingSettings[limit] || defaultLimit)) {
        const calls = Number.parseInt(input.value, 10);
        if (!Number.isSafeInteger(calls) || calls < 1) throw new Error(`${label} sticky limit must be at least 1.`);
        changes[limit] = calls;
      }
    }
    if (!Object.keys(changes).length) return;
    showRouting(await request(settingsApi, "PATCH", changes));
    notice(status, "Routing settings saved.");
  });
  onForm("profile-password-form", async () => {
    if (newPassword.value !== confirmPassword.value) throw new Error("Passwords do not match.");
    await request(settingsApi, "PATCH", { currentPassword: currentPassword.value, newPassword: newPassword.value });
    select("profile-password-form").reset();
    showPassword(true);
    notice(status, "Password updated.");
  });
  onForm("outbound-form", async () => {
    const proxyUrl = field("outbound-url");
    const body = { outboundProxyEnabled: select("outbound-enabled").checked, outboundNoProxy: field("outbound-exclusions") };
    if (proxyUrl) body.outboundProxyUrl = proxyUrl;
    try {
      await request(settingsApi, "PATCH", body);
    } catch (error) {
      if (error.status === 401) throw error;
      throw new Error("Could not save outbound proxy settings.");
    }
    outboundUrl.value = "";
    if (proxyUrl) outboundUrl.placeholder = "Leave blank to keep the stored URL";
    notice(status, "Proxy settings saved.");
  });
  select("outbound-test").addEventListener("click", event => {
    void task(event.currentTarget, status, async () => {
      const proxyUrl = field("outbound-url");
      if (!proxyUrl) throw new Error("Enter a proxy URL to test.");
      const result = await request("/api/settings/proxy-test", "POST", { proxyUrl });
      notice(status, `Proxy test passed (${result.status}) in ${result.elapsedMs} ms.`);
    });
  });
  shutdown.addEventListener("click", () => {
    if (shutdown.hidden || shutdown.disabled || shutdownDialog.open) return;
    notice(shutdownStatus);
    shutdownDialog.showModal();
  });
  select("profile-shutdown-form").addEventListener("submit", event => {
    event.preventDefault();
    if (shutdown.hidden || shutdown.disabled || !shutdownDialog.open) return;
    void task(shutdownConfirm, shutdownStatus, async () => {
      shutdownCancel.disabled = true;
      try {
        const result = await request("/api/version/shutdown", "POST");
        if (result.success !== true) throw new Error("Could not shut down 9Router.");
        shutdown.disabled = true;
        shutdownDialog.close();
        notice(status, "Shutdown requested.");
      } finally {
        shutdownCancel.disabled = false;
      }
    });
  });
  shutdownCancel.addEventListener("click", () => {
    if (!shutdownConfirm.disabled) shutdownDialog.close();
  });
  shutdownDialog.addEventListener("cancel", event => {
    if (shutdownConfirm.disabled) event.preventDefault();
  });
  select("profile-oidc-callback").textContent = `${window.location.origin}/api/auth/oidc/callback`;
  select("profile-saml-acs").textContent = `${window.location.origin}/api/auth/saml/acs`;
  select("profile-saml-issuer").addEventListener("input", showSamlAudience);
  ssoType.addEventListener("change", () => {
    showProtocol();
    notice(ssoStatus);
  });
  select("profile-sso-form").addEventListener("submit", event => {
    event.preventDefault();
    saveSso();
  });
  ssoTest.addEventListener("click", () => ssoType.value === "saml" ? testSaml() : testOidc());
  readSamlFile("profile-saml-metadata", loadMetadata, "Select valid identity provider metadata XML.");
  readSamlFile("profile-saml-cert-file", text => {
    if (!text.trim()) throw new Error();
    select("profile-saml-cert").value = text.trim();
    notice(ssoStatus, "Certificate loaded. Save sign-in settings to apply it.");
  }, "Could not read the certificate file.");
  backup.addEventListener("change", () => importButton.disabled = !backup.files.length);
  select("profile-export").addEventListener("click", () => openDatabaseAction("export"));
  importButton.addEventListener("click", () => openDatabaseAction("import"));
  select("profile-database-form").addEventListener("submit", event => void confirmDatabaseAction(event));
  dialogCancel.addEventListener("click", () => dialog.close());
  dialog.addEventListener("cancel", event => { if (dialogCancel.disabled) event.preventDefault(); });
  dialog.addEventListener("close", () => {
    databaseAction = null;
    dialogPassword.value = "";
  });

  void load().catch(error => fail(error, status, "Could not load Profile settings."));
})();
