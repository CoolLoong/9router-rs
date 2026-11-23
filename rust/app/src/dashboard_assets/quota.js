(async () => {
  "use strict";

  const { select, request, notice, element, action } = window.nineRouterUi;
  const output = select("quota-status");
  const accounts = select("quota-accounts");
  const filter = select("quota-provider");
  const status = select("quota-state");
  const size = select("quota-size");
  const sort = select("quota-sort");
  const auto = select("quota-refresh-auto");
  const hidden = select("quota-show-hidden");
  const prev = select("quota-previous");
  const next = select("quota-next");
  const refresh = select("quota-refresh");
  const expiry = select("quota-expiring");
  const turnOff = select("quota-disable-depleted");
  const turnOn = select("quota-enable-available");
  const states = new Map();
  let connections = [];
  let visibility = {};
  let visibilitySaving = false;
  let bulkBusy = false;
  let loadingPage = false;
  let expiringFirst = false;
  let page = 1;
  let revision = 0;

  let quota;
  try {
    quota = await import("/__9router-ui/quota-format.js");
  } catch (error) {
    notice(output, error.message, true);
    return;
  }
  try { auto.checked = JSON.parse(localStorage.getItem(quota.AUTO_REFRESH_STORAGE_KEY)) ?? true; } catch {}
  const usage = account => `/api/usage/${encodeURIComponent(account.id)}`;
  const connection = account => `/api/providers/${encodeURIComponent(account.id)}`;
  const usable = account => account.isActive !== false;
  const remaining = quota.getRemainingPercentage;

  function resetTime(value) {
    if (!value) return "—";
    const date = new Date(value);
    return Number.isFinite(date.getTime()) ? date.toLocaleString() : "—";
  }

  function appendRows(table, account, data) {
    const all = data.quotas;
    const visible = quota.filterQuotasByVisibility(account.provider, all, visibility);
    const skipped = quota.getHiddenQuotaRows(account.provider, all, visibility);
    const entries = visible.map(row => [row, false]);
    if (hidden.checked) entries.push(...skipped.map(row => [row, true]));
    if (sort.value !== "default") {
      const direction = sort.value === "remaining-asc" ? 1 : -1;
      entries.sort(([a], [b]) => direction * (remaining(a) - remaining(b)) || a.name.localeCompare(b.name));
    }
    for (const [quotaRow, isHidden] of entries) {
      const row = element("tr");
      const name = element("th", quotaRow.name || "Quota");
      name.scope = "row";
      row.append(name, element("td", quotaRow.emptyMessage || `${remaining(quotaRow)}%`));
      row.append(element("td", Number(quotaRow.total) > 0 ? `${quotaRow.used} / ${quotaRow.total}` : "—"));
      row.append(element("td", resetTime(quotaRow.resetAt)));
      const operations = element("td");
      if (quota.getQuotaVisibilityKey(quotaRow)) {
        operations.append(action(isHidden ? "Show" : "Hide", () => changeVisibility(account.provider, quotaRow, isHidden)));
      }
      row.append(operations);
      table.append(row);
    }
    return entries.length;
  }

  function render() {
    accounts.replaceChildren();
    turnOff.disabled = bulkBusy || loadingPage || !bulkTargets(false).length;
    turnOn.disabled = bulkBusy || loadingPage || !bulkTargets(true).length;
    if (!connections.length) accounts.append(element("p", "No matching accounts", "muted"));
    const ordered = quota.sortVisibleConnections(connections, Object.fromEntries(states), expiringFirst, filter.value, sort.value);
    for (const account of ordered) {
      const state = states.get(account.id);
      const card = element("section", null, "card");
      const heading = element("div", null, "card-heading");
      const identity = element("div");
      identity.append(element("h2", quota.getConnectionLabel(account)));
      identity.append(element("p", [account.provider, state?.data?.plan, usable(account) ? "Active" : "Turned off"]
        .filter(Boolean).join(" · "), "muted"));
      const controls = element("div", null, "key-actions");
      const edit = element("a", "Edit connection", "button");
      edit.href = `/dashboard/providers/connections/${encodeURIComponent(account.id)}`;
      controls.append(edit);
      const toggle = action(usable(account) ? "Turn off" : "Turn on", () => setActive(account));
      toggle.disabled = bulkBusy;
      controls.append(toggle);
      controls.append(action("Remove", () => remove(account), "button-danger"));
      const read = action("Refresh quota", () => loadQuota(account, true, revision));
      read.disabled = Boolean(state?.loading);
      controls.append(read);
      if (["claude", "codex"].includes(account.provider) && account.authType === "oauth") {
        controls.append(action("Auto-ping", () => openAutoPing(account)));
      }
      if (["claude", "codex"].includes(account.provider) && state?.data) {
        controls.append(action("Reset credits", () => credits(account, state.data)));
      }
      heading.append(identity, controls);
      card.append(heading);
      if (state?.loading) card.append(element("p", "Refreshing quota…", "muted"));
      if (state?.error) card.append(element("p", state.error, "notice notice-error"));
      if (state?.data?.message) card.append(element("p", state.data.message, "muted"));
      if (state?.data) {
        const frame = element("div", null, "table-scroll");
        const table = element("table", null, "usage-table");
        const labels = element("tr");
        for (const label of ["Quota", "Remaining", "Usage", "Reset", "Visibility"]) {
          const head = element("th", label);
          head.scope = "col";
          labels.append(head);
        }
        const thead = element("thead");
        thead.append(labels);
        const body = element("tbody");
        if (appendRows(body, account, state)) {
          table.append(thead, body);
          frame.append(table);
          card.append(frame);
        } else if (!state?.data?.message) {
          card.append(element("p", "No visible quotas", "muted"));
        }
      }
      accounts.append(card);
    }
  }

  async function loadQuota(account, force, current) {
    const prior = states.get(account.id);
    if (prior?.loading) return;
    states.set(account.id, { ...prior, loading: true, error: null });
    if (current === revision) render();
    try {
      const data = await request(usage(account) + (force ? "?force=1" : ""));
      states.set(account.id, { data, quotas: quota.parseQuotaData(account.provider, data), checkedAt: Date.now() });
    } catch (error) {
      states.set(account.id, { ...prior, loading: false, error: error.message, checkedAt: Date.now() });
    } finally {
      if (current === revision || connections.some(row => row.id === account.id)) render();
    }
  }

  async function refreshPage(force = false, current = revision, dueOnly = false) {
    const queue = connections.filter(account => !dueOnly ||
      Date.now() - (states.get(account.id)?.checkedAt ?? 0) >= (account.provider === "claude" ?
        quota.CLAUDE_REFRESH_INTERVAL_MS : quota.REFRESH_INTERVAL_MS));
    async function worker() {
      while (queue.length && current === revision) await loadQuota(queue.shift(), force, current);
    }
    await Promise.all(Array.from({ length: Math.min(queue.length, 3) }, worker));
  }

  async function load(target = page) {
    const current = ++revision;
    loadingPage = true;
    turnOff.disabled = turnOn.disabled = true;
    const requested = new URLSearchParams({ page: target, pageSize: size.value, accountStatus: status.value,
      provider: filter.value, sort: quota.sortRequestFromExpiringFirst(expiringFirst) });
    refresh.disabled = prev.disabled = next.disabled = true;
    notice(output, "Loading accounts…");
    try {
      const result = await request(`/api/providers/client?${requested}`);
      if (current !== revision) return;
      connections = result.connections ?? [];
      const selected = filter.value;
      filter.replaceChildren(element("option", "All providers"));
      filter.options[0].value = "all";
      for (const name of result.providerOptions ?? []) {
        const option = element("option", name);
        option.value = name;
        filter.append(option);
      }
      filter.value = [...filter.options].some(option => option.value === selected) ? selected : "all";
      const pagination = result.pagination;
      page = pagination.page;
      prev.disabled = page <= 1;
      next.disabled = page >= pagination.totalPages;
      select("quota-pagination").textContent = quota.getConnectionsPaginationSummary(pagination);
      notice(output);
      render();
      await refreshPage(false, current, true);
      return current === revision;
    } catch (error) {
      if (current === revision) notice(output, error.message, true);
    } finally {
      if (current === revision) {
        loadingPage = false;
        refresh.disabled = false;
        render();
      }
    }
    return false;
  }

  async function setActive(account) {
    try {
      await request(connection(account), "PUT", { isActive: !usable(account) });
      states.delete(account.id);
      await load();
    } catch (error) { notice(output, error.message, true); }
  }

  function bulkTargets(enable) {
    return connections.filter(account => {
      if (usable(account) === enable) return false;
      const state = states.get(account.id);
      if (state?.loading || state?.error || !state?.quotas) return false;
      const quotas = state.quotas.filter(row => Number(row.total) > 0);
      if (!quotas.length) return false;
      const depleted = quotas.some(row => quota.calculatePercentage(row.used, row.total) <= quota.DEPLETED_QUOTA_THRESHOLD);
      return enable ? !depleted : depleted;
    });
  }

  async function bulkSetActive(enable) {
    if (bulkBusy || loadingPage) return;
    const targets = bulkTargets(enable);
    if (!targets.length) return;
    const action = enable ? "Turn on" : "Turn off";
    bulkBusy = true;
    render();
    let succeeded = 0;
    try {
      for (let offset = 0; offset < targets.length; offset += 8) {
        const results = await Promise.allSettled(targets.slice(offset, offset + 8)
          .map(account => request(connection(account), "PUT", { isActive: enable })));
        succeeded += results.filter(result => result.status === "fulfilled").length;
      }
      if (await load()) {
        notice(output, `${action} succeeded for ${succeeded} of ${targets.length} accounts.`, succeeded < targets.length);
      }
    } finally {
      bulkBusy = false;
      render();
    }
  }

  async function remove(account) {
    if (!window.confirm(`Remove ${quota.getConnectionLabel(account)}?`)) return;
    try {
      await request(connection(account), "DELETE");
      states.delete(account.id);
      await load();
    } catch (error) { notice(output, error.message, true); }
  }

  async function changeVisibility(provider, row, show) {
    if (visibilitySaving) return;
    visibilitySaving = true;
    try {
      const saved = (await request("/api/settings")).quotaVisibility ?? {};
      const configuration = saved[provider] ?? {};
      const keys = new Set(configuration.hidden ?? []);
      const key = quota.getQuotaVisibilityKey(row);
      if (show) keys.delete(key);
      else keys.add(key);
      if (provider === "antigravity" && ["gemini", "claude"].includes(key)) {
        for (const child of keys) {
          if (child.startsWith(`${key}-`) && (key !== "gemini" || !child.includes("image"))) keys.delete(child);
        }
      }
      const updated = { ...saved, [provider]: { ...configuration, hidden: [...keys] } };
      await request("/api/settings", "PATCH", { quotaVisibility: updated });
      visibility = updated;
      render();
    } catch (error) { notice(output, error.message, true); }
    finally { visibilitySaving = false; }
  }

  async function openAutoPing(account) {
    try {
      const { openQuotaAutoPing } = await import("/__9router-ui/quota-auto-ping.js");
      await openQuotaAutoPing(account, quota.getConnectionLabel(account), output);
    } catch (error) { notice(output, error.message, true); }
  }

  async function credits(account, data) {
    const dialog = select("quota-credits");
    const banner = select("quota-credits-status");
    const list = select("quota-credits-list");
    const codex = account.provider === "codex";
    const endpoint = usage(account) + (codex ? "/codex-reset-credits" : "/claude-reset");
    list.replaceChildren();
    select("quota-credits-title").textContent = `Reset credits · ${quota.getConnectionLabel(account)}`;
    notice(banner, "Loading reset credits…");
    dialog.showModal();
    try {
      const values = codex ? await request(endpoint) : data.resetCredits;
      if (!values) {
        notice(banner, "No reset credits available.");
        return;
      }
      const available = Number(values.availableCount ?? (values.nextGrantId ? 1 : 0));
      list.append(element("dt", "Available"), element("dd", available));
      for (const entry of values.credits ?? values.grants ?? []) {
        list.append(element("dt", entry.name || entry.id || "Credit"),
          element("dd", resetTime(entry.expiresAt || entry.endsAt)));
      }
      if (available > 0) {
        const button = action("Use reset credit", async () => {
          if (!window.confirm("Use a reset credit for this account?")) return;
          button.disabled = true;
          try {
            await request(endpoint, "POST", codex ? undefined : { grantId: values.nextGrantId });
            dialog.close();
            await loadQuota(account, true, revision);
          } catch (error) { notice(banner, error.message, true); button.disabled = false; }
        });
        const cell = element("dd");
        cell.append(button);
        list.append(element("dt", "Reset"), cell);
      }
      notice(banner);
    } catch (error) { notice(banner, error.message, true); }
  }

  filter.addEventListener("change", () => load(1));
  status.addEventListener("change", () => load(1));
  size.addEventListener("change", () => { if (size.reportValidity()) load(1); });
  sort.addEventListener("change", render);
  expiry.addEventListener("click", () => {
    expiringFirst = !expiringFirst;
    expiry.setAttribute("aria-pressed", String(expiringFirst));
    render();
  });
  turnOff.addEventListener("click", () => bulkSetActive(false));
  turnOn.addEventListener("click", () => bulkSetActive(true));
  hidden.addEventListener("change", render);
  prev.addEventListener("click", () => load(page - 1));
  next.addEventListener("click", () => load(page + 1));
  refresh.addEventListener("click", async () => {
    refresh.disabled = true;
    try { await refreshPage(true); } finally { refresh.disabled = false; }
  });
  auto.addEventListener("change", () => {
    try { localStorage.setItem(quota.AUTO_REFRESH_STORAGE_KEY, JSON.stringify(auto.checked)); } catch {}
  });
  window.setInterval(() => {
    if (auto.checked && !document.hidden) void refreshPage(false, revision, true);
  }, quota.REFRESH_INTERVAL_MS);
  try { visibility = (await request("/api/settings")).quotaVisibility ?? {}; } catch {}
  await load(1);
})();
