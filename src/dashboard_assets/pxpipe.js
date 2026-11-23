(() => {
  "use strict";

  const { request, element, select, notice } = window.nineRouterUi;
  const refresh = select("pxpipe-refresh");
  const output = select("pxpipe-status");
  const period = select("pxpipe-window");
  const controls = [...select("pxpipe-controls").querySelectorAll("[data-pxpipe-action]")];
  const format = new Intl.NumberFormat();
  const number = value => Number.isFinite(value) ? format.format(value) : "—";
  const reasons = {
    applied: "Compressed", below_threshold: "Below size threshold",
    not_profitable: "Savings below minimum", below_min_chars: "Below minimum characters",
    below_min_tokens: "Below minimum tokens", unsupported_model: "Unsupported model",
    unsupported_format: "Unsupported format", timeout: "Timed out", transform_error: "Transform error",
    passthrough: "Passed through", unsupported_shape: "Request passed through",
    disabled: "Disabled", not_installed: "Not installed",
  };
  let current;

  function render() {
    if (!current) return;
    const { state, health, stats, logs } = current;
    const window = stats.windows?.[period.value] ?? {};
    const metrics = select("pxpipe-metrics");
    const label = !state.installed ? "Not installed" : health?.healthy ? "Healthy" :
      state.running ? "Running" : "Stopped";
    metrics.replaceChildren();
    for (const [name, value] of [
      ["Status", label], ["Pipeline", state.enabled ? "Enabled" : "Disabled"],
      ["Version", state.version ?? "—"], ["Uptime", state.uptimeMs > 0 ? `${Math.floor(state.uptimeMs / 60000)} min` : "—"],
      ["Requests", number(window.requests)], ["Compressed", number(window.compressed)],
      ["Bypassed", number(window.bypassed)], ["Errors", number(window.errors)],
      ["Estimated tokens before", number(window.tokensBeforeEst)], ["Estimated tokens after", number(window.tokensAfterEst)],
      ["Estimated tokens saved", number(window.tokensSavedEst)], ["Reduction", window.savedPct == null ? "—" : `${window.savedPct}%`],
      ["Images generated", number(window.imagesGenerated)],
      ["Average compression", window.avgCompressionMs == null ? "—" : `${window.avgCompressionMs} ms`],
    ]) {
      const list = element("div");
      list.append(element("dt", name), element("dd", value));
      metrics.append(list);
    }
    const trend = select("pxpipe-trend");
    trend.replaceChildren();
    const timeline = stats.timeline ?? [];
    const maximum = Math.max(1, ...timeline.map(item => Number(item.tokensSavedEst) || 0));
    for (const item of timeline) {
      const row = element("tr");
      const bar = element("meter");
      bar.max = maximum;
      bar.value = Number(item.tokensSavedEst) || 0;
      bar.setAttribute("aria-label", `Tokens saved on ${item.date}`);
      const savings = element("td", number(item.tokensSavedEst) + " ");
      savings.append(bar);
      row.append(element("th", item.date), savings);
      trend.append(row);
    }
    const history = select("pxpipe-history");
    history.replaceChildren(...(stats.recent ?? []).slice(0, 50).map(item => {
      const time = new Date(item.ts);
      const model = item.provider ? `${item.provider}/${item.model}` : item.model ?? "—";
      const row = element("tr");
      for (const value of [
        Number.isFinite(time.getTime()) ? time.toLocaleString() : "—", model,
        number(item.tokensBeforeEst), number(item.tokensAfterEst), number(item.tokensSavedEst),
        item.durationMs == null ? "—" : `${item.durationMs} ms`, reasons[item.reason] ?? item.reason ?? "—",
      ]) row.append(element("td", value));
      return row;
    }));
    select("pxpipe-log").textContent = logs.installLog || "No logs";
  }

  function setActions(busy) {
    refresh.disabled = busy;
    const state = current?.state;
    for (const button of controls) {
      const action = button.dataset.pxpipeAction;
      if (action === "install") button.textContent = state?.installed ? "Repair PXPIPE" : "Install PXPIPE";
      button.disabled = busy || !state || (
        action === "stop" ? !state.running :
        action === "start" ? state.running || !(state.installed || state.autoInstall) :
        action !== "install" && !state.installed
      );
    }
  }

  async function load(action) {
    setActions(true);
    notice(output, action ? "Updating PXPIPE…" : "Refreshing…");
    try {
      const result = action ? await request("/api/pxpipe/" + action, "POST") : null;
      const [state, stats, logs] = await Promise.all([
        request("/api/pxpipe/status"), request("/api/pxpipe/stats"), request("/api/pxpipe/logs?limit=50"),
      ]);
      const health = action === "health" ? result : result?.health;
      current = { state, stats, logs, health };
      render();
      notice(output, health?.error || (health?.healthy ? "PXPIPE health check passed." : undefined), Boolean(health?.error));
    } catch (error) { notice(output, error.message, true); }
    finally { setActions(false); }
  }

  refresh.addEventListener("click", () => load());
  for (const button of controls) {
    button.addEventListener("click", () => load(button.dataset.pxpipeAction));
  }
  period.addEventListener("change", render);
  void load();
})();
