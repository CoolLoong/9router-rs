(() => {
  "use strict";

  const { select, request, notice } = window.nineRouterUi;
  const base = "/api/translator";
  const status = select("translator-status");
  const provider = select("translator-provider");
  const model = select("translator-model");
  const formats = select("translator-formats");
  const stop = select("translator-stop");
  const stage = (number) => select(`translator-stage-${number}`);
  const editor = (panel) => panel.querySelector("textarea");
  const read = (panel) => editor(panel).value;
  const json = (value) => JSON.stringify(value, null, 2);
  let sending;

  function fail(error) {
    if (error.status === 401) window.location.replace("/login");
    else notice(status, error.message, true);
  }

  function fill(panel, content) {
    editor(panel).value = content;
    panel.open = true;
  }

  function next(panel, content) {
    fill(panel, content);
    editor(panel).focus();
  }

  function value(panel) {
    try {
      return JSON.parse(read(panel));
    } catch {
      throw new Error("Enter valid JSON.");
    }
  }

  function save(panel, content) {
    return request(`${base}/save`, "POST", { file: panel.dataset.file, content });
  }

  async function translate(step, body) {
    const result = await request(`${base}/translate`, "POST", { step, body });
    return result.result;
  }

  async function detect(body) {
    const info = await translate(1, body);
    provider.value = info.provider ?? "";
    model.value = info.model ?? "";
    formats.textContent = `${info.sourceFormat} → ${info.targetFormat}`;
    formats.hidden = false;
  }

  async function load(panel) {
    const address = (file) => `${base}/load?${new URLSearchParams({ file })}`;
    let result;
    try {
      result = await request(address(panel.dataset.file));
    } catch (error) {
      if (error.status !== 404 || panel !== stage(7)) throw error;
      result = await request(address("7_res_client.json"));
    }
    fill(panel, result.content);
    if (panel === stage(4)) {
      try {
        const target = JSON.parse(result.content);
        if (target.provider) provider.value = target.provider;
        if (target.model) model.value = target.model;
      } catch {}
    }
  }

  async function intermediate(panel) {
    const body = value(panel);
    await detect(body);
    const converted = await translate(2, body);
    const original = body.body || body;
    const source = json({ timestamp: new Date().toISOString(), headers: {}, body: original });
    const result = json(converted.body);
    fill(stage(2), source);
    next(stage(3), result);
    await Promise.all([
      save(panel, read(panel)),
      save(stage(2), source),
      save(stage(3), result),
    ]);
  }

  async function build(panel) {
    const body = value(panel);
    const name = provider.value.trim();
    const target = model.value.trim();
    if (!name || !target) throw new Error("Select a provider and model.");
    const preview = await translate(3, { provider: name, model: target, body: { ...body, model: target } });
    const requestText = json({ ...preview, provider: name, model: target });
    next(stage(4), requestText);
    await Promise.all([save(panel, read(panel)), save(stage(4), requestText)]);
  }

  async function send(panel) {
    if (sending) return;
    const preview = value(panel);
    const name = preview.provider || provider.value.trim();
    const target = preview.model || model.value.trim();
    if (!name || !target) throw new Error("Select a provider and model.");
    const controller = new AbortController();
    sending = controller;
    stop.hidden = false;
    try {
      await save(panel, read(panel)).catch(() => {});
      const response = await fetch(`${base}/send`, {
        method: "POST",
        cache: "no-store",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ provider: name, model: target, body: preview.body || preview }),
        signal: controller.signal,
      });
      if (!response.ok) {
        const problem = await response.json().catch(() => ({}));
        throw new Error(problem.error || `Request failed (${response.status}).`);
      }
      const content = await response.text();
      next(stage(5), content);
      await save(stage(5), content);
    } catch (error) {
      if (controller.signal.aborted) notice(status, "Request stopped.");
      else throw error;
    } finally {
      sending = undefined;
      stop.hidden = true;
    }
  }

  const operations = {
    load,
    save: async (panel) => {
      await save(panel, read(panel));
      notice(status, "Saved.");
    },
    format: (panel) => fill(panel, json(value(panel))),
    copy: async (panel) => {
      await navigator.clipboard.writeText(read(panel));
      notice(status, "Copied.");
    },
    detect: (panel) => detect(value(panel)),
    intermediate,
    build,
    send,
  };
  stop.addEventListener("click", () => sending?.abort());
  for (const button of document.querySelectorAll("[data-operation]")) {
    button.addEventListener("click", async () => {
      button.disabled = true;
      notice(status);
      try {
        await operations[button.dataset.operation](button.closest(".translator-stage"));
      } catch (error) {
        fail(error);
      } finally {
        button.disabled = false;
      }
    });
  }
})();
