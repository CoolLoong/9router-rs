(() => {
  "use strict";

  const { select, request, notice, element, action } = window.nineRouterUi;
  const status = select("catalog-status");
  const lines = (value) => String(value).split(/\r?\n/).map((line) => line.trim()).filter(Boolean);
  const field = (name) => select(name).value.trim();
  const query = (route, parameters) => `${route}?${new URLSearchParams(parameters)}`;
  const submit = (event) => event.currentTarget.querySelector('button[type="submit"]');

  async function task(control, output, work) {
    if (control.disabled) return;
    control.disabled = true;
    notice(output);
    try {
      await work();
    } catch (error) {
      if (error.status === 401) window.location.replace("/login");
      else notice(output, error.message, true);
    } finally {
      control.disabled = false;
    }
  }

  function row(title, details, actions) {
    const node = element("article", null, "key-row");
    const info = element("div", null, "key-meta");
    info.append(element("h3", title));
    for (const text of details) info.append(element("code", text));
    const buttons = element("div", null, "key-actions");
    buttons.append(...actions);
    node.append(info, buttons);
    return node;
  }

  function entries(id, items, message) {
    select(id).replaceChildren(...(items.length ? items : [element("p", message, "muted")]));
  }

  for (const button of document.querySelectorAll("[data-catalog-close]")) {
    button.addEventListener("click", () => select(button.dataset.catalogClose).close());
  }

  function modelManagement() {
    const parameters = new URLSearchParams(window.location.search);
    const provider = parameters.get("provider") || "";
    const aliasPath = "/api/models/alias";
    const customPath = "/api/models/custom";
    const disabledPath = "/api/models/disabled";
    const customDialog = select("custom-model-dialog");
    const customForm = select("custom-model-form");
    const customError = select("custom-model-error");
    const customType = select("custom-type");
    let editing;

    select("disabled-provider").value = provider;

    function remove(label, path, confirmation, reload) {
      return action(label, (event) => {
        if (!confirmation || window.confirm(confirmation)) {
          void task(event.currentTarget, status, async () => {
            await request(path, "DELETE");
            await reload();
          });
        }
      }, confirmation ? "button-danger" : "");
    }

    async function loadAliases() {
      const { aliases } = await request(aliasPath);
      const items = Object.entries(aliases ?? {})
        .filter(([, model]) => !provider || String(model).startsWith(provider + "/"))
        .sort(([left], [right]) => left.localeCompare(right))
        .map(([alias, model]) => row(alias, [model], [
          action("Edit", () => {
            select("alias-name").value = alias;
            select("alias-model").value = model;
            select("alias-model").focus();
          }),
          remove("Delete", query(aliasPath, { alias }), "Delete this alias?", loadAliases),
        ]));
      entries("alias-list", items, "No model aliases.");
    }

    async function loadCustom() {
      const { models } = await request(customPath);
      const items = (models ?? []).filter((model) => model && (!provider || model.providerAlias === provider))
        .map((model) => row(model.name || model.id, [
          `${model.providerAlias}/${model.id}`,
          model.transport ? `${model.type} · ${model.transport}` : model.type,
        ], [
          action("Edit", () => openCustom(model)),
          remove("Delete", query(customPath, {
            providerAlias: model.providerAlias,
            id: model.id,
            type: model.type || "llm",
          }), "Delete this custom model?", loadCustom),
        ]));
      entries("custom-list", items, "No custom models.");
    }

    async function loadDisabled() {
      const { disabled } = await request(disabledPath);
      const items = Object.entries(disabled ?? {})
        .filter(([alias]) => !provider || alias === provider)
        .flatMap(([providerAlias, ids]) => !Array.isArray(ids) || ids.length === 0 ? [] : [
          row(providerAlias, ids, [
            remove("Enable all", query(disabledPath, { providerAlias }), "Enable every disabled model for this provider?", loadDisabled),
          ]),
          ...ids.map((id) => row(String(id), [providerAlias], [
            remove("Enable", query(disabledPath, { providerAlias, id }), "", loadDisabled),
          ])),
        ]);
      entries("disabled-list", items, "No disabled models.");
    }

    select("alias-form").addEventListener("submit", (event) => {
      event.preventDefault();
      void task(submit(event), status, async () => {
        const alias = field("alias-name");
        const model = field("alias-model");
        if (!alias || !model) throw new Error("Enter an alias and model ID.");
        await request(aliasPath, "PUT", { alias, model });
        select("alias-form").reset();
        await loadAliases();
        notice(status, "Alias saved.");
      });
    });

    select("disable-model-form").addEventListener("submit", (event) => {
      event.preventDefault();
      void task(submit(event), status, async () => {
        const providerAlias = field("disabled-provider");
        const id = field("disabled-id");
        if (!providerAlias || !id) throw new Error("Enter a provider alias and model ID.");
        await request(disabledPath, "POST", { providerAlias, ids: [id] });
        select("disabled-id").value = "";
        await loadDisabled();
        notice(status, "Model disabled.");
      });
    });

    function fieldsForType() {
      select("custom-transport-fields").hidden = customType.value !== "stt";
    }

    function openCustom(model) {
      editing = model;
      customForm.reset();
      select("custom-model-title").textContent = model ? "Edit custom model" : "Add custom model";
      select("custom-provider").value = model?.providerAlias ?? provider;
      select("custom-id").value = model?.id ?? "";
      select("custom-name").value = model?.name ?? "";
      const preferred = parameters.get("type");
      const kind = [...customType.options].some((option) => option.value === preferred) ? preferred : "llm";
      customType.value = model?.type ?? kind;
      select("custom-provider").disabled = Boolean(model);
      select("custom-id").disabled = Boolean(model);
      customType.disabled = Boolean(model);
      for (const name of ["vision", "reasoning"]) {
        const value = model?.caps?.[name];
        const control = select(`custom-${name}`);
        control.options[0].hidden = typeof value === "boolean";
        control.value = typeof value === "boolean" ? String(value) : "";
      }
      const transport = select("custom-transport");
      transport.options[0].hidden = model?.transport === "gemini-live";
      transport.value = model?.transport ?? "";
      fieldsForType();
      notice(customError);
      customDialog.showModal();
    }

    select("new-custom-model").addEventListener("click", () => openCustom());
    customType.addEventListener("change", fieldsForType);
    customForm.addEventListener("submit", (event) => {
      event.preventDefault();
      void task(submit(event), customError, async () => {
        const providerAlias = editing?.providerAlias ?? field("custom-provider");
        const id = editing?.id ?? field("custom-id");
        const type = editing?.type || customType.value;
        if (!providerAlias || !id) throw new Error("Enter a provider alias and model ID.");
        const caps = {};
        for (const name of ["vision", "reasoning"]) {
          const value = select(`custom-${name}`).value;
          if (value) caps[name] = value === "true";
        }
        const transport = type === "stt" ? field("custom-transport") : "";
        await request(customPath, "POST", {
          providerAlias, id, type, name: field("custom-name") || id, caps, transport,
        });
        customDialog.close();
        await loadCustom();
        notice(status, "Custom model saved.");
      });
    });

    if (provider && parameters.get("create") === "1") {
      openCustom();
      parameters.delete("create");
      window.history.replaceState(window.history.state, "", query(window.location.pathname, parameters) + window.location.hash);
    }

    Promise.all([loadAliases(), loadCustom(), loadDisabled()]).catch((error) => {
      if (error.status === 401) window.location.replace("/login");
      else notice(status, error.message, true);
    });
  }

  function comboManagement() {
    const comboPath = "/api/combos";
    const settingsPath = "/api/settings";
    const search = select("combo-search");
    const selectAll = select("combo-select-all");
    const deleteSelected = select("delete-selected");
    const strategySelected = select("strategy-selected");
    const comboDialog = select("combo-dialog");
    const comboError = select("combo-error");
    const strategyDialog = select("strategy-dialog");
    const strategyError = select("strategy-error");
    const strategyKind = select("strategy-kind");
    const strategyPolicy = select("strategy-policy");
    const selected = new Set();
    const strategyLabels = new Map([...strategyKind.options].map((option) => [option.value, option.textContent]));
    let combos = [];
    let settings = {};
    let editing;
    let strategyNames = [];

    function members(combo) {
      return Array.isArray(combo.models) ? combo.models : [];
    }

    function shown() {
      const term = search.value.trim().toLowerCase();
      return combos.filter((combo) => [combo.name, ...members(combo)]
        .some((text) => String(text).toLowerCase().includes(term)));
    }

    function draw() {
      const visible = shown();
      const visibleSelected = visible.filter((combo) => selected.has(combo.id)).length;
      selectAll.checked = visible.length > 0 && visibleSelected === visible.length;
      selectAll.indeterminate = visibleSelected > 0 && visibleSelected < visible.length;
      deleteSelected.disabled = selected.size === 0;
      strategySelected.disabled = selected.size === 0;
      deleteSelected.textContent = selected.size ? `Delete selected (${selected.size})` : "Delete selected";
      entries("combo-list", visible.map((combo) => {
        const configured = settings.comboStrategies?.[combo.name]?.fallbackStrategy;
        const name = strategyLabels.get(configured) || strategyLabels.get(settings.comboStrategy) || "Fallback";
        const checkbox = element("input");
        checkbox.type = "checkbox";
        checkbox.checked = selected.has(combo.id);
        checkbox.setAttribute("aria-label", `Select ${combo.name}`);
        checkbox.addEventListener("change", () => {
          if (checkbox.checked) selected.add(combo.id);
          else selected.delete(combo.id);
          draw();
        });
        const entry = row(combo.name, members(combo).length ? members(combo) : ["No models"], [
          action(name, () => openStrategy([combo.name])),
          action("Edit", () => openCombo(combo)),
          action("Delete", (event) => deleteCombos(event.currentTarget, [combo]), "button-danger"),
        ]);
        entry.prepend(checkbox);
        return entry;
      }), "No matching combos.");
    }

    async function loadCombos() {
      const { combos: list } = await request(comboPath);
      combos = (list ?? []).filter((combo) => !combo.kind || combo.kind === "llm");
      for (const id of selected) {
        if (!combos.some((combo) => combo.id === id)) selected.delete(id);
      }
      draw();
    }

    async function loadSettings() {
      settings = await request(settingsPath);
      draw();
      drawPools();
    }

    function openCombo(combo) {
      editing = combo;
      select("combo-dialog-title").textContent = combo ? "Edit combo" : "Create combo";
      select("combo-name").value = combo?.name ?? "";
      select("combo-models").value = combo ? members(combo).join("\n") : "";
      notice(comboError);
      comboDialog.showModal();
    }

    search.addEventListener("input", draw);
    selectAll.addEventListener("change", () => {
      for (const combo of shown()) {
        if (selectAll.checked) selected.add(combo.id);
        else selected.delete(combo.id);
      }
      draw();
    });
    select("new-combo").addEventListener("click", () => openCombo());
    select("combo-form").addEventListener("submit", (event) => {
      event.preventDefault();
      void task(submit(event), comboError, async () => {
        const name = field("combo-name");
        const models = lines(select("combo-models").value);
        const path = editing ? `${comboPath}/${encodeURIComponent(editing.id)}` : comboPath;
        await request(path, editing ? "PUT" : "POST", { name, models });
        comboDialog.close();
        await loadCombos();
        notice(status, "Combo saved.");
      });
    });

    async function deleteCombos(button, list) {
      const label = list.length === 1 ? `combo "${list[0].name}"` : `${list.length} selected combos`;
      if (!list.length || !window.confirm(`Delete ${label}?`)) return;
      await task(button, status, async () => {
        const results = await Promise.allSettled(list.map((combo) =>
          request(`${comboPath}/${encodeURIComponent(combo.id)}`, "DELETE")));
        const removed = list.filter((_, index) => results[index].status === "fulfilled");
        await loadCombos();
        const latest = await request(settingsPath);
        const strategies = latest.comboStrategies ?? {};
        if (removed.some((combo) => Object.hasOwn(strategies, combo.name))) {
          for (const combo of removed) delete strategies[combo.name];
          settings = await request(settingsPath, "PATCH", { comboStrategies: strategies });
        }
        const failed = results.find((result) => result.status === "rejected");
        if (failed) throw failed.reason;
        notice(status, `Deleted ${removed.length === 1 ? "combo" : removed.length + " combos"}.`);
      });
      draw();
    }

    deleteSelected.addEventListener("click", (event) => deleteCombos(event.currentTarget,
      combos.filter((combo) => selected.has(combo.id))));
    strategySelected.addEventListener("click", () => openStrategy(
      combos.filter((combo) => selected.has(combo.id)).map((combo) => combo.name)));

    function strategyFields() {
      const one = strategyNames.length === 1;
      select("strategy-smart").hidden = !one || strategyKind.value !== "smart";
      select("strategy-fusion").hidden = !one || strategyKind.value !== "fusion";
      select("strategy-threshold").disabled = strategyPolicy.value !== "custom";
    }

    function openStrategy(names) {
      strategyNames = names;
      const config = names.length === 1 ? settings.comboStrategies?.[names[0]] ?? {} : {};
      select("strategy-subject").textContent = names.length === 1 ? names[0] : `${names.length} selected combos`;
      strategyKind.value = config.fallbackStrategy || "fallback";
      select("strategy-router").value = config.routerModel || "";
      select("strategy-judge").value = config.judgeModel || "";
      strategyPolicy.value = config.smartPolicy || (config.sessionSticky === false ? "per-turn" : "smart");
      const threshold = Number(config.confidenceThreshold || .8);
      select("strategy-threshold").value = Math.round(threshold <= 1 ? threshold * 100 : threshold) || 80;
      strategyFields();
      notice(strategyError);
      strategyDialog.showModal();
    }

    strategyKind.addEventListener("change", strategyFields);
    strategyPolicy.addEventListener("change", strategyFields);
    select("strategy-form").addEventListener("submit", (event) => {
      event.preventDefault();
      void task(submit(event), strategyError, async () => {
        const names = [...strategyNames];
        const kind = strategyKind.value;
        const extra = {};
        if (names.length === 1 && kind === "smart") {
          const policy = strategyPolicy.value;
          const threshold = select("strategy-threshold").valueAsNumber;
          if (policy === "custom" && !Number.isFinite(threshold)) throw new Error("Enter a confidence threshold.");
          extra.routerModel = field("strategy-router");
          extra.smartPolicy = policy;
          extra.sessionSticky = policy !== "per-turn";
          extra.confidenceThreshold = policy === "custom" ? threshold / 100 : policy === "smart" ? .8 : 0;
        } else if (names.length === 1 && kind === "fusion") {
          extra.judgeModel = field("strategy-judge");
        }
        const latest = await request(settingsPath);
        const strategies = Object.assign(Object.create(null), latest.comboStrategies);
        for (const name of names) {
          if (kind === "fallback") delete strategies[name];
          else strategies[name] = { ...strategies[name], fallbackStrategy: kind, ...extra };
        }
        settings = await request(settingsPath, "PATCH", { comboStrategies: strategies });
        strategyDialog.close();
        draw();
        notice(status, "Routing strategy saved.");
      });
    });

    async function presets(button, source) {
      await task(button, status, async () => {
        const preview = await request(query(`${comboPath}/presets`, { source }));
        if (!preview.toCreate) {
          notice(status, "No new presets available.");
          return;
        }
        if (!window.confirm(`Create ${preview.toCreate} presets and skip ${preview.toSkip} existing combos?`)) return;
        const applied = await request(`${comboPath}/presets`, "POST", { source });
        await loadCombos();
        notice(status, `Created ${applied.createdCount} presets.`);
      });
    }

    select("cursor-presets").addEventListener("click", (event) => presets(event.currentTarget, "cursor"));
    select("claude-presets").addEventListener("click", (event) => presets(event.currentTarget, "claude"));

    function pool(kind, title) {
      const current = settings.capacityAdapter?.[kind];
      const config = Array.isArray(current) ? {
        models: current.map((entry) => entry?.model ?? entry).filter(Boolean),
      } : current ?? {};
      const form = element("form", null, "catalog-pool");
      form.method = "post";
      form.append(element("h3", title));
      const check = (text, checked) => {
        const label = element("label", text, "check-row");
        const input = element("input");
        input.type = "checkbox";
        input.checked = checked;
        label.prepend(input);
        form.append(label);
        return input;
      };
      const enabled = check("Enable this pool", config.enabled !== false);
      const round = check("Rotate models", Boolean(config.roundRobin));
      const label = element("label", "Ordered model IDs, one per line");
      const area = element("textarea");
      area.rows = 3;
      area.id = `pool-${kind}`;
      area.spellcheck = false;
      area.value = (Array.isArray(config.models) ? config.models : [])
        .map((model) => model === "oc/mimo-v2.5-free" ? "oc/mimo-v2.6-flash-free" : model).join("\n");
      label.htmlFor = area.id;
      const save = element("button", "Save pool", "button");
      save.type = "submit";
      form.append(label, area, save);
      form.addEventListener("submit", (event) => {
        event.preventDefault();
        void task(save, status, async () => {
          const next = { enabled: enabled.checked, roundRobin: round.checked, models: lines(area.value) };
          const latest = await request(settingsPath);
          settings = await request(settingsPath, "PATCH", {
            capacityAdapter: { ...latest.capacityAdapter, [kind]: next },
          });
          notice(status, "Fallback pool saved.");
        });
      });
      return form;
    }

    function drawPools() {
      select("adapter-pools").replaceChildren(pool("vision", "Images"), pool("audioInput", "Audio input"));
    }

    Promise.all([loadCombos(), loadSettings()]).catch((error) => {
      if (error.status === 401) window.location.replace("/login");
      else notice(status, error.message, true);
    });
  }

  if (document.body.dataset.page === "models") modelManagement();
  if (document.body.dataset.page === "combos") comboManagement();
})();
