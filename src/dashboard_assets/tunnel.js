(() => {
  "use strict";

  const { action, element, notice, request, select } = window.nineRouterUi;
  const output = select("tunnel-status");
  const dashboard = select("tunnel-dashboard-access");
  const installer = select("tailscale-install-form");
  const base = "/api/tunnel";
  let operating = false;
  let updating = false;
  let installPlatform;
  let installWithBrew = false;

  function addresses(id, entries, gateway) {
    const container = select(id);
    container.replaceChildren();
    for (const [name, value] of entries) {
      if (!value) continue;
      const row = element("p", name + " ");
      try {
        const url = new URL(value);
        if (!["https:", "http:"].includes(url.protocol)) continue;
        const link = element("a", value);
        link.href = url.href;
        link.rel = "noreferrer";
        row.append(link);
      } catch {
        row.append(document.createTextNode(value));
      }
      container.append(row);
    }
    if (!gateway) return;
    let endpoint;
    try { endpoint = new URL(gateway); }
    catch { return; }
    if (!["https:", "http:"].includes(endpoint.protocol) || endpoint.username || endpoint.password
      || endpoint.search || endpoint.hash) return;
    const path = endpoint.pathname.replace(/\/+$/, "");
    endpoint.pathname = path.endsWith("/v1") ? path : `${path}/v1`;
    const api = element("p", "API endpoint ");
    api.append(element("code", endpoint.href), document.createTextNode(" "), action("Copy API endpoint", async () => {
      try {
        await navigator.clipboard.writeText(endpoint.href);
        notice(output, "API endpoint copied.");
      } catch {
        notice(output, "Clipboard unavailable. Select the API endpoint to copy it.", true);
      }
    }));
    container.append(api);
  }

  function draw(kind, state) {
    select(`${kind}-state`).textContent = state?.running ? "Running" :
      state?.settingsEnabled ? "Enabled · not running" : "Disabled";
    select(`${kind}-enable`).disabled = operating || Boolean(state?.running);
    select(`${kind}-disable`).disabled = operating || !state?.settingsEnabled;
  }

  async function load() {
    if (updating) return;
    updating = true;
    try {
      const { tunnel, tailscale, download } = await request(`${base}/status`);
      draw("cloudflare", tunnel);
      draw("tailscale", tailscale);
      if (download?.downloading) {
        select("cloudflare-state").textContent = `Downloading cloudflared · ${download.progress}%`;
      }
      addresses("cloudflare-addresses", [["Public URL", tunnel?.publicUrl], ["Tunnel URL", tunnel?.tunnelUrl]],
        tunnel?.running ? tunnel.publicUrl || tunnel.tunnelUrl : null);
      addresses("tailscale-addresses", [["Funnel URL", tailscale?.tunnelUrl]], tailscale?.running ? tailscale.tunnelUrl : null);
    } catch (error) { notice(output, error.message, true); }
    finally { updating = false; }
  }

  async function change(path) {
    if (operating) return;
    operating = true;
    for (const kind of ["cloudflare", "tailscale"]) for (const type of ["enable", "disable"]) {
      select(`${kind}-${type}`).disabled = true;
    }
    notice(output, "Updating tunnel…");
    try {
      const result = await request(`${base}/${path}`, "POST");
      if (result.needsLogin || result.funnelNotEnabled) {
        const destination = result.authUrl || result.enableUrl;
        const url = new URL(destination);
        if (url.protocol !== "https:") throw new Error("Tailscale returned an invalid authorization URL.");
        const link = element("a", "Open Tailscale authorization");
        link.href = url.href;
        link.rel = "noreferrer";
        link.target = "_blank";
        notice(output, result.needsLogin ? "Tailscale sign-in required. " : "Funnel approval required. ");
        output.append(link);
      } else {
        notice(output, result.success ? "" : result.error || "Could not update the tunnel.", !result.success);
      }
    } catch (error) { notice(output, error.message, true); }
    finally {
      operating = false;
      await load();
    }
  }

  select("cloudflare-enable").addEventListener("click", () => change("enable"));
  select("cloudflare-disable").addEventListener("click", () => change("disable"));
  select("tailscale-enable").addEventListener("click", () => change("tailscale-enable"));
  select("tailscale-disable").addEventListener("click", () => change("tailscale-disable"));
  select("tailscale-check").addEventListener("click", async () => {
    const button = select("tailscale-check");
    const setup = select("tailscale-setup");
    button.disabled = true;
    try {
      const status = await request(`${base}/tailscale-check`);
      setup.textContent = !status.installed ? "Tailscale is not installed." :
        !status.daemonRunning ? "The Tailscale daemon is stopped." :
        !status.loggedIn ? "Tailscale sign-in required." : "Tailscale is connected.";
      setup.hidden = false;
      installPlatform = status.platform;
      installWithBrew = status.brewAvailable === true;
      const linux = installPlatform === "linux";
      const desktop = ["darwin", "win32"].includes(installPlatform);
      installer.hidden = linux ? Boolean(status.loggedIn && status.daemonRunning) :
        !desktop || Boolean(status.installed);
      select("tailscale-sudo-fields").hidden = !linux;
      const password = select("tailscale-sudo-password");
      password.required = linux;
      password.disabled = !linux;
      const consent = select("tailscale-install-consent");
      consent.textContent = installPlatform === "win32" ? "Approve Windows Installer on this computer." :
        installWithBrew ? "Homebrew installs the Tailscale CLI and service." :
        "Complete setup in macOS Installer on this computer.";
      consent.hidden = !desktop;
    } catch (error) { notice(output, error.message, true); }
    finally { button.disabled = false; }
  });
  installer.addEventListener("submit", async event => {
    event.preventDefault();
    if (operating) return;
    const field = select("tailscale-sudo-password");
    const submit = installer.querySelector('[type="submit"]');
    const payload = installPlatform === "linux" ? { sudoPassword: field.value } : {};
    field.value = "";
    operating = submit.disabled = true;
    notice(output, "Preparing Tailscale…");
    try {
      const response = await fetch(`${base}/tailscale-install`, {
        method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(payload),
      });
      if (!response.ok || !response.headers.get("content-type")?.includes("text/event-stream")) {
        const data = await response.json().catch(() => ({}));
        throw new Error(data.error || "Tailscale setup failed.");
      }
      const reader = response.body.getReader();
      const decoder = new TextDecoder();
      let buffer = "";
      let completed = false;
      try {
        while (!completed) {
          const { done, value } = await reader.read();
          buffer = (buffer + decoder.decode(value, { stream: !done })).replace(/\r\n/g, "\n");
          const frames = buffer.split("\n\n");
          buffer = frames.pop();
          if (buffer.length > 65536) throw new Error("Invalid Tailscale progress response.");
          for (const frame of frames) {
            const lines = frame.split("\n");
            const name = lines.find(line => line.startsWith("event:"))?.slice(6).trim();
            const encoded = lines.filter(line => line.startsWith("data:")).map(line => line.slice(5).trim()).join("\n");
            if (!name || !encoded) continue;
            const data = JSON.parse(encoded);
            if (name === "progress") notice(output, data.message || "");
            if (name === "error") throw new Error(data.error || "Tailscale setup failed.");
            if (name === "done") {
              completed = true;
              if (data.authUrl) {
                const url = new URL(data.authUrl);
                if (url.origin !== "https://login.tailscale.com") throw new Error("Invalid Tailscale authorization URL.");
                const link = element("a", "Open Tailscale authorization");
                link.href = url.href;
                link.target = "_blank";
                link.rel = "noopener noreferrer";
                notice(output, "Tailscale sign-in required. ");
                output.append(link);
              } else if (data.rebootRequired) {
                notice(output, "Restart Windows to complete Tailscale setup.");
                installer.hidden = true;
              } else if (installPlatform !== "linux") {
                notice(output, installWithBrew ?
                  "Tailscale is installed. Start the Homebrew service and sign in before enabling Funnel." :
                  "Tailscale is installed. Open Tailscale and sign in before enabling Funnel.");
                installer.hidden = true;
              } else {
                notice(output, "Tailscale is ready.");
                installer.hidden = true;
              }
              break;
            }
          }
          if (done) break;
        }
      } finally {
        await reader.cancel().catch(() => {});
        reader.releaseLock();
      }
      if (!completed) throw new Error("Tailscale setup ended before confirmation.");
    } catch (error) { notice(output, error.message, true); }
    finally {
      operating = submit.disabled = false;
      await load();
    }
  });
  dashboard.addEventListener("change", async () => {
    dashboard.disabled = true;
    try { await request("/api/settings", "PATCH", { tunnelDashboardAccess: dashboard.checked }); }
    catch (error) { dashboard.checked = !dashboard.checked; notice(output, error.message, true); }
    finally { dashboard.disabled = false; }
  });
  request("/api/settings").then(config => { dashboard.checked = config.tunnelDashboardAccess === true; })
    .catch(error => notice(output, error.message, true));
  void load();
  window.setInterval(() => { if (!document.hidden) void load(); }, 3000);
})();
