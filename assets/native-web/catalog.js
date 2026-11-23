(() => {
  "use strict";

  const { select, request, notice, element, action } = window.nineRouterUi;
  const status = select("catalog-status");
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

  if (document.body.dataset.page === "models") modelManagement();
})();
