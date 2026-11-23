(() => {
  "use strict";

  const { select, request, notice, element, action } = window.nineRouterUi;
  const status = select("usage-status");
  const number = new Intl.NumberFormat();
  const currency = new Intl.NumberFormat(undefined, {
    style: "currency", currency: "USD", maximumFractionDigits: 6,
  });
  const date = new Intl.DateTimeFormat(undefined, { dateStyle: "short", timeStyle: "medium" });
  const view = new URLSearchParams(window.location.search).get("tab");
  const tab = ["details", "logs"].includes(view) ? view : "overview";
  const api = "/api/usage";

  const count = (value) => number.format(Number(value) || 0);
  const cost = (value) => currency.format(Number(value) || 0);
  const timestamp = (value) => {
    if (!value) return "—";
    const time = new Date(value);
    return Number.isNaN(time.getTime()) ? String(value) : date.format(time);
  };

  function failed(error) {
    if (error.status === 401) window.location.replace("/login");
    else notice(status, error.message, true);
  }

  function row(...columns) {
    const entry = element("tr");
    for (const value of columns) {
      const cell = element("td");
      if (value instanceof Node) cell.append(value);
      else cell.textContent = value ?? "—";
      entry.append(cell);
    }
    return entry;
  }

  function table(id, rows, width, empty) {
    const target = select(id);
    if (rows.length) target.replaceChildren(...rows);
    else {
      const fallback = row(empty);
      fallback.firstElementChild.colSpan = width;
      target.replaceChildren(fallback);
    }
  }

  function recent(entries) {
    table("usage-recent", (entries ?? []).map((record) => row(
      timestamp(record.timestamp),
      record.provider,
      record.model,
      count(record.promptTokens),
      count(record.completionTokens),
      record.status,
    )), 6, "No recent requests.");
  }

  function overview() {
    const period = select("usage-period");
    const group = select("usage-group");
    const breakdownMetric = select("usage-breakdown-metric");
    const breakdownSort = select("usage-breakdown-sort");
    const direction = select("usage-breakdown-direction");
    const refresh = select("usage-refresh");
    const trendMetric = select("usage-trend-metric");
    const providerMetric = select("usage-provider-rank-metric");
    const modelMetric = select("usage-model-rank-metric");
    const providerActivity = select("usage-activity-providers");
    const providers = new Map([...providerActivity.querySelectorAll("[data-provider]")]
      .map(item => [item.dataset.provider.toLowerCase(), item.textContent]));
    let loaded;
    let activity;
    let series = [];
    let pending = 0;
    let descending = true;
    const expansionKeys = {
      byModel: "usage-stats:expanded-models",
      byAccount: "usage-stats:expanded-accounts",
      byApiKey: "usage-stats:expanded-apikeys",
      byEndpoint: "usage-stats:expanded-endpoints",
    };

    function expansion(key) {
      try {
        const values = JSON.parse(localStorage.getItem(key));
        if (Array.isArray(values)) return new Set(values);
      } catch {}
      return new Set();
    }

    function grouped() {
      const key = group.value;
      const showCost = breakdownMetric.value === "cost";
      const format = showCost ? cost : count;
      const valueFields = ["requests", "input", "cached", "output", "total"];
      const storageKey = expansionKeys[key];
      const expanded = storageKey ? expansion(storageKey) : new Set();
      for (const label of ["input", "cached", "output", "total"]) {
        select(`usage-breakdown-${label}`).textContent = `${label[0].toUpperCase()}${label.slice(1)} ${showCost ? `cost${label === "total" ? "" : " (est.)"}` : "tokens"}`;
      }
      const descriptor = {
        byAccount: value => [value.connectionId || value.accountName, value.accountName || "Unknown connection"],
        byModel: value => [value.rawModel, value.rawModel || "Unknown model"],
        byApiKey: value => {
          const name = [value.keyName || "Local", value.apiKeyMasked].filter(Boolean).join(" · ");
          return [name, name];
        },
        byEndpoint: value => [value.endpoint, value.endpoint || "Unknown endpoint"],
      };
      const groups = new Map();
      for (const [id, entry] of Object.entries(loaded?.[key] ?? {})) {
        const [groupId, name] = descriptor[key]?.(entry) ?? [id, id];
        const prompt = Number(entry.promptTokens) || 0;
        const output = Number(entry.completionTokens) || 0;
        const cached = Number(entry.cachedTokens) || 0;
        const tokens = prompt + output;
        const totalCost = Number(entry.cost) || 0;
        const unitCost = tokens > 0 ? totalCost / tokens : 0;
        const item = {
          name: (key === "byModel" ? entry.provider : [entry.rawModel, entry.provider].filter(Boolean).join(" · ")) || name,
          requests: Number(entry.requests) || 0,
          lastUsed: Date.parse(entry.lastUsed) || 0,
          input: showCost ? Math.max(0, prompt - cached) * unitCost : prompt,
          cached: showCost ? cached * unitCost : cached,
          output: showCost ? output * unitCost : output,
          total: showCost ? totalCost : tokens,
        };
        const idKey = groupId || name;
        if (!groups.has(idKey)) groups.set(idKey, {
          name, requests: 0, lastUsed: 0, input: 0, cached: 0, output: 0, total: 0, items: [],
        });
        const grouped = groups.get(idKey);
        for (const field of valueFields) grouped[field] += item[field];
        grouped.lastUsed = Math.max(grouped.lastUsed, item.lastUsed);
        grouped.items.push(item);
      }
      const sortBy = breakdownSort.value;
      const compare = (left, right) => (descending ? -1 : 1) * (sortBy === "group"
        ? left.name.localeCompare(right.name) : left[sortBy] - right[sortBy]);
      const cells = entry => [count(entry.requests), timestamp(entry.lastUsed),
        format(entry.input), format(entry.cached), format(entry.output), format(entry.total)];
      const body = [];
      for (const [id, grouped] of [...groups].sort(([, left], [, right]) => compare(left, right))) {
        if (!storageKey) {
          body.push(row(grouped.name, ...cells(grouped)));
          continue;
        }
        const toggle = element("button", grouped.name, "button");
        toggle.type = "button";
        if (expanded.has(grouped.name)) expanded.add(id);
        const updateToggle = () => {
          const open = expanded.has(id);
          toggle.textContent = `${open ? "▾" : "▸"} ${grouped.name}`;
          toggle.setAttribute("aria-expanded", String(open));
        };
        updateToggle();
        const summary = row(toggle, ...cells(grouped));
        const detailRows = () => grouped.items.sort(compare).map(item => {
          const detail = row(item.name, ...cells(item));
          detail.classList.add("usage-breakdown-detail");
          return detail;
        });
        let details = expanded.has(id) ? detailRows() : [];
        body.push(summary, ...details);
        toggle.addEventListener("click", () => {
          if (expanded.delete(id)) {
            expanded.delete(grouped.name);
            for (const detail of details) detail.remove();
            details = [];
          } else {
            expanded.add(id);
            details = detailRows();
            summary.after(...details);
          }
          updateToggle();
          try { localStorage.setItem(storageKey, JSON.stringify([...expanded])); } catch {}
        });
      }
      table("usage-groups", body, 7, "No usage for this period.");
    }

    function measure(value, maximum, label, format) {
      const content = element("div", null, "usage-meter");
      const meter = element("meter");
      meter.max = maximum || 1;
      meter.value = value;
      meter.setAttribute("aria-label", label);
      content.append(meter, element("span", format(value)));
      return content;
    }

    function trend() {
      const selected = trendMetric.value;
      const metric = [["tokens", "Tokens", count], ["requests", "Requests", count], ["cost", "Cost", cost]];
      const peak = series.reduce((max, item) => Math.max(max, Number(item[selected]) || 0), 0);
      table("usage-trend", series.map(item => row(item.label, ...metric.map(([key, label, format]) => {
        const value = Number(item[key]) || 0;
        return key === selected ? measure(value, peak, label, format) : format(value);
      }))), 4, "No usage for this period.");
    }

    function ranks() {
      for (const [id, key, metric, limit] of [
        ["usage-provider-rank", "byProvider", providerMetric, undefined],
        ["usage-model-rank", "byModel", modelMetric, 5],
      ]) {
        const byRequests = metric.value === "requests";
        const items = Object.entries(loaded?.[key] ?? {}).map(([id, value]) => ({
          name: key === "byModel" ? value.rawModel || id : providers.get(id.toLowerCase()) || id,
          amount: byRequests ? Number(value.requests) || 0
            : (Number(value.promptTokens) || 0) + (Number(value.completionTokens) || 0),
        })).filter(item => item.amount > 0).sort((left, right) => right.amount - left.amount).slice(0, limit);
        const max = items[0]?.amount;
        table(id, items.map(({ name, amount }) => row(name, measure(amount, max, byRequests ? "Requests" : "Tokens", count))),
          2, "No usage for this period.");
      }
    }

    function showActivity(snapshot) {
      const active = new Map();
      for (const request of snapshot.activeRequests ?? []) {
        const id = request.provider?.toLowerCase();
        if (!id) continue;
        active.set(id, (active.get(id) ?? 0) + (Number(request.count) || 0));
      }
      const last = snapshot.recentRequests?.[0]?.provider?.toLowerCase();
      const error = snapshot.errorProvider?.toLowerCase();
      const ids = new Set([...providers.keys(), ...active.keys(), last, error]);
      const visible = [...ids].filter(Boolean);
      providerActivity.replaceChildren(...(visible.length ? visible.map(id => {
        const item = element("li", null, "key-row");
        const tags = [];
        if (active.get(id)) tags.push(`${count(active.get(id))} active`);
        if (id === last) tags.push("Last used");
        if (id === error) tags.push("Recent error");
        item.append(element("strong", providers.get(id) || id), element("span", tags.join(" · ") || "Idle", "muted"));
        return item;
      }) : [element("li", "No provider activity.", "muted")]));
    }

    async function load() {
      const current = ++pending;
      refresh.disabled = true;
      const query = new URLSearchParams({ period: period.value });
      try {
        const [summary, history] = await Promise.all([
          request(`${api}/stats?${query}`),
          request(`${api}/chart?${query}`),
        ]);
        if (current !== pending) return;
        loaded = summary;
        series = Array.isArray(history) ? history : [];
        const totals = {
          "usage-count": "totalRequests",
          "usage-prompt": "totalPromptTokens",
          "usage-completion": "totalCompletionTokens",
          "usage-cached": "totalCachedTokens",
        };
        for (const [id, key] of Object.entries(totals)) select(id).textContent = count(summary[key]);
        select("usage-cost").textContent = cost(summary.totalCost);
        trend();
        ranks();
        grouped();
        recent(activity?.recentRequests ?? summary.recentRequests);
        showActivity(activity ?? summary);
        notice(status);
      } catch (error) {
        if (current === pending) failed(error);
      } finally {
        if (current === pending) refresh.disabled = false;
      }
    }

    period.addEventListener("change", load);
    group.addEventListener("change", grouped);
    breakdownMetric.addEventListener("change", grouped);
    breakdownSort.addEventListener("change", grouped);
    direction.addEventListener("click", () => {
      descending = !descending;
      direction.textContent = descending ? "Descending" : "Ascending";
      grouped();
    });
    trendMetric.addEventListener("change", trend);
    providerMetric.addEventListener("change", ranks);
    modelMetric.addEventListener("change", ranks);
    refresh.addEventListener("click", load);
    const events = new EventSource(`${api}/stream`);
    const inFlight = select("usage-active");
    events.addEventListener("message", (event) => {
      try {
        activity = JSON.parse(event.data);
      } catch {
        return;
      }
      if (Array.isArray(activity.recentRequests)) recent(activity.recentRequests);
      showActivity(activity);
      const running = (activity.activeRequests ?? []).map((entry) =>
        `${entry.model} · ${count(entry.count)} active`);
      inFlight.textContent = running.join(" / ");
      inFlight.hidden = !running.length;
    });
    load();
  }

  function details() {
    const provider = select("usage-provider");
    const model = select("usage-model");
    const start = select("usage-start");
    const end = select("usage-end");
    const filters = [provider, model, start, end];
    const filterForm = select("usage-filters");
    const clear = select("usage-clear-filters");
    const back = select("usage-prev");
    const next = select("usage-next");
    const size = select("usage-page-size");
    const links = select("usage-page-links");
    const dialog = select("usage-detail-dialog");
    const providerNames = new Map();
    let page = 1;
    let generation = 0;
    let filtered = false;
    let records = [];

    function syncClear() {
      clear.disabled = !filtered && filters.every((control) => !control.value.trim());
    }

    function tokenValues(record) {
      const tokens = record.tokens ?? {};
      const cached = Number(tokens.cached_tokens || tokens.cache_read_input_tokens) || 0;
      return {
        input: Math.max(Number(tokens.prompt_tokens || tokens.input_tokens) || 0, cached),
        cached,
        created: Number(tokens.cache_creation_input_tokens) || 0,
        output: Number(tokens.completion_tokens || tokens.output_tokens) || 0,
      };
    }

    function ms(value) {
      return value == null || !Number.isFinite(Number(value)) ? "—" : `${count(value)} ms`;
    }

    function readMetrics(id, values) {
      select(id).replaceChildren(...values.filter(([, value]) => value != null && value !== "").map(([name, value]) => {
        const item = element("div");
        item.append(element("dt", name), element("dd", String(value)));
        return item;
      }));
    }

    const providerLabel = id => providerNames.get(id) || id || "—";

    function open(record) {
      const tokens = tokenValues(record);
      readMetrics("usage-detail-metadata", [
        ["Request ID", record.id], ["Time", timestamp(record.timestamp)], ["Provider", providerLabel(record.provider)],
        ["Model", record.model], ["Status", record.status], ["Endpoint", record.endpoint], ["Connection", record.connectionId],
        ["Time to first token", ms(record.latency?.ttft)], ["Total duration", ms(record.latency?.total)],
        ["Input tokens", count(tokens.input)], ["Cached", tokens.cached ? count(tokens.cached) : null],
        ["Cache creation", tokens.created ? count(tokens.created) : null], ["Output tokens", count(tokens.output)],
        ["Cost", record.cost == null ? null : cost(record.cost)],
      ]);
      const pxpipe = record.pxpipe;
      select("usage-detail-pxpipe").hidden = !pxpipe;
      if (pxpipe) {
        readMetrics("usage-detail-compression", pxpipe.applied ? [
          ["Status", "Applied"], ["Original tokens (est.)", count(pxpipe.tokensBeforeEst)],
          ["Compressed tokens (est.)", count(pxpipe.tokensAfterEst)], ["Saved", `${count(pxpipe.savedPct)}%`],
          ["Images", count(pxpipe.imageCount)], ["Compression duration", ms(pxpipe.durationMs)],
        ] : [["Status", "Skipped"], ["Reason", pxpipe.reason], ["Details", pxpipe.detail]]);
      }
      const payloads = select("usage-detail-payloads");
      payloads.replaceChildren();
      let redacted = false;
      for (const [key, name] of [
        ["request", "Client request"], ["providerRequest", "Provider request"],
        ["providerResponse", "Provider response"], ["response", "Client response"],
      ]) {
        const value = record[key];
        if (value == null) continue;
        if (value.redacted === true) {
          redacted = true;
          continue;
        }
        const section = element("details");
        section.open = key === "request" || key === "response";
        section.append(element("summary", name), element("pre",
          typeof value === "string" ? value : JSON.stringify(value, null, 2), "usage-log"));
        payloads.append(section);
      }
      if (redacted) payloads.append(element("p", "Conversation payloads were redacted by the server.", "muted"));
      dialog.showModal();
      dialog.scrollTop = 0;
    }

    function pagination(data) {
      page = data.page;
      back.disabled = !data.hasPrev;
      next.disabled = !data.hasNext;
      select("usage-page").textContent = data.totalItems ? `Page ${page} of ${data.totalPages}` : "No results";
      const firstItem = (page - 1) * data.pageSize + 1;
      const lastItem = Math.min(page * data.pageSize, data.totalItems);
      select("usage-range").textContent = firstItem <= lastItem
        ? `Showing ${count(firstItem)} to ${count(lastItem)} of ${count(data.totalItems)} results` : "";
      links.replaceChildren();
      if (data.totalPages <= 1) return;

      function pageLink(index) {
        const chosen = index === page;
        const button = action(String(index), () => load(index), chosen ? "button-primary" : "");
        button.setAttribute("aria-label", `Page ${index}`);
        if (chosen) button.setAttribute("aria-current", "page");
        button.disabled = chosen;
        links.append(button);
      }

      const first = Math.max(1, Math.min(page - 2, data.totalPages - 4));
      const last = Math.min(data.totalPages, first + 4);
      if (first > 1) {
        pageLink(1);
        if (first > 2) links.append(element("span", "…", "muted"));
      }
      for (let index = first; index <= last; index++) pageLink(index);
      if (last < data.totalPages) {
        if (last < data.totalPages - 1) links.append(element("span", "…", "muted"));
        pageLink(data.totalPages);
      }
    }

    function renderDetails() {
      table("usage-details-rows", records.map((record) => {
        const tokens = tokenValues(record);
        return row(timestamp(record.timestamp), providerLabel(record.provider), record.model, record.status,
          count(tokens.input), tokens.cached ? count(tokens.cached) : "—", tokens.created ? count(tokens.created) : "—",
          count(tokens.output), `TTFT ${ms(record.latency?.ttft)} · Total ${ms(record.latency?.total)}`,
          action("View", () => open(record)));
      }), 10, "No matching requests.");
    }

    async function load(number) {
      const current = ++generation;
      try {
        const values = new URLSearchParams({ page: number, pageSize: size.value });
        for (const [key, value] of Object.entries({
          provider: provider.value,
          model: model.value.trim(),
          startDate: start.value ? new Date(start.value).toISOString() : "",
          endDate: end.value ? new Date(end.value).toISOString() : "",
        })) {
          if (value) values.set(key, value);
        }
        filtered = values.size > 2;
        syncClear();
        back.disabled = true;
        next.disabled = true;
        for (const button of links.querySelectorAll("button")) button.disabled = true;
        const data = await request(`${api}/request-details?${values}`);
        if (generation !== current) return;
        pagination(data.pagination);
        records = Array.isArray(data.details) ? data.details : [];
        renderDetails();
        notice(status);
      } catch (error) {
        if (generation === current) failed(error);
      }
    }

    request(`${api}/providers`).then(({ providers }) => {
      for (const entry of providers ?? []) {
        if (!entry.id) continue;
        providerNames.set(entry.id, entry.name || entry.id);
        const option = element("option", entry.name || entry.id);
        option.value = entry.id;
        provider.append(option);
      }
      if (records.length) renderDetails();
    }).catch(failed);
    filterForm.addEventListener("input", syncClear);
    filterForm.addEventListener("submit", (event) => {
      event.preventDefault();
      load(1);
    });
    clear.addEventListener("click", () => {
      if (clear.disabled) return;
      for (const control of filters) control.value = "";
      load(1);
    });
    size.addEventListener("change", () => load(1));
    back.addEventListener("click", () => load(page - 1));
    next.addEventListener("click", () => load(page + 1));
    load(1);
  }

  function logs() {
    const refresh = select("usage-log-refresh");
    async function load() {
      refresh.disabled = true;
      try {
        const lines = await request(`${api}/logs`);
        select("usage-log").textContent = lines.length ? lines.join("\n") : "No request logs.";
        notice(status);
      } catch (error) {
        failed(error);
      } finally {
        refresh.disabled = false;
      }
    }
    refresh.addEventListener("click", load);
    load();
  }

  for (const name of ["overview", "details", "logs"]) {
    select(`usage-${name}`).hidden = name !== tab;
  }
  select(`usage-tab-${tab}`).setAttribute("aria-current", "page");
  if (tab === "overview") overview();
  if (tab === "details") details();
  if (tab === "logs") logs();
})();
