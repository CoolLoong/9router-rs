(() => {
  "use strict";

  const { select, request, notice, element, action } = window.nineRouterUi;
  const status = select("optimization-status");
  const open = (id) => select(id).showModal();
  const close = (id) => select(id).close();

  function fail(error, output) {
    if (error.status === 401) window.location.replace("/login");
    else notice(output, error.message, true);
  }

  async function work(control, operation, output = status) {
    if (control.disabled) return;
    control.disabled = true;
    notice(output);
    try {
      await operation();
    } catch (error) {
      fail(error, output);
    } finally {
      control.disabled = false;
    }
  }

  function form(id, handler, output) {
    const target = select(id);
    target.addEventListener("submit", (event) => {
      event.preventDefault();
      work(target.querySelector('[type="submit"]'), handler, output);
    });
  }

  for (const button of document.querySelectorAll("[data-optimization-close]")) {
    button.addEventListener("click", () => close(button.dataset.optimizationClose));
  }

  function proxyPools() {
    const api = "/api/proxy-pools";
    const search = select("pool-search");
    const filter = select("pool-filter");
    const list = select("pool-list");
    const selectAll = select("pool-select");
    const healthButton = select("pool-health");
    const clearSelection = select("pool-clear-selection");
    const edit = select("pool-form");
    const poolUrl = select("pool-url");
    const type = select("pool-type");
    const selected = new Set();
    const kinds = new Map([...type.options].map((option) => [option.value, option.textContent]));
    let pools = [];
    let current;
    let anchor;
    let bulkBusy = false;

    const path = (id) => `${api}/${encodeURIComponent(id)}`;
    const active = (pool) => pool.isActive === true;
    const host = (target) => {
      try {
        const parsed = new URL(target);
        return `${parsed.protocol}//${parsed.host}`;
      } catch {
        return "";
      }
    };

    function visible() {
      const term = search.value.trim().toLowerCase();
      return pools.filter((pool) =>
        (filter.value === "all" || active(pool) === (filter.value === "enabled"))
          && [pool.name, host(pool.proxyUrl), kinds.get(pool.type)].some((value) => String(value || "").toLowerCase().includes(term)));
    }

    function picked(rows) {
      return rows.filter((pool) => selected.has(pool.id));
    }

    function selection() {
      const rows = visible();
      const chosen = picked(rows).length;
      selectAll.checked = rows.length > 0 && chosen === rows.length;
      selectAll.indeterminate = chosen > 0 && chosen < rows.length;
      selectAll.disabled = bulkBusy || rows.length === 0;
      filter.disabled = bulkBusy;
      search.disabled = bulkBusy;
      list.inert = bulkBusy;
      clearSelection.hidden = chosen === 0;
      clearSelection.disabled = bulkBusy;
      select("pool-delete-selected").disabled = bulkBusy || chosen === 0;
      for (const name of ["pool-enable", "pool-disable", "pool-health"]) {
        select(name).disabled = bulkBusy || rows.length === 0;
      }
    }

    function row(pool) {
      const card = element("article", null, "key-row");
      const checkbox = element("input");
      checkbox.type = "checkbox";
      checkbox.checked = selected.has(pool.id);
      checkbox.dataset.poolId = pool.id;
      checkbox.setAttribute("aria-label", `Select ${pool.name || "pool"}`);
      checkbox.addEventListener("click", (event) => {
        const ids = visible().map((item) => item.id);
        const here = ids.indexOf(pool.id);
        const previous = event.shiftKey ? ids.indexOf(anchor) : -1;
        const range = previous < 0 ? [pool.id] : ids.slice(Math.min(previous, here), Math.max(previous, here) + 1);
        for (const id of range) {
          if (checkbox.checked) selected.add(id);
          else selected.delete(id);
        }
        for (const control of list.querySelectorAll("[data-pool-id]")) {
          control.checked = selected.has(control.dataset.poolId);
        }
        anchor = pool.id;
        selection();
      });
      const info = element("div", null, "key-meta");
      const health = { active: "Healthy", error: "Failed test" }[pool.testStatus] || "Untested";
      const bound = Number(pool.boundConnectionCount) || 0;
      const state = [kinds.get(pool.type) || "HTTP proxy", active(pool) ? "Active" : "Disabled",
        health, `${bound} bound`, pool.strictProxy ? "Strict" : ""].filter(Boolean).join(" · ");
      info.append(element("h3", pool.name || "Proxy pool"), element("code", host(pool.proxyUrl)),
        element("span", state, "key-state"));
      if (pool.noProxy) info.append(element("span", `No proxy: ${pool.noProxy}`, "muted"));
      const testedAt = Date.parse(pool.lastTestedAt);
      if (Number.isFinite(testedAt)) {
        const lastError = typeof pool.lastError === "string"
          ? pool.lastError.replace(/(\w+:\/\/)[^\s/@]+@/g, "$1") : "";
        info.append(element("span", [
          `Last tested ${new Date(testedAt).toLocaleString()}`, lastError,
        ].filter(Boolean).join(" · "), "muted"));
      }
      const actions = element("div", null, "key-actions");
      actions.append(
        action("Edit", () => configure(pool)),
        action(active(pool) ? "Disable" : "Enable", (event) => work(event.currentTarget, async () => {
          await request(path(pool.id), "PUT", { isActive: !active(pool) });
          await load();
        })),
        action("Test", (event) => work(event.currentTarget, async () => {
          const data = await request(`${path(pool.id)}/test`, "POST");
          await load();
          const elapsed = data.elapsedMs > 0 ? ` (${data.elapsedMs} ms)` : "";
          notice(status, `Proxy test ${data.ok ? "passed" : "failed"}${elapsed}.`, !data.ok);
        })),
        action("Delete", (event) => {
          if (!window.confirm(`Delete proxy pool "${pool.name}"?`)) return;
          work(event.currentTarget, async () => {
            await request(path(pool.id), "DELETE");
            await load();
            notice(status, "Proxy pool deleted.");
          });
        }, "button-danger"),
      );
      card.append(checkbox, info, actions);
      return card;
    }

    function draw() {
      const rows = visible();
      list.replaceChildren(...(rows.length ? rows.map(row) : [element("p", "No matching pools.", "muted")]));
      selection();
    }

    async function load() {
      const { proxyPools } = await request(`${api}?includeUsage=true`);
      pools = proxyPools ?? [];
      const present = new Set(pools.map((pool) => pool.id));
      for (const id of selected) {
        if (!present.has(id)) selected.delete(id);
      }
      draw();
    }

    function configure(pool) {
      current = pool;
      edit.reset();
      select("pool-edit-title").textContent = pool ? "Edit proxy pool" : "New proxy pool";
      select("pool-name").value = pool?.name ?? "";
      type.value = kinds.has(pool?.type) ? pool.type : "http";
      poolUrl.required = !pool;
      poolUrl.placeholder = pool ? "Leave blank to keep the stored URL" : "https://proxy.example:8080";
      select("pool-no-proxy").value = pool?.noProxy ?? "";
      select("pool-active").checked = pool?.isActive !== false;
      select("pool-strict").checked = pool?.strictProxy === true;
      notice(select("pool-error"));
      open("pool-dialog");
    }

    select("pool-new").addEventListener("click", () => configure());
    form("pool-form", async () => {
      const name = select("pool-name").value.trim();
      const proxyUrl = poolUrl.value.trim();
      if (!name || (!current && !proxyUrl)) throw new Error("Enter a name and proxy URL.");
      const body = {
        name,
        type: type.value,
        noProxy: select("pool-no-proxy").value.trim(),
        isActive: select("pool-active").checked,
        strictProxy: select("pool-strict").checked,
      };
      if (proxyUrl) body.proxyUrl = proxyUrl;
      await request(current ? path(current.id) : api, current ? "PUT" : "POST", body);
      poolUrl.value = "";
      close("pool-dialog");
      await load();
      notice(status, "Proxy pool saved.");
    }, select("pool-error"));
    select("pool-dialog").addEventListener("close", () => { poolUrl.value = ""; });

    for (const [kind, label, accountField, tokenField, missing] of [
      ["cloudflare", "Cloudflare", "accountId", "apiToken", "Enter a Cloudflare account ID and API token."],
      ["deno", "Deno", "orgDomain", "denoToken", "Enter a Deno organization and API token."],
      ["netlify", "Netlify", null, "netlifyToken", "Enter a Netlify API token."],
      ["vercel", "Vercel", "teamId", "vercelToken", "Enter a Vercel API token."],
    ]) {
      const prefix = `pool-${kind}`;
      const formId = `${prefix}-form`;
      const dialogId = `${prefix}-dialog`;
      const token = select(`${prefix}-token`);
      const accountFieldInput = accountField ? select(`${prefix}-account`) : null;
      const deployError = select(`${prefix}-error`);
      select(`${prefix}-deploy`).addEventListener("click", () => {
        select(formId).reset();
        notice(deployError);
        open(dialogId);
      });
      select(dialogId).addEventListener("close", () => { token.value = ""; });
      form(formId, async () => {
        const account = accountFieldInput?.value.trim();
        const apiToken = token.value.trim();
        const projectName = select(`${prefix}-name`).value.trim();
        if (!apiToken || (accountFieldInput?.required && !account)) throw new Error(missing);
        token.value = "";
        try {
          await request(`${api}/${kind}-deploy`, "POST", {
            ...(account ? { [accountField]: account } : {}),
            [tokenField]: apiToken,
            projectName,
          });
        } catch (error) {
          const message = typeof error?.message === "string"
            ? error.message.replaceAll(apiToken, "[redacted]") : `${label} deployment failed.`;
          throw Object.assign(new Error(message), { status: error?.status });
        }
        close(dialogId);
        await load();
        notice(status, `${label} relay deployed.`);
      }, deployError);
    }

    search.addEventListener("input", draw);
    function clearPicked() {
      selected.clear();
      anchor = undefined;
      draw();
    }
    filter.addEventListener("change", clearPicked);
    clearSelection.addEventListener("click", clearPicked);
    selectAll.addEventListener("change", () => {
      for (const pool of visible()) {
        if (selectAll.checked) selected.add(pool.id);
        else selected.delete(pool.id);
      }
      anchor = undefined;
      draw();
    });

    function targetsForBatch(deletion = false) {
      const rows = visible();
      const chosen = picked(rows);
      return deletion || chosen.length ? chosen : rows;
    }

    function bulk(control, operation) {
      if (bulkBusy) return;
      work(control, async () => {
        bulkBusy = true;
        selection();
        try { await operation(); }
        finally { bulkBusy = false; }
      }).then(selection);
    }

    function batch(control, operation, deletion = false) {
      const targets = targetsForBatch(deletion);
      if (!targets.length || deletion && !window.confirm(`Delete ${targets.length} selected proxy pools?`)) return;
      bulk(control, async () => {
        let successes = 0;
        let failures = 0;
        let bound = 0;
        for (let offset = 0; offset < targets.length; offset += 8) {
          const results = await Promise.allSettled(targets.slice(offset, offset + 8).map(operation));
          for (const result of results) {
            if (result.status === "fulfilled" && result.value?.ok !== false) successes++;
            else if (deletion && result.reason?.status === 409) bound++;
            else failures++;
          }
        }
        if (deletion) selected.clear();
        await load();
        notice(status, `${successes} succeeded · ${failures} failed${bound ? ` · ${bound} still bound` : ""}.`,
          failures > 0 || bound > 0);
      });
    }

    select("pool-enable").addEventListener("click", (event) => batch(event.currentTarget,
      (pool) => request(path(pool.id), "PUT", { isActive: true })));
    select("pool-disable").addEventListener("click", (event) => batch(event.currentTarget,
      (pool) => request(path(pool.id), "PUT", { isActive: false })));
    healthButton.addEventListener("click", () => {
      const targets = targetsForBatch();
      if (!targets.length) return;
      bulk(healthButton, async () => {
        let next = 0;
        let done = 0;
        const failed = new Set();
        async function probe() {
          while (next < targets.length) {
            const { id } = targets[next++];
            try {
              const result = await request(`${path(id)}/test`, "POST");
              if (!result.ok) failed.add(id);
            } catch {
              failed.add(id);
            } finally {
              healthButton.textContent = `Checking ${++done}/${targets.length}`;
            }
          }
        }
        healthButton.textContent = `Checking 0/${targets.length}`;
        try {
          await Promise.all(Array.from({ length: Math.min(10, targets.length) }, probe));
          await load();
          const summary = `${targets.length - failed.size} healthy · ${failed.size} failed`;
          const remaining = pools.filter(pool => failed.has(pool.id) && active(pool));
          if (!remaining.length || !window.confirm(`${summary}. Disable ${remaining.length} failed pools still active?`)) {
            notice(status, `${summary}.`, failed.size > 0);
            return;
          }
          const results = await Promise.allSettled(remaining.map(pool => request(path(pool.id), "PUT", { isActive: false })));
          const disabled = results.filter(result => result.status === "fulfilled").length;
          await load();
          notice(status, `${summary} · ${disabled}/${remaining.length} disabled.`, disabled < remaining.length);
        } finally {
          healthButton.textContent = "Test health";
        }
      });
    });
    select("pool-delete-selected").addEventListener("click", (event) => batch(event.currentTarget,
      (pool) => request(path(pool.id), "DELETE"), true));

    const importText = select("pool-import-text");
    select("pool-import").addEventListener("click", () => {
      importText.value = "";
      notice(select("pool-import-error"));
      open("pool-import-dialog");
    });
    select("pool-import-dialog").addEventListener("close", () => { importText.value = ""; });
    form("pool-import-form", async () => {
      const lines = importText.value.split(/\r?\n/).map((line) => line.trim()).filter(Boolean);
      const entries = lines.map((line, index) => {
        try {
          if (line.includes("://")) return new URL(line);
          const parts = line.split(":");
          if (parts.length !== 4 || !parts.every(Boolean)) throw new Error();
          const [host, port, username, password] = parts;
          return new URL(`http://${encodeURIComponent(username)}:${encodeURIComponent(password)}@${host}:${port}`);
        } catch {
          throw new Error(`Invalid proxy format on line ${index + 1}.`);
        }
      });
      if (!entries.length) throw new Error("Enter at least one proxy.");
      const existing = new Set(pools.filter((pool) => !pool.noProxy?.trim()).map((pool) => pool.proxyUrl?.trim()));
      let created = 0;
      let skipped = 0;
      let failed = 0;
      for (const proxy of entries) {
        const proxyUrl = proxy.toString();
        if (existing.has(proxyUrl)) {
          skipped++;
          continue;
        }
        try {
          await request(api, "POST", { type: "http", proxyUrl, name: `Imported ${proxy.host}`, isActive: true });
          existing.add(proxyUrl);
          created++;
        } catch {
          failed++;
        }
      }
      close("pool-import-dialog");
      await load();
      notice(status, `${created} imported · ${skipped} skipped · ${failed} failed.`, failed > 0);
    }, select("pool-import-error"));

    load().catch((error) => fail(error, status));
  }

  function tokenSaver() {
    const api = "/api/settings";
    const forms = [...document.querySelectorAll("[data-settings-form]")];
    const values = (target) => [...target.querySelectorAll("[data-setting]")];
    const defaults = {
      rtkEnabled: true,
      headroomUrl: "http://localhost:8787",
      headroomTimeoutMs: 3000,
      headroomKompress: true,
      cavemanLevel: "full",
      ponytailLevel: "full",
      pxpipeMinChars: 25000,
    };
    const headroomConfig = (settings) => ({
      url: settings.headroomUrl || defaults.headroomUrl,
      codeAware: settings.headroomCodeAware === true,
      kompress: settings.headroomKompress !== false,
    });
    let savedHeadroom;

    function apply(target, settings) {
      for (const control of values(target)) {
        const field = control.dataset.setting;
        const value = settings[field] ?? defaults[field];
        if (control.type === "checkbox") control.checked = value === true;
        else control.value = value ?? "";
      }
    }

    request(api).then((settings) => {
      savedHeadroom = headroomConfig(settings);
      for (const target of forms) {
        apply(target, settings);
        target.querySelector("fieldset").disabled = false;
      }
    }).catch((error) => fail(error, status));

    for (const target of forms) {
      target.addEventListener("submit", (event) => {
        event.preventDefault();
        work(target.querySelector('[type="submit"]'), async () => {
          const patch = {};
          for (const control of values(target)) {
            const field = control.dataset.setting;
            if (control.type === "checkbox") patch[field] = control.checked;
            else if (control.type === "number") patch[field] = control.valueAsNumber;
            else if (field === "headroomUrl") patch[field] = control.value.trim() || defaults[field];
            else patch[field] = control.value;
          }
          const saved = await request(api, "PATCH", patch);
          apply(target, saved);
          const restarted = Object.hasOwn(patch, "headroomUrl") && await applyHeadroom(saved);
          notice(status, restarted ? "Settings saved. Headroom restarted." : "Settings saved.");
        });
      });
    }

    async function applyHeadroom(saved) {
      const before = savedHeadroom;
      const next = headroomConfig(saved);
      savedHeadroom = next;
      let restarting = false;
      try {
        const current = await refreshHeadroom();
        const local = ["localhost", "127.0.0.1", "[::1]"].includes(window.location.hostname);
        const flagsChanged = before && (before.codeAware !== next.codeAware || before.kompress !== next.kompress);
        if (!local || !flagsChanged || before.url !== next.url || current.localUrl !== true
          || current.running !== true || !current.managedPid) return false;
        restarting = true;
        notice(status, "Settings saved. Restarting Headroom…");
        const result = await request("/api/headroom/restart", "POST");
        const after = await refreshHeadroom();
        if (result.success !== true || after.running !== true || after.managedPid !== result.pid) {
          throw new Error("The restarted proxy did not report ready.");
        }
        return true;
      } catch (error) {
        if (restarting) await refreshHeadroom().catch(() => {});
        if (error.status === 401) throw error;
        const operation = restarting ? "Headroom restart could not be confirmed." : "Headroom status could not be read.";
        throw new Error(`Settings saved. ${operation} ${error.message}`);
      }
    }

    async function refreshHeadroom() {
      const data = await request("/api/headroom/status");
      const output = select("headroom-result");
      const local = data.localUrl !== false;
      const managed = local && !!data.managedPid;
      const canStart = local && !data.running && data.canStart === true;
      const install = local && !data.running && !canStart && !data.installed && !!data.python;
      const hint = select("headroom-setup-hint");
      output.textContent = data.running ? "Running" : !local ? "External" : data.installed ? "Stopped" : "Not installed";
      output.hidden = false;
      select("headroom-dashboard").hidden = !data.running;
      document.querySelector('[data-headroom="start"]').hidden = !canStart;
      document.querySelector('[data-headroom="stop"]').hidden = !managed;
      select("headroom-install").hidden = !install;
      hint.textContent = data.running || canStart || install ? ""
        : !local || data.installed ? "Start Headroom at the configured URL, then check its status."
        : "Local Headroom requires Python 3.10 or newer. You can also configure an external URL.";
      hint.hidden = !hint.textContent;
      return data;
    }

    select("headroom-status").addEventListener("click", (event) => work(event.currentTarget, refreshHeadroom));
    select("headroom-install-copy").addEventListener("click", (event) => work(event.currentTarget, async () => {
      await navigator.clipboard.writeText(select("headroom-install-command").textContent);
      notice(status, "Install command copied.");
    }));
    refreshHeadroom().catch((error) => fail(error, status));
    for (const control of document.querySelectorAll("[data-headroom]")) {
      control.addEventListener("click", () => work(control, async () => {
        await request(`/api/headroom/${control.dataset.headroom}`, "POST");
        await refreshHeadroom();
      }));
    }

    const extrasApi = "/api/headroom/extras";
    const extrasResult = select("headroom-extras-result");
    const extrasLog = select("headroom-extras-log");
    const changes = [...document.querySelectorAll("[data-headroom-change]")];
    function showExtras(data) {
      const installed = (data.available ?? ["code", "ml"]).filter(name => data.extras?.[name] === true);
      extrasResult.textContent = data.installed
        ? `Installed extras: ${installed.join(", ") || "none"}` : "Headroom package is unavailable.";
      extrasResult.hidden = false;
    }
    async function refreshExtrasLog() {
      const result = await request(`${extrasApi}?log=1`);
      extrasLog.textContent = result.log || "";
      extrasLog.hidden = !result.log;
    }
    select("headroom-extras-refresh").addEventListener("click", event => work(event.currentTarget, async () => {
      showExtras(await request(extrasApi));
    }));
    for (const button of changes) {
      button.addEventListener("click", () => work(button, async () => {
        const extra = [...document.querySelectorAll("[data-headroom-extra]:checked")].map(input => input.value);
        if (!extra.length) throw new Error("Select an extra.");
        for (const other of changes) other.disabled = true;
        const live = window.setInterval(() => refreshExtrasLog().catch(() => {}), 1000);
        try {
          showExtras(await request(extrasApi, button.dataset.headroomChange, { extras: extra }));
        } finally {
          clearInterval(live);
          await refreshExtrasLog().catch(() => {});
          for (const other of changes) other.disabled = false;
        }
      }));
    }
  }

  if (document.body.dataset.page === "proxy-pools") proxyPools();
  if (document.body.dataset.page === "token-saver") tokenSaver();
})();
