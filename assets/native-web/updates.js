(() => {
  "use strict";

  const { select, notice } = window.nineRouterUi;
  const panel = select("native-update");
  if (!panel) return;

  const check = select("native-update-check");
  const install = select("native-update-install");
  const signIn = select("native-update-sign-in");
  const status = select("native-update-status");
  const supportsInstall = panel.dataset.canInstall === "true";
  const signedIn = panel.dataset.signedIn === "true";
  const canInstall = supportsInstall && signedIn;
  let offer = null;
  let busy = false;

  async function request(path, method, timeout) {
    const response = await fetch(path, { method, cache: "no-store", signal: AbortSignal.timeout(timeout) });
    const data = await response.json().catch(() => null);
    if (!response.ok) {
      throw new Error(data?.message || data?.error || `Request failed (${response.status}).`);
    }
    return data;
  }

  function controls() {
    check.disabled = busy;
    install.disabled = busy;
    install.hidden = !canInstall || !offer;
    signIn.hidden = !supportsInstall || signedIn || !offer;
  }

  check.addEventListener("click", async () => {
    if (busy) return;
    busy = true;
    offer = null;
    controls();
    notice(status, "Checking the signed release source…");
    try {
      const { currentVersion, latestVersion, hasUpdate } = await request("/api/version", "GET", 12000) || {};
      if (typeof currentVersion !== "string" || !currentVersion) {
        throw new Error("Could not read the running version.");
      }
      if (hasUpdate === true && typeof latestVersion === "string" && latestVersion && latestVersion !== currentVersion) {
        offer = latestVersion;
        let message = `Signed preview v${latestVersion} is available. Running v${currentVersion}.`;
        if (!supportsInstall) message += " Install the preview manually on this platform.";
        notice(status, message);
      } else if (typeof latestVersion === "string" && latestVersion) {
        notice(status, `Running v${currentVersion}. No newer signed preview is available.`);
      } else {
        notice(status, `Running v${currentVersion}. No signed release was returned by the source.`);
      }
    } catch (error) {
      notice(status, error.message || "The update check failed.", true);
    } finally {
      busy = false;
      controls();
    }
  });

  async function waitForRestart(version) {
    const until = Date.now() + 60000;
    let lastVersion = null;
    while (Date.now() < until) {
      await new Promise(resolve => window.setTimeout(resolve, 1000));
      try {
        const data = await request("/api/version", "GET", 4000);
        if (typeof data?.currentVersion !== "string" || !data.currentVersion) continue;
        await request("/api/settings/require-login", "GET", 4000);
        lastVersion = data.currentVersion;
        if (lastVersion === version) {
          notice(status, `Update complete. Running v${version}.`);
          return;
        }
      } catch {
        lastVersion = null;
      }
    }
    const message = lastVersion
      ? `Running v${lastVersion}. The gateway did not confirm v${version} after the update attempt.`
      : `The gateway did not confirm v${version}. Check the running version before trying again.`;
    notice(status, message, true);
  }

  install.addEventListener("click", async () => {
    if (busy || !canInstall || !offer) return;
    const version = offer;
    busy = true;
    offer = null;
    controls();
    notice(status, `Downloading and verifying signed preview v${version}…`);
    try {
      const reply = await request("/api/version/update", "POST", 120000);
      if (reply?.success !== true) {
        throw new Error(reply?.message || "The gateway did not accept the update.");
      }
      notice(status, "Update staged. Waiting for the gateway to restart…");
      await waitForRestart(version);
    } catch (error) {
      notice(status, error.message || "The update could not be confirmed.", true);
    } finally {
      busy = false;
      controls();
    }
  });
})();
