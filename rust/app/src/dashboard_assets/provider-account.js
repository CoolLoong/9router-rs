(() => {
  "use strict";

  const { select, request, notice, element, action } = window.nineRouterUi;
  const section = select("provider-account");
  if (!section) return;
  const { provider, local } = section.dataset;
  const output = select("provider-account-status");
  const controls = select("provider-account-forms");
  let sequence = 0;

  const secretFields = new Set([
    "token", "apiKey", "accessToken", "access_token", "refreshToken", "refresh_token",
    "idToken", "id_token", "clientSecret", "mimoPassToken", "deviceCode", "codeVerifier",
    "cliProxyAuth", "callbackUrl", "cookie", "code",
  ]);

  function secretValues(payload) {
    return (Array.isArray(payload) ? payload : [payload]).flatMap((entry) =>
      [...secretFields].map((name) => entry?.[name]).filter((value) => typeof value === "string" && value));
  }

  function message(error, payload) {
    const text = typeof error?.message === "string" ? error.message : "The request failed.";
    const secrets = [...new Set(secretValues(payload))].sort((left, right) => right.length - left.length);
    return secrets.reduce((clean, value) => clean.split(value).join("[redacted]"), text);
  }

  function updated() {
    document.dispatchEvent(new Event("provider-accounts-updated"));
  }

  function form(title, specifications, submitLabel, submit) {
    const panel = element("form", null, "settings-form");
    panel.method = "post";
    panel.autocomplete = "off";
    const heading = element("h3", title);
    heading.id = `provider-account-form-${++sequence}`;
    panel.setAttribute("aria-labelledby", heading.id);
    panel.append(heading);
    const fields = {};
    for (const [name, label, type, required, value = ""] of specifications) {
      const control = element(type === "textarea" ? "textarea" : "input");
      control.id = `${heading.id}-${name}`;
      control.name = name;
      control.required = Boolean(required);
      control.spellcheck = false;
      if (type !== "textarea") control.type = type;
      else control.rows = 6;
      control.autocomplete = type === "password" ? "new-password" : "off";
      control.defaultValue = value;
      control.value = value;
      const caption = element("label", label);
      caption.htmlFor = control.id;
      panel.append(caption, control);
      fields[name] = control;
    }
    const buttons = element("div", null, "key-actions");
    const save = element("button", submitLabel, "button button-primary");
    save.type = "submit";
    buttons.append(save);
    panel.append(buttons);
    let busy = false;
    const run = async (work) => {
      if (busy) return;
      busy = true;
      notice(output);
      for (const button of buttons.querySelectorAll("button")) button.disabled = true;
      for (const field of Object.values(fields)) field.readOnly = true;
      try {
        await work();
      } catch (error) {
        notice(output, message(error, values()), true);
      } finally {
        for (const button of buttons.querySelectorAll("button")) button.disabled = false;
        for (const field of Object.values(fields)) field.readOnly = false;
        busy = false;
      }
    };
    const values = () => Object.fromEntries(Object.entries(fields).map(([name, field]) => [name, field.value.trim()]));
    panel.addEventListener("submit", (event) => {
      event.preventDefault();
      void run(async () => {
        const changed = await submit(values());
        panel.reset();
        notice(output, typeof changed === "string" ? changed : "Connection saved.");
        updated();
      });
    });
    controls.append(panel);
    return { panel, fields, buttons, save, run };
  }

  function detect(editor, endpoint, fill) {
    if (local !== "true") return;
    editor.buttons.append(action("Detect local credentials", () => void editor.run(async () => {
      const result = await request(endpoint);
      if (result.found !== true || !fill(result)) {
        throw new Error("Local credentials could not be detected.");
      }
      notice(output, "Local credentials loaded. Review and save the connection.");
    })));
  }

  function credential(providerId, title, fields, actionName, validate) {
    const endpoint = `/api/oauth/${providerId}/${actionName}`;
    return form(title, fields, "Save connection", async (body) => {
      validate?.(body);
      const result = await request(endpoint, "POST", body);
      if (result.success !== true) throw new Error("The connection could not be saved.");
      return "Connection saved.";
    });
  }

  function fillFields(editor, data, names) {
    for (const name of names) {
      const value = data[name];
      editor.fields[name].value = typeof value === "string" || typeof value === "number" ? String(value) : "";
    }
  }

  function miniMaxDesktop(providerId) {
    const endpoint = `/api/oauth/${providerId}`;
    const editor = form("MiniMax Desktop account", [["dataDir", "Desktop data directory", "text", false]],
      "Connect desktop account", async (body) => {
        const result = await request(`${endpoint}/lease-connect`, "POST", body);
        if (result.success !== true) throw new Error("The desktop account could not be connected.");
        return result.reused ? "Desktop account already connected." : "Desktop account connected.";
      });
    editor.buttons.append(action("Check desktop account", () => void editor.run(async () => {
      const path = editor.fields.dataDir.value.trim();
      const query = path ? `?${new URLSearchParams({data_dir: path})}` : "";
      const status = await request(`${endpoint}/lease-status${query}`);
      if (!status.found) throw new Error(status.message || "MiniMax Desktop is unavailable.");
      notice(output, status.status ? `MiniMax Desktop status: ${status.status}` : "MiniMax Desktop is available.");
    })));
  }

  function browserLogin(providerId) {
    const endpoint = "/api/providers/web-login";
    const statusUrl = `${endpoint}?${new URLSearchParams({provider: providerId})}`;
    const fields = element("div", null, "settings-form");
    const label = element("label", "Browser profile");
    const profile = element("input");
    const suggestions = element("datalist");
    profile.id = "provider-browser-profile";
    profile.value = "default";
    suggestions.id = "provider-browser-profiles";
    profile.setAttribute("list", suggestions.id);
    label.htmlFor = profile.id;
    const buttons = element("div", null, "key-actions");
    let polling;
    const start = action("Sign in with browser", async () => {
      start.disabled = true;
      profile.readOnly = true;
      try {
        const result = await request(endpoint, "POST", {provider: providerId, profile: profile.value.trim()});
        if (result.status !== "running") throw new Error("The browser sign-in could not start.");
        profile.value = result.profile;
        cancel.hidden = false;
        notice(output, "Complete sign-in in the browser window.");
        polling = setTimeout(poll, 1000);
      } catch (error) {
        finish(message(error), true);
      }
    });
    const cancel = action("Cancel", async () => {
      try {
        await request(statusUrl, "DELETE");
      } catch (error) {
        notice(output, message(error), true);
      }
    });
    cancel.hidden = true;
    buttons.append(start, cancel);
    fields.append(label, profile, suggestions, buttons);
    controls.append(fields);
    void request(endpoint).then((result) => {
      for (const name of result.profiles || []) {
        const option = element("option");
        option.value = name;
        suggestions.append(option);
      }
    }).catch(() => {});

    function finish(text, failed = false) {
      clearTimeout(polling);
      start.disabled = false;
      cancel.hidden = true;
      profile.readOnly = false;
      notice(output, text, failed);
    }

    async function poll() {
      try {
        const result = await request(statusUrl);
        if (result.status === "running") {
          polling = setTimeout(poll, 1000);
        } else if (result.status === "success") {
          finish(result.email ? `Connected ${result.email}.` : "Connection saved.");
          updated();
        } else if (result.status === "cancelled") {
          finish("Sign-in cancelled.");
        } else {
          finish(result.error || "The browser sign-in did not complete.", true);
        }
      } catch (error) {
        finish(message(error), true);
      }
    }
  }

  function deviceAuthorization(provider, name) {
    const group = element("div", null, "key-actions");
    const kiro = provider === "kiro";
    const qoder = provider === "qoder" || provider === "qoder-cn";
    const minimax = provider === "minimax-code" || provider === "minimax-code-global";
    const linkOnly = qoder || ["glm", "codebuddy-cn", "codebuddy-intl"].includes(provider);
    const muse = provider === "muse";
    const userCode = element("code");
    userCode.id = "provider-device-user-code";
    userCode.hidden = true;
    const verify = element("a", `Open ${name} device verification`, "button");
    verify.id = "provider-device-verify";
    verify.target = "_blank";
    verify.rel = "noopener noreferrer";
    verify.hidden = true;
    const stop = action("Cancel", () => end(`Stopped checking ${name} authorization.`));
    stop.hidden = true;
    const start = action(`Generate ${name} ${linkOnly ? "sign-in link" : "device code"}`, () => void begin());
    const actionUrl = muse ? element("a", "Open Meta account help", "button") : undefined;
    if (actionUrl) {
      actionUrl.hidden = true;
      actionUrl.id = "provider-device-action";
      actionUrl.target = "_blank";
      actionUrl.rel = "noopener noreferrer";
      group.append(element("p", "Completing authorization can enroll this account in Muse Code."));
    }
    let current;
    let timer;
    let method, region, startUrl;
    if (kiro) {
      method = element("select");
      method.id = "kiro-device-method";
      for (const [value, title] of [["builder-id","AWS Builder ID"],["idc","IAM Identity Center"]]) {
        const option = element("option", title);
        option.value = value;
        method.append(option);
      }
      method.value = "builder-id";
      region = element("input");
      region.id = "kiro-device-region";
      region.value = "us-east-1";
      startUrl = element("input");
      startUrl.id = "kiro-device-start-url";
      startUrl.type = "url";
      for (const [label, input] of [["Kiro sign-in method",method],["AWS region",region],["IAM Identity Center start URL",startUrl]]) {
        const caption = element("label",label);
        caption.htmlFor = input.id;
        group.append(caption,input);
      }
    }
    group.append(start, userCode, verify, stop);
    if (actionUrl) group.append(actionUrl);
    controls.append(group);

    function end(text = "", failed = false) {
      current = undefined;
      if (timer !== undefined) clearTimeout(timer);
      timer = undefined;
      userCode.textContent = "";
      userCode.hidden = verify.hidden = stop.hidden = true;
      verify.removeAttribute("href");
      if (actionUrl) {
        actionUrl.hidden = true;
        actionUrl.removeAttribute("href");
      }
      start.disabled = false;
      notice(output, text, failed);
    }

    function schedule(attempt) {
      const duration = Math.max(0, Math.min(attempt.interval, attempt.expires - Date.now()));
      timer = setTimeout(() => void poll(attempt), duration);
    }

    function validVerification(url) {
      if (url.protocol !== "https:" || url.username || url.password) return false;
      if (qoder) {
        const origin = provider === "qoder" ? "https://qoder.com" : "https://qoder.com.cn";
        return url.origin === origin && url.pathname === "/device/selectAccounts" && !url.hash;
      }
      if (minimax) {
        const domain = provider === "minimax-code" ? "minimax.cn" : "minimax.io";
        return !url.port && !url.hash && [domain, `account.${domain}`, `agent.${domain}`, `www.${domain}`].includes(url.hostname);
      }
      if (kiro) return url.hostname.endsWith(".amazonaws.com") || url.hostname.endsWith(".awsapps.com");
      if (provider === "github") return url.origin === "https://github.com" && url.pathname === "/login/device" && !url.hash;
      if (provider === "grok-cli") return !url.port && ["x.ai", "grok.com", "x.com"].some((domain) =>
        url.hostname === domain || url.hostname.endsWith(`.${domain}`));
      if (provider === "nous") return url.origin === "https://portal.nousresearch.com";
      if (provider === "kimi") return !url.port && ["kimi.com", "www.kimi.com", "auth.kimi.com"].includes(url.hostname);
      if (provider === "glm") return !url.port && (url.hostname === "z.ai" || url.hostname.endsWith(".z.ai")) && !url.hash;
      if (provider === "codebuddy-cn") return url.origin === "https://copilot.tencent.com" && !url.hash;
      if (provider === "codebuddy-intl") return !url.port && ["codebuddy.ai", "www.codebuddy.ai"].includes(url.hostname);
      if (muse) return !url.port && ["auth.meta.com", "meta.com", "www.meta.com", "accountscenter.meta.com",
        "meta.ai", "www.meta.ai", "api.meta.ai", "dev.meta.ai", "muse.ai", "www.muse.ai",
        "facebook.com", "www.facebook.com"].includes(url.hostname);
      return !url.port && (["kilo.ai","kilocode.ai"].includes(url.hostname) ||
        url.hostname.endsWith(".kilo.ai") || url.hostname.endsWith(".kilocode.ai"));
    }

    async function poll(attempt) {
      if (current !== attempt) return;
      if (Date.now() >= attempt.expires) {
        end(`The ${name} device code expired.`, true);
        return;
      }
      const body = { deviceCode: attempt.code, codeVerifier: attempt.verifier };
      try {
        const result = await request(`/api/oauth/${provider}/poll`, "POST", body);
        if (current !== attempt) return;
        if (result.success === true) {
          end(`${name} connection saved.`);
          updated();
        } else if (result.pending === true) {
          if (result.error === "slow_down") attempt.interval += 5000;
          schedule(attempt);
        } else {
          end(message(new Error(result.errorDescription || result.error || `${name} authorization failed.`), body), true);
          if (actionUrl && typeof result.actionUrl === "string") {
            const target = URL.canParse(result.actionUrl) ? new URL(result.actionUrl) : null;
            if (target && validVerification(target) && !target.search && !target.hash) {
              actionUrl.href = target.href;
              actionUrl.hidden = false;
            }
          }
        }
      } catch (error) {
        if (current === attempt) end(message(error, body), true);
      }
    }

    async function begin() {
      if (current) return;
      if (actionUrl) {
        actionUrl.hidden = true;
        actionUrl.removeAttribute("href");
      }
      const attempt = {};
      current = attempt;
      start.disabled = true;
      stop.hidden = false;
      notice(output, `Requesting a ${name} device code…`);
      try {
        let endpoint = `/api/oauth/${provider}/device-code`;
        if (kiro) {
          const query = new URLSearchParams({ auth_method: method.value, region: region.value.trim() });
          if (method.value === "idc") {
            const start = new URL(startUrl.value.trim());
            if (start.protocol !== "https:" || start.username || start.password) throw new Error("Enter a valid AWS start URL.");
            query.set("start_url", start.href);
          }
          endpoint += `?${query}`;
        }
        const result = await request(endpoint);
        if (current !== attempt) return;
        const url = new URL(result.verification_uri_complete || result.verification_uri);
        if (!validVerification(url) ||
            (!linkOnly && (typeof result.user_code !== "string" || !result.user_code)) ||
            typeof result.device_code !== "string" || !result.device_code) {
          throw new Error(`${name} returned an invalid device authorization response.`);
        }
        const seconds = (value, fallback) => Number.isFinite(value) && value > 0 ? value : fallback;
        attempt.code = result.device_code;
        attempt.verifier = result.codeVerifier;
        attempt.interval = Math.max(1, seconds(result.interval, 5)) * 1000;
        attempt.expires = Date.now() + Math.min(seconds(result.expires_in, 900), 3600) * 1000;
        userCode.textContent = linkOnly ? "" : result.user_code;
        userCode.hidden = linkOnly;
        verify.hidden = false;
        verify.href = url.href;
        notice(output, linkOnly
          ? `Open ${name} to authorize the connection.`
          : `Enter this code on ${kiro ? "AWS" : name} to authorize the connection.`);
        schedule(attempt);
      } catch (error) {
        if (current === attempt) end(message(error), true);
      }
    }

    window.addEventListener("pagehide", () => end());
  }

  function grokAccounts(input) {
    let payload;
    try {
      payload = JSON.parse(input);
    } catch {
      try { payload = JSON.parse(`[${input.replace(/\}\s*\{/g, "},{").replace(/,\s*$/, "")}]`); }
      catch { throw new Error("Enter valid account JSON."); }
    }
    const accounts = Array.isArray(payload) ? payload : Array.isArray(payload?.accounts) ? payload.accounts : [payload];
    if (!accounts.length || !accounts.some((entry) => entry && typeof entry === "object")) {
      throw new Error("Enter one or more account objects.");
    }
    return accounts;
  }

  function grok() {
    const editor = form("Import Grok CLI accounts",
      [["accounts", "Account JSON", "textarea", true]], "Import accounts", async (body) => {
        const accounts = grokAccounts(body.accounts);
        let result;
        try { result = await request("/api/oauth/grok-cli/bulk-import", "POST", { accounts }); }
        catch (error) { throw new Error(message(error, accounts)); }
        const report = `Imported ${Number(result.success) || 0} of ${Number(result.total) || accounts.length} accounts.`;
        if (!result.success) throw new Error(report);
        if (result.failed) {
          const failures = (result.results ?? []).filter((row) => row.ok === false && Number.isInteger(row.index));
          const notes = failures.map((row) => `Account ${row.index + 1} — ${message(new Error(row.error || "Import failed."), accounts)}`);
          return [report, ...notes].join(" ");
        }
        return report;
      });
    const upload = element("input");
    upload.id = "provider-grok-files";
    upload.type = "file";
    upload.accept = ".json,application/json";
    upload.multiple = true;
    const caption = element("label", "Load account JSON files");
    caption.htmlFor = upload.id;
    editor.panel.insertBefore(caption, editor.buttons);
    editor.panel.insertBefore(upload, editor.buttons);
    upload.addEventListener("change", () => void editor.run(async () => {
      const accounts = [];
      for (const file of upload.files) {
        if (file.size > 8 * 1024 * 1024) throw new Error("The account JSON file is too large.");
        accounts.push(...grokAccounts(await file.text()));
      }
      if (accounts.length) editor.fields.accounts.value = JSON.stringify(accounts, null, 2);
      upload.value = "";
    }));
  }

  function codexBulk() {
    form("Bulk add Codex accounts", [["accounts", "Account JSON", "textarea", true]],
      "Import accounts", async (body) => {
        let parsed;
        try { parsed = JSON.parse(body.accounts); }
        catch { throw new Error("Enter valid account JSON."); }
        let accounts = [];
        if (Array.isArray(parsed)) accounts = parsed;
        else if (parsed && typeof parsed === "object") accounts = Array.isArray(parsed.accounts) ? parsed.accounts : [parsed];
        if (!accounts.length) throw new Error("No accounts found in input.");
        let result;
        try { result = await request("/api/oauth/codex/bulk-import", "POST", { accounts }); }
        catch (error) { throw new Error(message(error, accounts)); }
        const imported = Number(result?.success) || 0;
        const failures = (Array.isArray(result?.results) ? result.results : [])
          .filter(row => row?.ok === false && Number.isInteger(row.index))
          .map(row => `Account ${row.index + 1} — ${message(new Error(row.error || "Import failed."), accounts)}`);
        const report = [`Imported ${imported} of ${accounts.length} accounts.`, ...failures].join(" ");
        if (!imported) throw new Error(report);
        return report;
      });
  }

  function mimo() {
    const editor = credential("xiaomi-mimo", "Import Xiaomi MiMo credentials", [
      ["apiKey", "API key", "password"], ["baseUrl", "API base URL", "url"],
      ["mimoPassToken", "Session token", "password"], ["uid", "User ID", "text"],
      ["mimoUserId", "MiMo user ID", "text"], ["mimoCUserId", "MiMo client user ID", "text"],
      ["region", "Region", "text"],
    ], "api-key", (body) => {
      if (!body.apiKey && !body.mimoPassToken) throw new Error("Enter an API key or session token.");
    });
    if (local !== "true") return;
    const detected = element("p", null, "muted");
    detected.hidden = true;
    detected.setAttribute("role", "status");
    editor.panel.insertBefore(detected, editor.buttons);
    let lookup = 0;
    function clearDetection() {
      lookup++;
      detected.hidden = true;
      editor.save.textContent = "Save connection";
    }
    editor.panel.addEventListener("reset", clearDetection);
    for (const control of Object.values(editor.fields)) control.addEventListener("input", clearDetection);
    detect(editor, "/api/oauth/xiaomi-mimo/auto-import", (data) => {
      if (typeof data.apiKey !== "string" || !data.apiKey) return false;
      clearDetection();
      editor.fields.apiKey.value = data.apiKey;
      fillFields(editor, data, ["uid", "baseUrl", "mimoPassToken", "mimoUserId", "mimoCUserId", "region"]);
      const uid = editor.fields.uid.value.trim();
      const current = lookup;
      if (uid) void request("/api/providers").then(result => {
        if (current !== lookup || editor.fields.uid.value.trim() !== uid) return;
        const connections = (result.connections ?? []).filter(row => row.provider === "xiaomi-mimo");
        const linked = connections.find(row => row.email === `${uid}@xiaomi`);
        const sameUid = connections.some(row => [row.providerSpecificData?.uid, row.providerSpecificData?.mimoUserId]
          .some(value => String(value ?? "").trim() === uid));
        if (linked) {
          editor.save.textContent = "Re-sync local credentials";
          detected.textContent = linked.testStatus === "active"
            ? "This Xiaomi MiMo account is already connected and active."
            : "This Xiaomi MiMo account is already connected.";
        } else if (sameUid) {
          detected.textContent = "A saved connection has the same Xiaomi user ID.";
        } else return;
        detected.hidden = false;
      }).catch(() => {});
      return true;
    });
    mimoAuthorization(editor);
    mimoDesktopLogin(editor);
  }

  function mimoDesktopLogin(editor) {
    const endpoint = "/api/oauth/xiaomi-mimo/login";
    const clusters = [
      ["cn", "China (Mainland)"], ["sgp", "Singapore"], ["ams", "Europe · Amsterdam"],
      ["ru", "Russia"], ["in", "India"],
    ];
    const region = element("select");
    region.id = "mimo-desktop-login-region";
    for (const [value, title] of [["", "Select a region"], ...clusters]) {
      const option = element("option", title);
      option.value = value;
      region.append(option);
    }
    region.value = "";
    const label = element("label", "MiMo Desktop sign-in region");
    label.htmlFor = region.id;
    const link = element("a", "Open MiMo Desktop sign-in", "button");
    link.target = "_blank";
    link.rel = "noopener noreferrer";
    link.hidden = true;
    let attempt;
    let timer;
    let navigation = 0;
    const start = action("Start Desktop sign-in", () => void editor.run(begin));
    const cancel = action("Cancel Desktop sign-in", () => void editor.run(async () => {
      const current = clear();
      if (!current) return;
      await stop(current.state);
      notice(output, "MiMo Desktop sign-in cancelled.");
    }));
    cancel.hidden = true;
    editor.buttons.append(label, region, start, link, cancel);

    function clear() {
      const current = attempt;
      clearTimeout(timer);
      attempt = undefined;
      link.hidden = cancel.hidden = true;
      region.disabled = start.hidden = false;
      link.removeAttribute("href");
      return current;
    }

    const stop = (state) => request(`${endpoint}/cancel`, "POST", { state });
    function poll(current) {
      timer = setTimeout(async () => {
        if (attempt !== current) return;
        try {
          if (Date.now() >= current.expires) throw new Error("MiMo Desktop sign-in expired.");
          const result = await request(`${endpoint}/status?${new URLSearchParams({ state: current.state })}`);
          if (attempt !== current) return;
          if (result.status === "pending") {
            poll(current);
            return;
          }
          if (result.status !== "done" || result.region !== current.region || typeof result.connection?.id !== "string") {
            throw new Error("MiMo Desktop sign-in did not complete.");
          }
          clear();
          notice(output, "MiMo Desktop session saved.");
          updated();
        } catch (error) {
          if (attempt !== current) return;
          clear();
          void stop(current.state).catch(() => {});
          notice(output, message(error, { token: current.state }), true);
        }
      }, 2500);
    }

    async function begin() {
      if (attempt) return;
      const revision = navigation;
      const selected = region.value;
      if (!clusters.some(([id]) => id === selected)) throw new Error("Select a MiMo Desktop sign-in region.");
      region.disabled = true;
      let state;
      try {
        const data = await request(`${endpoint}/start`, "POST", { region: selected });
        state = typeof data.state === "string" && data.state;
        const page = new URL(data.pageUrl);
        if (data.success !== true || !state || data.region !== selected || page.protocol !== "http:"
          || page.hostname !== "127.0.0.2" || !page.port || page.username || page.password || page.hash) {
          throw new Error("Xiaomi MiMo did not return a valid Desktop sign-in link.");
        }
        if (revision !== navigation) {
          void stop(state).catch(() => {});
          return;
        }
        attempt = { state, region: selected, expires: Date.now() + 14 * 60 * 1000 };
        start.hidden = true;
        link.href = page.href;
        link.hidden = cancel.hidden = false;
        notice(output, "Open MiMo Desktop sign-in to connect the account.");
        poll(attempt);
      } catch (error) {
        if (state) void stop(state).catch(() => {});
        throw new Error(message(error, { token: state }));
      } finally {
        region.disabled = Boolean(attempt);
      }
    }

    window.addEventListener("pagehide", () => {
      navigation++;
      const current = clear();
      if (current) void stop(current.state).catch(() => {});
    });
  }

  function mimoAuthorization(editor) {
    const endpoint = "/api/oauth/xiaomi-mimo";
    const link = element("a", "Open Xiaomi MiMo authorization", "button");
    link.target = "_blank";
    link.rel = "noopener noreferrer";
    link.hidden = true;
    let attempt;
    let timer;
    const start = action("Generate MiMo API key link", () => void editor.run(begin));
    const cancel = action("Cancel MiMo authorization", () => void editor.run(async () => {
      if (!attempt || attempt.saving) return;
      const current = clear();
      await stop(current.state);
      notice(output, "MiMo authorization cancelled.");
    }));
    cancel.hidden = true;
    editor.buttons.append(start, link, cancel);

    function clear() {
      const current = attempt;
      clearTimeout(timer);
      attempt = undefined;
      link.hidden = cancel.hidden = true;
      start.hidden = false;
      link.removeAttribute("href");
      return current;
    }

    function stop(state) {
      return request(`${endpoint}/stop-proxy?${new URLSearchParams({ state })}`);
    }

    function poll(current) {
      timer = setTimeout(async () => {
        if (attempt !== current) return;
        try {
          if (Date.now() >= current.expires) throw new Error("MiMo authorization expired.");
          const result = await request(`${endpoint}/poll-status?${new URLSearchParams({ state: current.state })}`);
          if (attempt !== current) return;
          if (result.status === "pending") {
            poll(current);
            return;
          }
          if (result.status !== "done") throw new Error("MiMo authorization did not complete.");
          current.saving = true;
          cancel.hidden = link.hidden = true;
          const saved = await request(`${endpoint}/exchange`, "POST", { state: current.state });
          if (attempt !== current) return;
          if (saved.success !== true) throw new Error("The MiMo account could not be saved.");
          clear();
          notice(output, "Connection saved.");
          updated();
        } catch (error) {
          if (attempt !== current) return;
          clear();
          void stop(current.state).catch(() => {});
          notice(output, message(error, { token: current.state }), true);
        }
      }, 1000);
    }

    async function begin() {
      if (attempt) return;
      const data = await request(`${endpoint}/authorize`);
      const state = typeof data.state === "string" && data.state;
      try {
        const authorize = new URL(data.authorizeUrl);
        const redirect = new URL(data.redirectUri);
        if (!state || authorize.origin !== "https://platform.xiaomimimo.com" || authorize.pathname !== "/authorize"
          || authorize.username || authorize.password || authorize.hash
          || authorize.searchParams.getAll("redirect_uri").length !== 1
          || authorize.searchParams.get("redirect_uri") !== data.redirectUri
          || authorize.searchParams.get("kn") !== "mimocode" || !authorize.searchParams.get("pk")
          || redirect.protocol !== "http:" || !["localhost", "127.0.0.1"].includes(redirect.hostname)
          || redirect.pathname !== "/" || redirect.username || redirect.password || redirect.search || redirect.hash
          || !redirect.port || Number(redirect.port) !== data.port) throw new Error();
        attempt = { state, expires: Date.now() + 300000 };
        start.hidden = true;
        link.href = authorize.href;
        link.hidden = cancel.hidden = false;
        notice(output, "Open Xiaomi MiMo to authorize API key creation.");
        poll(attempt);
      } catch {
        if (state) void stop(state).catch(() => {});
        throw new Error("Xiaomi MiMo did not return a valid authorization link.");
      }
    }

    window.addEventListener("pagehide", () => {
      const current = clear();
      if (current && !current.saving) void stop(current.state).catch(() => {});
    });
  }

  function cursor() {
    const editor = credential("cursor", "Import a Cursor session", [
      ["accessToken", "Access token", "password", true], ["machineId", "Machine ID", "text", true],
    ], "import");
    detect(editor, "/api/oauth/cursor/auto-import", (data) => {
      if (!data.accessToken || !data.machineId) return false;
      fillFields(editor, data, ["accessToken", "machineId"]);
      return true;
    });
  }

  function zed() {
    const editor = credential("zed", "Import a Zed session", [
      ["userId", "User ID", "text", true], ["accessToken", "Access token", "password", true],
      ["systemId", "System ID", "text"],
    ], "import");
    detect(editor, "/api/oauth/zed/auto-import", (data) => {
      if (!data.accessToken || !data.userId) return false;
      fillFields(editor, data, ["accessToken", "userId", "systemId"]);
      return true;
    });

    const endpoint = "/api/oauth/zed";
    let attempt;
    let timer;
    const signIn = form("Sign in to Zed", [
      ["callbackUrl", "Zed callback URL", "password", true],
    ], "Finish sign-in", async ({ callbackUrl }) => {
      const current = attempt;
      if (!current?.state || Date.now() >= current.until) {
        throw new Error("Generate a new Zed sign-in link.");
      }
      let callback;
      try { callback = new URL(callbackUrl); }
      catch { throw new Error("Paste the Zed callback URL."); }
      const expected = new URL(current.redirectUri);
      if (callback.origin !== expected.origin || !["/", "/callback"].includes(callback.pathname)
        || callback.username || callback.password) {
        throw new Error("The callback does not match this Zed sign-in.");
      }
      clearTimeout(timer);
      const payload = {
        code: callback.href, state: current.state, redirectUri: current.redirectUri,
        codeVerifier: current.codeVerifier, systemId: current.systemId,
      };
      try {
        const result = await request(`${endpoint}/exchange`, "POST", payload);
        if (result.success !== true) throw new Error("The connection could not be saved.");
      } catch (error) {
        if (attempt === current && current.proxy) watch(current);
        throw new Error(message(error, payload));
      }
      await stop();
      return "Connection saved.";
    });
    const link = element("a", "Open Zed sign-in", "button");
    link.target = "_blank";
    link.rel = "noopener noreferrer";
    link.hidden = true;
    const cancel = action("Cancel sign-in", () => void signIn.run(stop));
    cancel.hidden = true;

    async function stop() {
      const current = attempt;
      attempt = undefined;
      clearTimeout(timer);
      link.hidden = cancel.hidden = true;
      link.removeAttribute("href");
      signIn.fields.callbackUrl.value = "";
      if (current?.proxy) await request(`${endpoint}/stop-proxy`).catch(() => {});
    }

    function watch(current) {
      timer = setTimeout(async () => {
        if (attempt !== current) return;
        if (Date.now() >= current.until) {
          await stop();
          notice(output, "Zed sign-in expired. Generate a new link.", true);
          return;
        }
        const query = new URLSearchParams({ state: current.state });
        const result = await request(`${endpoint}/poll-status?${query}`).catch(() => null);
        if (attempt !== current) return;
        if (result?.status === "done") {
          await stop();
          notice(output, "Connection saved.");
          updated();
        } else if (result?.status === "error") {
          await stop();
          notice(output, "Zed could not complete sign-in.", true);
        } else {
          watch(current);
        }
      }, 1000);
    }

    const start = action("Generate Zed sign-in link", () => void signIn.run(async () => {
      await stop();
      const localBrowser = ["localhost", "127.0.0.1"].includes(window.location.hostname);
      const proxy = localBrowser
        ? await request(`${endpoint}/start-proxy`).catch(() => null) : null;
      const current = { proxy: proxy?.success === true, until: Date.now() + 600000 };
      attempt = current;
      try {
        const redirect = new URL(current.proxy ? proxy.callbackUrl : "http://127.0.0.1:58443/");
        if (redirect.protocol !== "http:" || !["localhost", "127.0.0.1"].includes(redirect.hostname)
          || redirect.pathname !== "/" || !redirect.port || redirect.username || redirect.password
          || redirect.search || redirect.hash || (current.proxy && Number(redirect.port) !== proxy.port)) {
          throw new Error("Zed did not return a valid callback address.");
        }
        const parameters = new URLSearchParams({ redirect_uri: redirect.href });
        const result = await request(`${endpoint}/authorize?${parameters}`);
        const url = new URL(result.authUrl);
        if (url.origin !== "https://zed.dev" || url.pathname !== "/native_app_signin"
          || url.username || url.password || url.hash || url.searchParams.get("native_app_port") !== redirect.port
          || !url.searchParams.get("native_app_public_key") || !result.systemId
          || url.searchParams.get("system_id") !== result.systemId || !result.state
          || typeof result.codeVerifier !== "string" || !result.codeVerifier.startsWith("zed-rsa-pkcs1:")
          || result.redirectUri !== redirect.href) {
          throw new Error("Zed did not return a valid sign-in link.");
        }
        Object.assign(current, {
          state: result.state, redirectUri: result.redirectUri,
          codeVerifier: result.codeVerifier, systemId: result.systemId,
        });
        if (current.proxy) {
          const registered = await request(`${endpoint}/register-session`, "POST", {
            state: current.state, codeVerifier: current.codeVerifier, systemId: current.systemId,
          });
          if (registered.success !== true) throw new Error("Could not start Zed callback handling.");
          watch(current);
        }
        link.href = url.href;
        link.hidden = cancel.hidden = false;
      } catch (error) {
        await stop();
        throw new Error(message(error, current));
      }
    }));
    signIn.buttons.append(start, link, cancel);
    window.addEventListener("pagehide", () => void stop());
  }

  function commandCode() {
    const editor = form("Import a Command Code API key", [
      ["name", "Connection name", "text", true], ["apiKey", "API key", "password", true],
    ], "Save connection", async (body) => {
      const result = await request("/api/providers", "POST", { provider, ...body });
      if (!result.connection?.id) throw new Error("The connection could not be saved.");
    });
    detect(editor, "/api/oauth/commandcode/auto-import", (data) => {
      if (typeof data.apiKey !== "string" || !data.apiKey) return false;
      editor.fields.apiKey.value = data.apiKey;
      const name = [data.userName, data.keyName].find((value) => typeof value === "string" && value);
      editor.fields.name.value ||= name || "Command Code CLI";
      return true;
    });
  }

  function kiro() {
    const editor = credential("kiro", "Import Kiro IAM Identity Center credentials", [
      ["refreshToken", "Refresh token", "password", true], ["clientId", "Client ID", "text", true],
      ["clientSecret", "Client secret", "password", true], ["profileArn", "Profile ARN", "text"],
      ["region", "AWS region", "text", true, "us-east-1"],
    ], "import");
    detect(editor, "/api/oauth/kiro/auto-import", (data) => {
      if (typeof data.refreshToken !== "string" || !data.refreshToken) return false;
      editor.fields.refreshToken.value = data.refreshToken;
      fillFields(editor, data, ["clientId", "clientSecret", "profileArn", "region"]);
      editor.fields.region.value ||= "us-east-1";
      return true;
    });
    credential("kiro", "Import a Kiro API key", [
      ["apiKey", "API key", "password", true], ["region", "AWS region", "text", true, "us-east-1"],
    ], "api-key");
    credential("kiro", "Import Kiro CLIProxyAPI credentials", [
      ["cliProxyAuth", "Auth JSON", "textarea", true],
    ], "import-cli-proxy");
    kiroSocial();
    deviceAuthorization("kiro", "Kiro");
  }

  function kiroSocial() {
    let pending;
    const checked = form("Kiro social sign-in", [
      ["callbackUrl", "Callback URL", "password", true],
    ], "Save connection", async ({ callbackUrl }) => {
      if (!pending) throw new Error("Generate a sign-in link first.");
      let url;
      try { url = new URL(callbackUrl); }
      catch { throw new Error("Paste the Kiro callback URL."); }
      const code = url.searchParams.get("code");
      const state = url.searchParams.get("state");
      if (url.protocol !== "kiro:" || url.hostname.toLowerCase() !== "kiro.kiroagent"
        || url.pathname !== "/authenticate-success" || !code || state !== pending.state) {
        throw new Error("The callback does not match this sign-in.");
      }
      const result = await request("/api/oauth/kiro/social-exchange", "POST", {
        code, state, codeVerifier: pending.codeVerifier, provider: pending.provider,
      });
      if (result.success !== true) throw new Error("The connection could not be saved.");
      clear();
      return "Connection saved.";
    });
    const choice = element("select");
    choice.id = "kiro-social-provider";
    for (const [value, title] of [["google","Google"],["github","GitHub"]]) {
      const option = element("option", title);
      option.value = value;
      choice.append(option);
    }
    choice.value = "google";
    const label = element("label", "Sign-in provider");
    label.htmlFor = choice.id;
    const link = element("a", "Open Kiro sign-in", "button");
    link.id = "kiro-social-link";
    link.target = "_blank";
    link.rel = "noopener noreferrer";
    link.hidden = true;
    function clear() {
      pending = undefined;
      link.hidden = true;
      link.removeAttribute("href");
      checked.fields.callbackUrl.value = "";
    }
    checked.buttons.append(label, choice, action("Generate Kiro sign-in link", () => void checked.run(async () => {
      clear();
      const provider = choice.value;
      const result = await request(`/api/oauth/kiro/social-authorize?${new URLSearchParams({ provider })}`);
      let url;
      try { url = new URL(result.authUrl); }
      catch { throw new Error("Kiro did not return a valid sign-in link."); }
      if (url.origin !== "https://prod.us-east-1.auth.desktop.kiro.dev" || url.pathname !== "/login"
        || result.provider !== provider || !result.state || !result.codeVerifier) {
        throw new Error("Kiro did not return a valid sign-in link.");
      }
      if (choice.value !== provider) return;
      pending = result;
      link.href = url.href;
      link.hidden = false;
    })), link);
    choice.addEventListener("change", clear);
  }

  function trae() {
    let pending;
    let timer;
    const endpoint = "/api/oauth/trae";
    const editor = form("Connect Trae", [
      ["code", "Trae access token or callback URL", "password", true],
    ], "Save connection", async (body) => {
      const result = await request(`${endpoint}/exchange`, "POST", body);
      if (result.success !== true) throw new Error("The connection could not be saved.");
      await stop();
      return "Connection saved.";
    });
    const link = element("a", "Open Trae sign-in", "button");
    link.target = "_blank";
    link.rel = "noopener noreferrer";
    link.hidden = true;
    const cancel = action("Cancel sign-in", () => void editor.run(stop));
    cancel.hidden = true;
    const watch = (attempt) => {
      timer = setTimeout(async () => {
        if (pending !== attempt) return;
        const query = new URLSearchParams({ state: attempt.state });
        const data = await request(`${endpoint}/poll-status?${query}`).catch(() => null);
        if (pending !== attempt) return;
        if (data?.status === "done") {
          await stop();
          notice(output, "Connection saved.");
          updated();
        } else if (data?.status === "error") {
          await stop();
          notice(output, data.error || "Trae could not complete authorization.", true);
        } else if (data?.status === "pending" && Date.now() < attempt.until) {
          watch(attempt);
        }
      }, 1000);
    };
    async function stop() {
      const attempt = pending;
      pending = undefined;
      clearTimeout(timer);
      link.hidden = cancel.hidden = true;
      link.removeAttribute("href");
      if (attempt?.proxy) await request(`${endpoint}/stop-proxy`).catch(() => {});
    }
    editor.buttons.append(action("Generate Trae sign-in link", () => void editor.run(async () => {
      await stop();
      const proxy = ["localhost", "127.0.0.1"].includes(window.location.hostname)
        ? await request(`${endpoint}/start-proxy`).catch(() => null) : null;
      pending = { proxy: proxy?.success === true };
      try {
        const redirectUri = pending.proxy ? proxy.callbackUrl : "http://127.0.0.1:8080/callback";
        const params = new URLSearchParams({ redirect_uri: redirectUri });
        const result = await request(`${endpoint}/authorize?${params}`);
        const url = new URL(result.authUrl);
        const trusted = ["trae.ai", "marscode.com"].some((domain) =>
          url.hostname === domain || url.hostname.endsWith(`.${domain}`));
        if (!trusted || url.protocol !== "https:" || url.username || url.password
          || url.pathname !== "/authorization" || !result.state || result.redirectUri !== redirectUri) {
          throw new Error("Trae did not return a valid sign-in link.");
        }
        if (pending.proxy) {
          const registration = await request(`${endpoint}/register-session`, "POST", { state: result.state });
          if (registration.success !== true) throw new Error("Could not start Trae callback handling.");
          pending.state = result.state;
          pending.until = Date.now() + 600000;
          watch(pending);
        }
        link.href = url.href;
        link.hidden = cancel.hidden = false;
      } catch (error) {
        await stop();
        throw error;
      }
    })), link, cancel);
  }

  function authorization(provider, issuer, name) {
    let pending;
    let polling;
    const fixedCallback = {
      codex: "http://localhost:1455/auth/callback",
      xai: "http://127.0.0.1:56121/callback",
    }[provider];
    const fields = [
      ["callbackUrl", provider === "kimchi" ? "Callback URL or Kimchi token" : "Callback URL or authorization code", "password", true],
    ];
    if (provider === "gitlab") fields.unshift(
      ["instanceUrl", "GitLab base URL", "url", true, "https://gitlab.com"],
      ["clientId", "OAuth application ID", "text", true],
      ["clientSecret", "OAuth application secret", "password"],
    );
    const editor = form(`Authorize ${name}`, fields, "Save connection", async ({ callbackUrl }) => {
      if (!pending) throw new Error("Generate a sign-in link first.");
      if (provider === "claude" && !callbackUrl.includes("://")) {
        const [code, state] = callbackUrl.split("#");
        await exchange({ code, state });
      } else if (["xai", "kimchi"].includes(provider) && !callbackUrl.includes("://")) {
        await exchange({ code: callbackUrl, state: pending.state });
      } else {
        let callback;
        try { callback = new URL(callbackUrl); }
        catch { throw new Error("Paste the authorization callback URL."); }
        const expected = new URL(pending.redirectUri);
        if (callback.origin !== expected.origin || callback.pathname !== expected.pathname) {
          throw new Error("The callback does not match this sign-in.");
        }
        const fragment = new URLSearchParams(provider === "kimchi" ? callback.hash.slice(1) : "");
        await exchange({
          code: callback.searchParams.get("code") || callback.searchParams.get("token") || fragment.get("token"),
          state: callback.searchParams.get("state") || fragment.get("state"),
        });
      }
      return "Connection saved.";
    });
    const link = element("a", `Open ${name} sign-in`, "button");
    link.id = "provider-authorization-link";
    link.target = "_blank";
    link.rel = "noopener noreferrer";
    link.hidden = true;

    function clear(wipeSecret = false) {
      pending = undefined;
      if (polling !== undefined) clearTimeout(polling);
      polling = undefined;
      link.hidden = true;
      link.removeAttribute("href");
      editor.fields.callbackUrl.value = "";
      if (wipeSecret && editor.fields.clientSecret) editor.fields.clientSecret.value = "";
    }

    function watchCallback(attempt) {
      polling = setTimeout(async () => {
        if (pending !== attempt) return;
        let observation;
        try {
          observation = await request(`/api/oauth/${provider}/poll-status?${new URLSearchParams({ state: attempt.state })}`);
        } catch {
          if (pending === attempt) notice(output, "Paste the callback URL to finish authorization.", true);
          return;
        }
        if (pending !== attempt) return;
        if (observation.status === "done") {
          clear();
          notice(output, "Connection saved.");
          updated();
        } else if (observation.status === "error") {
          clear();
          notice(output, "The provider could not complete authorization.", true);
        } else if (observation.status === "pending" && Date.now() < attempt.pollUntil) {
          watchCallback(attempt);
        }
      }, 1000);
    }

    async function exchange(data) {
      if (!pending || typeof data?.code !== "string" || !data.code || data.state !== pending.state) {
        throw new Error("The callback does not match this sign-in.");
      }
      const { codeVerifier, redirectUri } = pending;
      const meta = pending.meta;
      const attempt = { code: data.code, state: data.state, codeVerifier, redirectUri, ...(meta ? { meta } : {}) };
      try {
        const result = provider === "xai" && pending.pollUntil
          ? await request("/api/oauth/xai/manual-code", "POST", { code: data.code, state: data.state })
          : await request(`/api/oauth/${provider}/exchange`, "POST", attempt);
        if (result.success !== true) throw new Error("The connection could not be saved.");
      } catch (error) {
        throw new Error(message(error, { ...attempt, clientSecret: meta?.clientSecret }));
      } finally {
        clear(true);
      }
    }

    editor.buttons.append(action("Generate sign-in link", () => void editor.run(async () => {
      clear();
      const port = window.location.port || (window.location.protocol === "https:" ? "443" : "80");
      const redirectUri = fixedCallback || (provider === "kimchi"
        ? `${window.location.origin}/callback` : `http://localhost:${port}/callback`);
      const params = new URLSearchParams({ redirect_uri: redirectUri });
      let expected = issuer;
      let meta;
      if (provider === "gitlab") {
        const baseUrl = editor.fields.instanceUrl.value.trim().replace(/\/+$/, "");
        const clientId = editor.fields.clientId.value.trim();
        const clientSecret = editor.fields.clientSecret.value.trim();
        const base = new URL(baseUrl);
        if (!clientId || !["http:", "https:"].includes(base.protocol) || base.username || base.password) {
          throw new Error("Enter a GitLab base URL and OAuth application ID.");
        }
        params.set("baseUrl", baseUrl);
        params.set("clientId", clientId);
        meta = { baseUrl, clientId, ...(clientSecret ? { clientSecret } : {}) };
        expected = base.origin;
      }
      const response = await request(`/api/oauth/${provider}/authorize?${params}`);
      let url;
      try { url = new URL(response.authUrl); }
      catch { throw new Error("The provider did not return a valid sign-in link."); }
      const trustedIssuer = provider === "xai"
        ? url.protocol === "https:" && !url.username && !url.password && !url.hash
          && (url.hostname === "x.ai" || url.hostname.endsWith(".x.ai"))
        : url.origin === expected && (provider !== "kimchi" ||
          (url.pathname === "/cli-auth" && !url.username && !url.password && !url.hash));
      if (!trustedIssuer || !response.state || !response.codeVerifier
        || response.redirectUri !== redirectUri) {
        throw new Error("The provider did not return a valid sign-in link.");
      }
      if (meta) response.meta = meta;
      pending = response;
      link.href = url.href;
      link.hidden = false;
      if (fixedCallback && ["localhost", "127.0.0.1"].includes(window.location.hostname)) {
        const params = new URLSearchParams({ app_port: port, state: response.state,
          code_verifier: response.codeVerifier, redirect_uri: redirectUri });
        const proxy = await request(`/api/oauth/${provider}/start-proxy?${params}`).catch(() => null);
        if (pending === response && proxy?.serverSide === true) {
          response.pollUntil = Date.now() + 300000;
          watchCallback(response);
        }
      }
    })), link, action("Cancel sign-in", () => {
      const attempt = pending;
      clear(true);
      if (fixedCallback && attempt?.pollUntil) {
        void request(`/api/oauth/${provider}/stop-proxy?${new URLSearchParams({ state: attempt.state })}`).catch(() => {});
      }
    }));

    function receive(data) {
      if (!pending || data?.state !== pending.state) return;
      if (data.error) {
        notice(output, "The provider declined authorization.", true);
        clear(true);
        return;
      }
      if (typeof data.code !== "string") return;
      void editor.run(async () => {
        await exchange(data);
        notice(output, "Connection saved.");
        updated();
      });
    }
    if (typeof BroadcastChannel !== "undefined") {
      const channel = new BroadcastChannel("oauth_callback");
      channel.addEventListener("message", (event) => receive(event.data));
      window.addEventListener("pagehide", () => channel.close(), { once: true });
    }
    window.addEventListener("message", (event) => {
      if (event.origin === window.location.origin && event.data?.type === "oauth_callback") {
        receive(event.data.data);
      }
    });
  }

  switch (provider) {
    case "github": deviceAuthorization("github", "GitHub"); break;
    case "kilocode": deviceAuthorization("kilocode", "Kilo Code"); break;
    case "nous": deviceAuthorization("nous", "Nous Portal"); break;
    case "kimi": deviceAuthorization("kimi", "Kimi Code"); break;
    case "glm": deviceAuthorization("glm", "GLM Coding Plan"); break;
    case "kimchi": authorization(provider, "https://app.kimchi.dev", "Kimchi"); break;
    case "muse": deviceAuthorization("muse", "Muse Code"); break;
    case "codebuddy-intl": deviceAuthorization("codebuddy-intl", "CodeBuddy"); break;
    case "codebuddy-cn": deviceAuthorization("codebuddy-cn", "CodeBuddy China"); break;
    case "windsurf":
      credential("windsurf", "Import a Windsurf API key", [["code", "API key", "password", true]], "exchange");
      break;
    case "trae": trae(); break;
    case "qoder": deviceAuthorization("qoder", "Qoder"); break;
    case "qoder-cn": deviceAuthorization("qoder-cn", "Qoder China"); break;
    case "minimax-code": deviceAuthorization(provider, "MiniMax Code China"); miniMaxDesktop(provider); break;
    case "minimax-code-global": deviceAuthorization(provider, "MiniMax Code Global"); miniMaxDesktop(provider); break;
    case "grok-web":
    case "chatgpt-web":
    case "gemini-web":
    case "muse-web":
    case "deepseek-web":
    case "uc-web":
      browserLogin(provider);
      break;
    case "gitlab":
      credential("gitlab", "Import a GitLab personal access token", [
        ["baseUrl", "GitLab base URL", "url", true, "https://gitlab.com"],
        ["token", "Personal access token", "password", true],
      ], "pat");
      authorization(provider, null, "GitLab");
      break;
    case "grok-cli": deviceAuthorization(provider, "Grok CLI"); grok(); break;
    case "iflow":
      credential("iflow", "Import an iFlow cookie and refresh its API key", [
        ["cookie", "iFlow cookie", "password", true],
      ], "cookie");
      authorization(provider, "https://iflow.cn", "iFlow");
      break;
    case "xiaomi-mimo": mimo(); break;
    case "zed": zed(); break;
    case "commandcode": commandCode(); break;
    case "cursor": cursor(); break;
    case "kiro": kiro(); break;
    case "codex":
      authorization(provider, "https://auth.openai.com", "Codex");
      credential("codex", "Import a Codex access token", [
        ["accessToken", "Access token", "password", true], ["name", "Connection name", "text"],
      ], "import-token");
      codexBulk();
      break;
    case "xai": authorization(provider, "https://auth.x.ai", "xAI"); break;
    case "claude": authorization(provider, "https://claude.ai", "Claude"); break;
    case "gemini-cli": authorization(provider, "https://accounts.google.com", "Gemini CLI"); break;
    case "antigravity": authorization(provider, "https://accounts.google.com", "Antigravity"); break;
    case "cline": authorization(provider, "https://api.cline.bot", "Cline"); break;
    case "clinepass": authorization(provider, "https://api.cline.bot", "ClinePass"); break;
  }
})();
