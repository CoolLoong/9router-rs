(() => {
  "use strict";

  const { select, request, notice, element, action, cliEndpoints } = window.nineRouterUi;
  const status = select("integration-status");
  const providerApi = "/api/providers";
  const nodeApi = "/api/provider-nodes";
  const comboApi = "/api/combos";
  const mediaPath = "/dashboard/media-providers";
  const cliStatus = "/__9router-ui/cli-status";
  const url = (base, id) => `${base}/${encodeURIComponent(id)}`;
  const query = (base, values) => `${base}?${new URLSearchParams(values)}`;
  const field = (id) => select(id).value.trim();
  const comboPath = (kind) => `${mediaPath}/${["webSearch", "webFetch"].includes(kind) ? "web" : kind || "web"}`;
  const shellQuote = value => `'${String(value).replaceAll("'", "'\"'\"'")}'`;
  const tomlString = value => JSON.stringify(value.replace(/[\uD800-\uDFFF]/gu, "\uFFFD"))
    .replace(/\u007F/g, "\\u007F");

  function curlCommand(path, headers, body, multipart = false, output) {
    const lines = [`curl -sS -X POST ${shellQuote(new URL(path, window.location.origin).href)}`];
    for (const [name, value] of Object.entries(headers)) lines.push(`-H ${shellQuote(`${name}: ${value}`)}`);
    if (multipart) {
      for (const [name, value] of Object.entries(body)) lines.push(`--form-string ${shellQuote(`${name}=${value}`)}`);
      lines.push("--form 'file=@audio.wav'");
    } else {
      lines.push(`--data-binary ${shellQuote(JSON.stringify(body))}`);
    }
    if (output) lines.push(`--output ${shellQuote(output)}`);
    return lines.join(" \\\n  ");
  }

  function failed(error) {
    if (error.status === 401) window.location.replace("/login");
    else notice(status, error.message, true);
  }

  async function run(control, operation) {
    if (control.disabled) return;
    control.disabled = true;
    notice(status);
    try {
      await operation();
    } catch (error) {
      failed(error);
    } finally {
      control.disabled = false;
    }
  }

  function form(id, operation) {
    const target = select(id);
    const submit = target.querySelector('[type="submit"]');
    target.addEventListener("submit", (event) => {
      event.preventDefault();
      run(submit, operation);
    });
  }

  function row(name, detail, buttons) {
    const entry = element("article", null, "key-row");
    const info = element("div", null, "key-meta");
    info.append(element("h3", name), element("span", detail, "muted"));
    const controls = element("div", null, "key-actions");
    controls.append(...buttons);
    entry.append(info, controls);
    return entry;
  }

  function link(text, href) {
    const node = element("a", text, "button");
    node.href = href;
    return node;
  }

  async function hints(selectBox, defaultKey) {
    const { keys } = await request("/api/keys");
    const active = new Map();
    for (const item of keys ?? []) {
      if (!item.id || !item.key || item.isActive === false) continue;
      const option = element("option", item.name || "Router key");
      option.value = item.id;
      selectBox.append(option);
      active.set(item.id, item.key);
    }
    if (defaultKey && active.size) selectBox.value = active.keys().next().value;
    return () => active.get(selectBox.value);
  }

  function mediaList() {
    const kind = select("media-page").dataset.mediaKind;
    const list = select("media-provider-list");

    async function catalog() {
      const { connections } = await request(providerApi);
      const initial = [...list.querySelectorAll("[data-media-id]")];
      for (const entry of initial) {
        const attached = (connections ?? []).filter((item) => item.provider === entry.dataset.mediaId);
        const count = attached.filter((item) => item.isActive !== false).length;
        entry.querySelector("[data-connection-count]").textContent = `${count} active of ${attached.length} connections`;
      }
      if (kind === "embedding") {
        const { nodes } = await request(nodeApi);
        for (const entry of nodes ?? []) {
          if (entry.type !== "custom-embedding") continue;
          list.append(row(entry.name, entry.prefix || entry.id, [
            link("Configure", url(`${mediaPath}/embedding`, entry.id)),
          ]));
        }
      }
    }

    if (["web", "webFetch", "webSearch"].includes(kind)) {
      request(comboApi).then(({ combos }) => {
        const selected = (combos ?? []).filter((combo) => ["webFetch", "webSearch"].includes(combo.kind));
        select("media-combo-list").replaceChildren(...selected.map((combo) =>
          row(combo.name, combo.kind === "webFetch" ? "Web Fetch" : "Web Search",
            [link("Edit", url(`${mediaPath}/combo`, combo.id))])));
      }).catch(failed);
      form("media-new-combo", async () => {
        const response = await request(comboApi, "POST", {
          name: field("media-combo-name"), kind: select("media-combo-kind").value, models: [],
        });
        window.location.assign(url(`${mediaPath}/combo`, response.id));
      });
    }
    catalog().catch(failed);
  }

  function mediaDetail() {
    const { provider, alias, mediaKind, endpoint } = select("media-page").dataset;
    const create = select("media-create-connection");
    create.href = query("/dashboard/providers/new", { provider });
    select("media-models").href = query("/dashboard/models", { provider: alias, type: mediaKind });
    const list = select("media-connections");
    const custom = provider.startsWith("custom-embedding-");

    async function connections() {
      const { connections } = await request(providerApi);
      const attached = (connections ?? []).filter((entry) => entry.provider === provider);
      if (["image", "video"].includes(mediaKind)) {
        const selector = select("media-preview-connection");
        const previous = selector.value;
        const choices = attached.filter(entry => entry.isActive !== false && entry.id).map(entry => {
          const choice = element("option", entry.name || entry.id);
          choice.value = entry.id;
          return choice;
        });
        const automatic = element("option", "Auto (by priority)");
        automatic.value = "";
        selector.replaceChildren(automatic, ...choices);
        selector.value = choices.some(choice => choice.value === previous) ? previous : "";
        select("media-preview-connections").hidden = choices.length === 0;
      }
      list.replaceChildren(...(attached.length ? attached.map((entry) => {
        const enabled = entry.isActive !== false;
        return row(entry.name || provider, enabled ? "Active" : "Disabled", [
          editor && entry.authType === "apikey"
            ? action("Edit", () => editor.edit(entry))
            : link("Edit", url("/dashboard/providers/connections", entry.id)),
          action(enabled ? "Disable" : "Enable", (event) => run(event.currentTarget, async () => {
            await request(url(providerApi, entry.id), "PUT", { isActive: !enabled });
            await connections();
          })),
        ]);
      }) : [element("p", "No connections configured.", "muted")]));
    }

    async function node() {
      if (!custom) return alias;
      const { nodes } = await request(nodeApi);
      const saved = (nodes ?? []).find((entry) => entry.id === provider);
      if (!saved) throw new Error("Embedding endpoint was not found.");
      const prefix = saved.prefix || provider;
      select("media-models").href = query("/dashboard/models", { provider: prefix, type: mediaKind });
      return prefix;
    }

    const editor = mediaCredentialEditor(provider, mediaKind, connections);
    const publicProxy = select("media-public-proxy");
    if (!publicProxy.hidden) configurePublicProxy(provider).catch(failed);
    if (select("provider-account")) {
      document.addEventListener("provider-accounts-updated", () => connections().catch(failed));
    }
    connections().catch(failed);
    const modelPrefix = node();
    if (endpoint) {
      preview(mediaKind, endpoint, modelPrefix).catch(failed);
    } else {
      modelPrefix.catch(failed);
    }
  }

  async function configurePublicProxy(provider) {
    const pool = select("media-public-proxy-pool");
    const rotation = select("media-public-proxy-rotation");
    const info = select("media-public-proxy-status");
    const button = select("media-public-proxy-form").querySelector('[type="submit"]');
    const [data, settings] = await Promise.all([request("/api/proxy-pools?isActive=true"), request("/api/settings")]);
    const saved = settings.providerStrategies?.[provider] ?? {};
    const relay = ["vercel", "cloudflare", "deno", "netlify"];
    for (const entry of data.proxyPools ?? []) {
      if (!entry.id || ["edge-tts", "google-tts"].includes(provider) && relay.includes(entry.type)) continue;
      const option = element("option", entry.name || entry.id);
      option.value = entry.id;
      pool.append(option);
    }
    if (saved.proxyPoolId && saved.proxyPoolId !== "__none__") {
      if ([...pool.options].some(option => option.value === saved.proxyPoolId)) pool.value = saved.proxyPoolId;
      else notice(info, "The saved proxy pool is unavailable. Choose an active pool or use gateway settings.", true);
    }
    rotation.value = ["round-robin", "random"].includes(saved.rotateStrategy) ? saved.rotateStrategy : "none";
    const sync = () => { pool.disabled = rotation.value !== "none"; };
    rotation.addEventListener("change", sync);
    rotation.disabled = false;
    sync();
    button.disabled = false;
    form("media-public-proxy-form", async () => {
      const saved = await request(url("/api/settings/provider-proxy", provider), "PATCH", {
        proxyPoolId: pool.value || null,
        rotateStrategy: rotation.value,
      });
      pool.value = saved.proxyPoolId || "";
      rotation.value = saved.rotateStrategy || "none";
      sync();
      notice(info, "Proxy settings saved.");
    });
  }

  function mediaCredentialEditor(provider, kind, refresh) {
    const panel = select("media-credential-form");
    if (!panel) return null;
    const { scheme } = panel.dataset;
    const name = select("media-credential-name");
    const key = select("media-credential-key");
    const account = select("media-credential-account");
    const endpoint = select("media-credential-url");
    const file = select("media-credential-file");
    const location = select("media-credential-location");
    const awsAccess = select("media-credential-aws-access");
    const awsRegion = select("media-credential-aws-region");
    const awsToken = select("media-credential-aws-token");
    const awsClear = select("media-credential-aws-clear");
    const cancel = select("media-credential-cancel");
    const settings = select("media-credential-settings");
    const submit = panel.querySelector('[type="submit"]');
    let existing;

    function reset(entry) {
      existing = entry;
      const details = entry?.providerSpecificData ?? {};
      name.value = entry?.name || "";
      key.value = "";
      file.value = "";
      endpoint.value = typeof details.baseUrl === "string" ? details.baseUrl : "";
      account.value = typeof details.accountId === "string" ? details.accountId : "";
      location.value = typeof details.location === "string" ? details.location : "";
      awsAccess.value = typeof details.accessKeyId === "string" ? details.accessKeyId : "";
      awsRegion.value = typeof details.region === "string" ? details.region : "";
      awsToken.value = "";
      awsClear.checked = false;
      select("media-credential-aws-clear-row").hidden = !entry;
      awsAccess.required = scheme === "polly";
      key.required = scheme !== "vertex" && !entry;
      file.required = scheme === "vertex" && !entry;
      account.required = scheme === "cloudflare";
      endpoint.required = scheme === "selfhosted";
      submit.textContent = entry ? "Save changes" : "Save connection";
      cancel.hidden = settings.hidden = !entry;
      if (entry) settings.href = url("/dashboard/providers/connections", entry.id);
      else settings.removeAttribute("href");
    }

    const validSegment = (value) => /^[a-zA-Z0-9_.:@-]+$/.test(value) && value !== "." && value !== "..";
    async function build() {
      const label = name.value.trim();
      if (!label) throw new Error("Enter a connection name.");
      const payload = { name: label };
      if (!existing) payload.provider = provider;
      if (scheme === "vertex") {
        const selected = file.files[0];
        if (selected) {
          let credentials;
          try { credentials = JSON.parse(await selected.text()); }
          catch { throw new Error("Select a valid Vertex service account JSON file."); }
          if (credentials?.type !== "service_account" || typeof credentials.project_id !== "string"
              || !validSegment(credentials.project_id) || typeof credentials.client_email !== "string"
              || !credentials.client_email.trim() || typeof credentials.private_key !== "string" || !credentials.private_key.trim()) {
            throw new Error("Select a Vertex service account file with a project ID, client email and private key.");
          }
          payload.apiKey = JSON.stringify(credentials);
        } else if (!existing) {
          throw new Error("Select a Vertex service account JSON file.");
        }
        const choice = location.value.trim();
        if (choice && !validSegment(choice)) throw new Error("Enter a valid Vertex location.");
        if (choice || existing) payload.providerSpecificData = { location: choice || null };
      } else {
        const secret = key.value.trim();
        if (secret) payload.apiKey = secret;
        else if (!existing) throw new Error("Enter an API key.");
        if (scheme === "cloudflare") {
          const id = account.value.trim();
          if (!/^[a-zA-Z0-9_-]+$/.test(id)) throw new Error("Enter a valid Cloudflare account ID.");
          payload.providerSpecificData = { accountId: id };
        } else if (scheme === "polly") {
          const accessKeyId = awsAccess.value.trim();
          const region = awsRegion.value.trim();
          const sessionToken = awsToken.value.trim();
          if (!/^[a-zA-Z0-9_.-]+$/.test(accessKeyId)) throw new Error("Enter a valid AWS access key ID.");
          if (region && !/^[a-z0-9][a-z0-9-]{0,30}$/.test(region)) throw new Error("Enter a valid AWS region.");
          if (sessionToken && awsClear.checked) throw new Error("Choose whether to replace or remove the session token.");
          if (accessKeyId.startsWith("ASIA") && (!existing || awsClear.checked) && !sessionToken) {
            throw new Error("Temporary AWS credentials require a session token.");
          }
          payload.providerSpecificData = { accessKeyId, region: region || null };
          if (sessionToken || awsClear.checked) payload.providerSpecificData.sessionToken = sessionToken || null;
        } else {
          const baseUrl = endpoint.value.trim();
          let address;
          try { address = new URL(baseUrl); }
          catch { throw new Error("Enter the HTTP or HTTPS endpoint URL."); }
          if (!["http:", "https:"].includes(address.protocol) || address.username || address.password || address.hash
              || (kind !== "stt" && address.search)) {
            throw new Error("Enter an HTTP or HTTPS endpoint URL with a separate API key.");
          }
          payload.providerSpecificData = { baseUrl };
        }
      }
      return payload;
    }

    reset();
    cancel.addEventListener("click", () => { if (!submit.disabled) reset(); });
    form("media-credential-form", async () => {
      try {
        const payload = await build();
        let result;
        try {
          result = await request(existing ? url(providerApi, existing.id) : providerApi, existing ? "PUT" : "POST", payload);
        } catch (error) {
          if (error.status === 401) throw error;
          throw new Error(error.status === 409 ? "A connection with this name already exists." : "Could not save the connection.");
        }
        if (!result.connection?.id) throw new Error("Could not save the connection.");
        reset();
        await refresh();
        notice(status, "Connection saved.");
      } finally {
        key.value = "";
        file.value = "";
        awsToken.value = "";
      }
    });
    return { edit(entry) {
      if (submit.disabled) return;
      reset(entry);
      name.focus();
    } };
  }

  function speechFields(prefix, content, provider) {
    const panel = select("media-tts");
    if (!panel) return null;
    const config = JSON.parse(panel.dataset.catalog) ?? {};
    const model = select("media-tts-model");
    const modelList = select("media-tts-models");
    const voice = select("media-tts-voice");
    const load = select("media-tts-load");
    const picker = select("media-tts-voice-list");
    const language = select("media-tts-language");
    const voices = select("media-tts-voices");
    const format = select("media-tts-format");
    const info = select("media-tts-status");
    const availableModels = new Map((config.models ?? []).map(item => [item.id, item.name || item.id]));
    let catalog;
    let generated;

    function option(value, label) {
      const node = element("option", label);
      node.value = value;
      return node;
    }

    function modelId() {
      return model.value.trim().startsWith(`${prefix}/`)
        ? model.value.trim().slice(prefix.length + 1) : model.value.trim();
    }

    function updateDraft(field) {
      try {
        const draft = JSON.parse(content.value);
        if (!draft || typeof draft !== "object" || Array.isArray(draft)) throw new Error();
        if (field !== "format") {
          const selectedVoice = voice.value.trim();
          const namedVoice = provider === "ollama-local" && selectedVoice === "default" ? "" : selectedVoice;
          draft.model = [prefix, config.hasModel === false ? "" : modelId(), namedVoice].filter(Boolean).join("/");
        }
        if (field !== "model") draft.response_format = format.value;
        content.value = JSON.stringify(draft, null, 2);
        generated = content.value;
      } catch {
        notice(info, "Correct the request JSON before updating the speech settings.", true);
      }
    }

    function modelVoices() {
      const index = config.modelVoices?.[modelId()] ?? config.defaultVoices;
      return config.voiceSets?.[index] ?? [];
    }

    function updateVoices() {
      const groups = catalog?.byLang ?? {};
      const code = language.value;
      const listed = catalog
        ? (code ? groups[code]?.voices ?? [] : catalog.voices ?? Object.values(groups).flatMap(group => group.voices ?? []))
        : modelVoices();
      const unique = new Map(listed.filter(item => typeof item.id === "string" && item.id)
        .map(item => [item.id, item.name || item.id]));
      voices.replaceChildren(option("", "Choose a voice"), ...[...unique].map(([id, name]) => option(id, name)));
      voices.value = unique.has(voice.value) ? voice.value : "";
      picker.hidden = !catalog && !unique.size;
      language.disabled = !catalog || language.options.length < 2;
    }

    function updateModels() {
      modelList.replaceChildren(...[...availableModels].map(([id, name]) => option(id, name)));
    }

    const formats = config.formats ?? [{ value: "mp3", label: "MP3" }, { value: "json", label: "JSON (Base64)" }];
    format.replaceChildren(...formats.map(item => option(item.value, item.label)));
    format.value = config.defaultFormat || formats[0].value;
    model.value = availableModels.keys().next().value ?? "";
    voice.value = config.defaultVoice || modelVoices()[0]?.id || "";
    updateModels();
    updateVoices();
    updateDraft();

    model.addEventListener("change", () => {
      const choices = modelVoices();
      if (!catalog && config.modelVoices && !choices.some(item => item.id === voice.value)) {
        voice.value = choices[0]?.id || "";
      }
      updateVoices();
      updateDraft("model");
    });
    voice.addEventListener("change", () => { updateVoices(); updateDraft("model"); });
    language.addEventListener("change", updateVoices);
    voices.addEventListener("change", () => {
      if (!voices.value) return;
      voice.value = voices.value;
      updateDraft("model");
    });
    format.addEventListener("change", () => updateDraft("format"));
    if (config.endpoint) load.addEventListener("click", event => run(event.currentTarget, async () => {
      catalog = await request(config.endpoint);
      const groups = catalog.languages ?? Object.values(catalog.byLang ?? {});
      language.replaceChildren(option("", "All languages"), ...groups
        .filter(entry => typeof entry.code === "string").map(entry => option(entry.code, entry.name || entry.code)));
      updateVoices();
      notice(info, voices.options.length > 1 ? "Voice catalog loaded." : "The provider returned no voices.");
    }));

    return (data) => {
      if (config.hasModel === false) return;
      for (const item of data) {
        if (typeof item.id !== "string" || !item.id.startsWith(`${prefix}/`)) continue;
        const id = item.id.slice(prefix.length + 1);
        if (id && !availableModels.has(id)) availableModels.set(id, item.name || id);
      }
      updateModels();
      if (!model.value && availableModels.size && content.value === generated) {
        model.value = availableModels.keys().next().value;
        if (!voice.value) voice.value = modelVoices()[0]?.id || "";
        updateVoices();
        updateDraft("model");
      }
    };
  }

  async function preview(kind, endpoint, modelPrefix) {
    const prefix = await modelPrefix;
    const key = select("media-key");
    const selectedSecret = await hints(key, true);
    const content = select("media-preview-body");
    const formNode = select("media-preview-form");
    const submit = formNode.querySelector('[type="submit"]');
    const copy = select("media-preview-copy");
    const stop = select("media-preview-stop");
    const output = select("media-preview-result");
    const info = select("media-preview-status");
    const audio = select("media-preview-audio");
    const video = select("media-preview-video");
    const image = select("media-preview-image");
    const download = select("media-preview-download");
    const formats = {
      "audio/mpeg": [audio, "mp3"], "audio/mp3": [audio, "mp3"], "audio/wav": [audio, "wav"], "audio/x-wav": [audio, "wav"],
      "audio/ogg": [audio, "ogg"], "audio/flac": [audio, "flac"], "audio/aac": [audio, "aac"],
      "audio/pcm": [null, "pcm"], "audio/mulaw": [null, "mulaw"], "audio/alaw": [null, "alaw"],
      "image/png": [image, "png"], "image/jpeg": [image, "jpg"], "image/webp": [image, "webp"],
      "video/mp4": [video, "mp4"], "video/webm": [video, "webm"],
    };
    const defaults = {
      embedding: { input: "Offline example text" },
      tts: { input: "Hello", response_format: "mp3" },
      stt: { response_format: "json" },
      image: { prompt: "A small garden in the rain", n: 1 },
      video: { prompt: "Waves rolling toward a beach" },
      webSearch: { query: "What is the latest news about AI?", search_type: "web", max_results: 5 },
      webFetch: { url: "https://example.com", format: "markdown" },
      systemone: { state: "Payments have failed for three days.", questions: { is_urgent: { type: "noul", instructions: "Does this request require urgent attention?" } } },
    };
    const providerModel = ["webSearch", "webFetch"].includes(kind);
    const provider = select("media-page").dataset.provider;
    const sample = { model: providerModel ? prefix : `${prefix}/model-id`, ...defaults[kind] };
    let current;
    let downloadUrl;
    content.value = JSON.stringify(sample, null, 2);
    const speechModels = kind === "tts" ? speechFields(prefix, content, provider) : null;

    function clearMedia() {
      for (const element of [audio, video, image]) {
        element.removeAttribute("src");
        element.hidden = true;
      }
      if (downloadUrl) URL.revokeObjectURL(downloadUrl);
      downloadUrl = undefined;
      download.removeAttribute("href");
      download.hidden = true;
    }

    function showMedia(source, player, extension) {
      if (player) {
        player.src = source;
        player.hidden = false;
      }
      download.href = source;
      download.download = `preview.${extension}`;
      download.hidden = false;
    }

    function showInlineImage(inline, codec, secret) {
      if (typeof inline !== "string" || !/^[A-Za-z0-9+/]+={0,2}$/.test(inline) || secret && inline.includes(secret)) return;
      const types = { png: "image/png", jpeg: "image/jpeg", webp: "image/webp" };
      const extension = Object.hasOwn(types, codec) ? codec : "png";
      showMedia(`data:${types[extension]};base64,${inline}`, image, extension);
    }

    async function streamImage(response, codec, redact, secret) {
      const reader = response.body.getReader();
      const decoder = new TextDecoder();
      let pending = "";
      let event = "";
      let lines = [];
      let final;
      try {
        while (true) {
          const { value, done } = await reader.read();
          pending += done ? decoder.decode() + "\n\n" : decoder.decode(value, { stream: true });
          for (let edge = pending.indexOf("\n"); edge >= 0; edge = pending.indexOf("\n")) {
            const line = pending.slice(0, edge).replace(/\r$/, "");
            pending = pending.slice(edge + 1);
            if (line.startsWith("event:")) { event = line.slice(6).trim(); continue; }
            if (line.startsWith("data:")) { lines.push(line.slice(5).trimStart()); continue; }
            if (line || !lines.length) continue;
            let payload;
            try { payload = JSON.parse(lines.join("\n")); } catch { payload = {}; }
            lines = [];
            if (event === "progress") info.textContent = redact(String(payload.stage || "Generating image…"));
            else if (event === "partial_image") showInlineImage(payload.b64_json, codec, secret);
            else if (event === "done") final = payload;
            else if (event === "error") throw new Error(redact(payload.message || "Image stream failed."));
            event = "";
          }
          if (done) break;
        }
      } finally {
        await reader.cancel().catch(() => {});
        reader.releaseLock();
      }
      if (!final) throw new Error("The image stream ended before returning a result.");
      showInlineImage(final.data?.[0]?.b64_json, codec, secret);
      output.textContent = redact(JSON.stringify(final, null, 2));
      output.hidden = false;
      info.textContent = `HTTP ${response.status} · Image complete`;
    }

    async function catalog() {
      if (providerModel) return;
      const secret = selectedSecret();
      const response = await fetch(`/v1/models/${encodeURIComponent(kind)}`, {
        headers: secret ? { Authorization: `Bearer ${secret}` } : {},
        cache: "no-store",
      });
      if (!response.ok) return;
      const { data } = await response.json();
      if (speechModels) { speechModels(data ?? []); return; }
      const suggestion = (data ?? []).find((item) => item.id?.startsWith(prefix + "/"));
      if (suggestion && (!content.value || content.value.includes('"model-id"') || content.value.includes(`"${prefix}/model-id"`))) {
        content.value = JSON.stringify({ ...sample, model: suggestion.id }, null, 2);
      }
    }

    function describeRequest(secret) {
      const data = JSON.parse(content.value);
      if (!data || typeof data !== "object" || Array.isArray(data)) throw new Error("Enter a JSON request object.");
      const headers = {};
      if (secret) headers.Authorization = `Bearer ${secret}`;
      if (kind !== "stt") headers["Content-Type"] = "application/json";
      const preferred = select("media-preview-connection").value;
      if (["image", "video"].includes(kind) && preferred) headers["x-connection-id"] = preferred;
      const destination = kind === "image" && select("media-image-response-format").value === "binary"
        ? query(endpoint, { response_format: "binary" }) : endpoint;
      if (kind === "image" && provider === "codex" && destination === endpoint) headers.Accept = "text/event-stream";
      return { data, headers, destination };
    }

    stop.addEventListener("click", () => current?.abort());
    key.addEventListener("change", () => { catalog().catch(() => {}); });
    copy.addEventListener("click", event => run(event.currentTarget, async () => {
      const { data, headers, destination } = describeRequest(selectedSecret());
      let output;
      if (kind === "image" && destination !== endpoint) {
        output = `preview.${["png", "jpeg", "webp"].includes(data.output_format) ? data.output_format : "png"}`;
      } else if (kind === "tts" && data.response_format !== "json") {
        output = `speech.${["mp3", "wav", "pcm", "ogg", "aac", "flac"].includes(data.response_format) ? data.response_format : "mp3"}`;
      }
      await navigator.clipboard.writeText(curlCommand(destination, headers, data, kind === "stt", output));
      notice(status, "cURL command copied.");
    }));
    form("media-preview-form", async () => {
      const secret = selectedSecret();
      const { data, headers, destination } = describeRequest(secret);
      const redact = text => secret ? text.replaceAll(secret, "[redacted]") : text;
      let body;
      if (kind === "stt") {
        const file = select("media-preview-file").files[0];
        if (!file) throw new Error("Choose an audio file.");
        const multipart = new FormData();
        for (const [name, value] of Object.entries(data)) multipart.append(name, String(value));
        multipart.append("file", file);
        body = multipart;
      } else {
        body = JSON.stringify(data);
      }
      const controller = new AbortController();
      current = controller;
      stop.hidden = false;
      clearMedia();
      output.hidden = true;
      output.textContent = "";
      info.textContent = "Sending preview…";
      try {
        const res = await fetch(destination, { method: "POST", cache: "no-store", headers, body, signal: controller.signal });
        const mime = (res.headers.get("content-type") || "unknown").split(";", 1)[0].trim().toLowerCase();
        if (res.ok && kind === "image" && mime === "text/event-stream" && res.body) {
          await streamImage(res, data.output_format, redact, secret);
        } else if (mime.includes("application/json")) {
          const result = await res.json();
          const text = JSON.stringify(result, null, 2);
          output.textContent = redact(text);
          output.hidden = false;
          const inline = result?.data?.[0]?.b64_json;
          if (res.ok && kind === "image") showInlineImage(inline, data.output_format, secret);
          if (res.ok && kind === "tts" && typeof result.audio === "string" && /^[A-Za-z0-9+/]+={0,2}$/.test(result.audio)
              && (!secret || !result.audio.includes(secret))) {
            const codecs = { mp3: "audio/mpeg", wav: "audio/wav", ogg: "audio/ogg", flac: "audio/flac", aac: "audio/aac" };
            if (Object.hasOwn(codecs, result.format)) {
              showMedia(`data:${codecs[result.format]};base64,${result.audio}`, audio, result.format);
            }
          }
          info.textContent = `HTTP ${res.status} · ${mime}`;
        } else {
          const blob = await res.blob();
          info.textContent = `HTTP ${res.status} · ${mime} · ${blob.size} bytes`;
          if (res.ok && (Object.hasOwn(formats, mime) || mime === "application/octet-stream")) {
            const [player, extension] = formats[mime] ?? [null, "bin"];
            downloadUrl = URL.createObjectURL(blob);
            showMedia(downloadUrl, player, extension);
          }
        }
      } catch (error) {
        info.textContent = controller.signal.aborted ? "Preview stopped." : "Could not complete preview.";
        if (!controller.signal.aborted) throw error;
      } finally {
        current = undefined;
        stop.hidden = true;
      }
    });
    window.addEventListener("pagehide", () => {
      current?.abort();
      clearMedia();
    });
    submit.disabled = false;
    copy.disabled = false;
    catalog().catch(() => {});
  }

  function mediaCombo() {
    const page = select("media-page");
    const id = page.dataset.combo;
    const endpoint = url(comboApi, id);
    const settingsForm = select("media-combo-form").querySelector("fieldset");
    const rotate = select("media-combo-rotate");
    const preview = select("media-combo-preview");
    const previewButton = select("media-combo-run");
    const copy = select("media-combo-copy");
    const result = select("media-combo-result");
    const resultStatus = select("media-combo-result-status");
    const image = select("media-combo-image");
    const audio = select("media-combo-audio");
    const download = select("media-combo-download");
    const logs = select("media-combo-logs");
    const refreshLogs = select("media-combo-logs-refresh");
    const memberInput = select("media-combo-member");
    const memberEditor = select("media-combo-members");
    const memberOptions = select("media-combo-options");
    const memberOrder = select("media-combo-order");
    const webProviders = JSON.parse(page.dataset.providers);
    const examples = {
      webSearch: ["/api/v1/search", { query: "What is the latest news about AI?", search_type: "web", max_results: 5 }],
      webFetch: ["/api/v1/web/fetch", { url: "https://example.com", format: "markdown" }],
      image: ["/api/v1/images/generations", { prompt: "A cute cat playing piano", n: 1, size: "1024x1024" }],
      tts: ["/api/v1/audio/speech", { input: "Hello, this is a test.", voice: "alloy" }],
    };
    let target;
    let sample;
    let blobUrl;
    let pendingPreview;

    function members() {
      return memberEditor.value.split(/\r?\n/).map(value => value.trim()).filter(Boolean);
    }

    function showMembers() {
      const list = members();
      if (!list.length) {
        memberOrder.replaceChildren(element("p", "No members configured.", "muted"));
        return;
      }
      memberOrder.replaceChildren(...list.map((name, index) => {
        const move = (shift) => {
          const next = index + shift;
          if (next < 0 || next >= list.length) return;
          [list[index], list[next]] = [list[next], list[index]];
          memberEditor.value = list.join("\n");
          showMembers();
        };
        const up = action("Move up", () => move(-1));
        const down = action("Move down", () => move(1));
        up.disabled = index === 0;
        down.disabled = index === list.length - 1;
        const remove = action("Remove", () => {
          list.splice(index, 1);
          memberEditor.value = list.join("\n");
          showMembers();
        });
        return row(name, String(index + 1), [up, down, remove]);
      }));
    }

    async function loadMemberOptions() {
      const item = target;
      const [providers, aliases, keys] = (await Promise.allSettled([
        request(providerApi), request("/api/models/alias"), request("/api/keys"),
      ])).map(result => result.value ?? {});
      if (target !== item) return;
      const choices = new Map();
      if (["webSearch", "webFetch"].includes(item.kind)) {
        const active = new Set((providers.connections ?? []).filter(entry => entry.isActive !== false).map(entry => entry.provider));
        for (const entry of webProviders) {
          if (entry.kinds.includes(item.kind) && (entry.noAuth || active.has(entry.id))) choices.set(entry.alias, entry.name || entry.id);
        }
      } else if (["image", "tts", "stt", "video", "embedding", "systemone"].includes(item.kind)) {
        const secret = keys.keys?.find(key => key.isActive !== false && key.key)?.key;
        const headers = secret ? { Authorization: `Bearer ${secret}` } : {};
        const response = await fetch(`/v1/models/${item.kind}`, { headers, cache: "no-store" });
        if (response.ok) {
          const models = await response.json();
          for (const model of models.data ?? []) if (model.id) choices.set(model.id, model.name || model.id);
        }
      }
      if (target !== item) return;
      for (const [alias, value] of Object.entries(aliases.aliases ?? {})) {
        const id = typeof value === "string" ? value : `${value?.provider}/${value?.model}`;
        if (choices.has(id)) choices.set(alias, id);
      }
      memberOptions.replaceChildren(...[...choices].sort(([left], [right]) => left.localeCompare(right)).map(([id, name]) => {
        const option = element("option", name);
        option.value = id;
        return option;
      }));
    }

    memberEditor.addEventListener("input", showMembers);
    select("media-combo-add").addEventListener("click", () => {
      const model = memberInput.value.trim();
      const list = members();
      if (!model || list.includes(model)) return;
      list.push(model);
      memberEditor.value = list.join("\n");
      memberInput.value = "";
      memberInput.focus();
      showMembers();
    });
    memberInput.addEventListener("keydown", event => {
      if (event.key !== "Enter") return;
      event.preventDefault();
      select("media-combo-add").click();
    });

    function clearResult() {
      pendingPreview?.abort();
      if (blobUrl) URL.revokeObjectURL(blobUrl);
      blobUrl = undefined;
      for (const media of [image, audio]) {
        media.removeAttribute("src");
        media.hidden = true;
      }
      download.removeAttribute("href");
      download.removeAttribute("download");
      download.hidden = true;
      result.textContent = "";
      result.hidden = true;
      notice(resultStatus);
    }

    function displayMedia(media, source, label, filename) {
      if (media) {
        media.src = source;
        media.hidden = false;
      }
      download.href = source;
      download.textContent = label;
      if (filename) download.download = filename;
      download.hidden = false;
    }

    function showCombo(item) {
      target = item;
      select("media-combo-title").textContent = item.name;
      select("media-combo-name").value = item.name;
      const members = Array.isArray(item.models) ? item.models : [];
      memberEditor.value = members.join("\n");
      showMembers();
      select("media-combo-back").href = comboPath(item.kind);
      settingsForm.disabled = false;
      refreshLogs.disabled = false;
      const template = Object.hasOwn(examples, item.kind) ? examples[item.kind] : null;
      sample = template ? { path: template[0], body: { model: item.name, ...template[1] } } : null;
      preview.hidden = !sample;
      previewButton.disabled = !sample || !members.length;
      if (sample) {
        select("media-combo-endpoint").textContent = `POST ${sample.path}`;
        select("media-combo-example").textContent = JSON.stringify(sample.body, null, 2);
      }
      clearResult();
      loadMemberOptions().catch(failed);
    }

    async function readRotation() {
      rotate.disabled = true;
      const item = target;
      try {
        const data = await request("/api/settings");
        if (target !== item) return;
        rotate.checked = data.comboStrategies?.[item.name]?.fallbackStrategy === "round-robin";
        rotate.disabled = false;
      } catch (error) {
        if (target === item) failed(error);
      }
    }

    async function usage() {
      const item = target;
      try {
        const lines = await request("/api/usage/logs");
        if (target !== item) return;
        const relevant = Array.isArray(lines)
          ? lines.filter((line) => typeof line === "string" && line.includes(item.name)).slice(0, 50)
          : [];
        logs.textContent = relevant.length ? relevant.join("\n") : "No usage yet.";
      } catch {
        if (target === item) logs.textContent = "Could not load usage logs.";
      }
    }

    async function testExample() {
      const item = target;
      const example = sample;
      clearResult();
      const controller = new AbortController();
      pendingPreview = controller;
      const { keys } = await request("/api/keys");
      if (target !== item || controller.signal.aborted) return;
      const secret = keys?.find((entry) => entry.isActive !== false && entry.key)?.key;
      const redact = (text) => secret
        ? text.replaceAll(secret, "[redacted]").replaceAll(encodeURIComponent(secret), "[redacted]")
        : text;
      const headers = { "Content-Type": "application/json" };
      if (secret) headers.Authorization = `Bearer ${secret}`;
      const started = Date.now();
      try {
        const response = await fetch(example.path, {
          method: "POST", headers, cache: "no-store", body: JSON.stringify(example.body), signal: controller.signal,
        });
        const elapsed = Date.now() - started;
        if (target !== item || controller.signal.aborted) return;
        const type = (response.headers.get("content-type") || "").split(";", 1)[0].trim().toLowerCase();
        if (!response.ok) {
          const payload = await response.json().catch(() => ({}));
          if (target !== item || controller.signal.aborted) return;
          const detail = payload?.error?.message || payload?.error || payload?.message;
          result.textContent = redact(JSON.stringify(payload, null, 2));
          result.hidden = false;
          notice(resultStatus, typeof detail === "string" ? redact(detail) : `HTTP ${response.status}`, true);
          return;
        }
        if (type.startsWith("image/") || type.startsWith("audio/") || type === "application/octet-stream") {
          const blob = await response.blob();
          if (target !== item || controller.signal.aborted) return;
          const isImage = type.startsWith("image/");
          blobUrl = URL.createObjectURL(blob);
          const suffix = type === "audio/mpeg" ? "mp3" : type.split("/")[1];
          const filename = (isImage ? "image" : "speech") + (/^[a-z0-9]+$/.test(suffix) ? "." + suffix : "");
          displayMedia(isImage ? image : audio, blobUrl, isImage ? "Download image" : "Download audio", filename);
        } else {
          const data = await response.json();
          if (target !== item || controller.signal.aborted) return;
          const text = JSON.stringify(data, (key, value) =>
            key === "b64_json" && typeof value === "string" && value.length > 100
              ? `<${value.length} chars base64>`
              : value, 2);
          result.textContent = redact(text);
          result.hidden = false;
          if (item.kind === "image") {
            const first = data?.data?.[0];
            if (typeof first?.b64_json === "string" && /^[A-Za-z0-9+/]+={0,2}$/.test(first.b64_json)
                && redact(first.b64_json) === first.b64_json) {
              displayMedia(image, `data:image/png;base64,${first.b64_json}`, "Download image", "image.png");
            } else if (typeof first?.url === "string" && redact(first.url) === first.url) {
              try {
                const remote = new URL(first.url);
                if (remote.protocol === "https:" && !remote.username && !remote.password) {
                  displayMedia(null, remote.href, "Open generated image");
                }
              } catch {}
            }
          }
        }
        notice(resultStatus, `HTTP ${response.status} · ${elapsed} ms`);
      } catch (error) {
        if (target === item && !controller.signal.aborted) notice(resultStatus, redact(error.message), true);
      } finally {
        if (pendingPreview === controller) pendingPreview = undefined;
        if (target === item && !controller.signal.aborted) await usage();
      }
    }

    request(endpoint).then(async (item) => {
      showCombo(item);
      await Promise.all([readRotation(), usage()]);
    }).catch(failed);
    form("media-combo-form", async () => {
      const item = await request(endpoint, "PUT", { name: field("media-combo-name"), models: members() });
      showCombo(item);
      await Promise.all([readRotation(), usage()]);
      notice(status, "Combo saved.");
    });
    rotate.addEventListener("change", () => {
      const checked = rotate.checked;
      run(rotate, async () => {
        const item = target;
        const data = await request("/api/settings");
        if (target !== item) return;
        const strategies = { ...data.comboStrategies };
        delete strategies[item.name];
        await request("/api/settings", "PATCH", {
          comboStrategies: checked
            ? { ...strategies, [item.name]: { fallbackStrategy: "round-robin" } }
            : strategies,
        });
        notice(status, "Rotation saved.");
      }).then(readRotation);
    });
    previewButton.addEventListener("click", () => {
      if (!sample || !Array.isArray(target.models) || !target.models.length) return;
      run(previewButton, testExample).then(() => {
        previewButton.disabled = !sample || !Array.isArray(target.models) || !target.models.length;
      });
    });
    copy.addEventListener("click", event => run(event.currentTarget, async () => {
      if (!sample) return;
      const example = sample;
      const { keys } = await request("/api/keys");
      const secret = keys?.find(entry => entry.isActive !== false && entry.key)?.key;
      const headers = { "Content-Type": "application/json" };
      if (secret) headers.Authorization = `Bearer ${secret}`;
      await navigator.clipboard.writeText(curlCommand(example.path, headers, example.body));
      notice(status, "cURL command copied.");
    }));
    refreshLogs.addEventListener("click", () => run(refreshLogs, usage));
    select("media-combo-delete").addEventListener("click", (event) => {
      if (!target || !window.confirm("Delete this media combo?")) return;
      run(event.currentTarget, async () => {
        await request(endpoint, "DELETE");
        window.location.assign(comboPath(target.kind));
      });
    });
    window.addEventListener("pagehide", clearResult);
  }

  function stateText(data) {
    if (typeof data?.installed !== "boolean") return "Status unavailable";
    if (!data.installed) return "Not installed";
    return data.has9Router ? "9Router configured" : "Installed";
  }

  function cliList() {
    select("cli-refresh").addEventListener("click", (event) => run(event.currentTarget, async () => {
      const states = await request(cliStatus);
      for (const item of document.querySelectorAll("[data-cli-id]")) {
        item.querySelector("[data-install-state]").textContent = stateText(states[item.dataset.cliId]);
      }
    }));
  }

  function cliGuide() {
    const guide = select("cli-guide");
    const values = select("cli-guide-values");
    const base = select("cli-guide-base");
    const model = select("cli-guide-model");
    const key = select("cli-guide-key");
    const templates = new Map([...document.querySelectorAll("[data-guide-template]")]
      .map(node => [node, node.textContent]));
    const keys = values.hidden ? Promise.resolve(() => undefined) : hints(key, false);
    base.value = window.location.origin;
    const presets = values.hidden ? null : cliEndpoints(base, status);

    function routerUrl() {
      try {
        const endpoint = new URL(base.value.trim());
        if (!["http:", "https:"].includes(endpoint.protocol) || endpoint.username || endpoint.password || endpoint.search || endpoint.hash) return null;
        const path = endpoint.pathname.replace(/\/+$/, "");
        return `${endpoint.origin}${path}${path.endsWith("/v1") ? "" : "/v1"}`;
      } catch { return null; }
    }

    function renderTemplate(node, apiKey) {
      const fields = { apiKey, baseUrl: routerUrl() ?? "<Router base URL>", model: model.value.trim() || "provider/model-id" };
      return templates.get(node).replace(/\{\{(apiKey|baseUrl|model)\}\}/g, (_, field) => {
        const value = fields[field];
        if (node.dataset.guideLanguage === "json") return JSON.stringify(value).slice(1, -1);
        if (node.dataset.guideLanguage === "bash") return value.replace(/["\\`$]/g, "\\$&");
        return value;
      });
    }

    function preview() {
      for (const node of templates.keys()) node.textContent = renderTemplate(node, "<9Router API key>");
    }

    values.addEventListener("input", preview);
    for (const button of document.querySelectorAll("[data-guide-copy]")) {
      button.addEventListener("click", event => run(event.currentTarget, async () => {
        const node = button.previousElementSibling;
        const template = templates.get(node);
        if (template.includes("{{baseUrl}}") && !routerUrl()) throw new Error("Enter a valid router base URL.");
        const apiKey = template.includes("{{apiKey}}") ? (await keys)() || "sk_9router" : "";
        await navigator.clipboard.writeText(renderTemplate(node, apiKey));
        notice(status, "Copied.");
      }));
    }
    const install = select("cli-guide-refresh");
    if (!install.hidden) {
      install.addEventListener("click", event => run(event.currentTarget, async () => {
        const data = await request("/api/cli-tools/devin-settings");
        const result = select("cli-guide-status");
        result.textContent = [stateText(data), data.version, data.message].filter(Boolean).join(" · ");
        result.hidden = false;
      }));
    }
    preview();
    if (values.hidden) return;
    keys.catch(failed);
    Promise.allSettled([request("/api/tunnel/status"), request("/api/models/alias"), request("/v1/models")]).then(results => {
      const [network, aliases, listed] = results.map(result => result.value ?? {});
      const publicUrls = [network.tunnel?.publicUrl, network.tunnel?.tunnelUrl, network.tailscale?.tunnelUrl].filter(Boolean);
      if (guide.dataset.external === "true" && base.value === window.location.origin && publicUrls[0]) base.value = publicUrls[0];
      presets?.network(network);
      const ids = new Set([...Object.keys(aliases.aliases ?? {}), ...(listed.data ?? []).map(item => item.id)]);
      for (const id of [...ids].filter(Boolean).sort()) {
        const option = element("option");
        option.value = id;
        select("cli-guide-models").append(option);
      }
      preview();
    }).catch(failed);
  }

  function cliDetail() {
    const tool = select("cli-page").dataset.tool;
    const base = select("cli-base-url");
    const model = select("cli-model");
    const key = select("cli-key");
    const sample = select("cli-preview");
    const copy = select("cli-copy");
    const manualPath = {
      copilot: "chatLanguageModels.json",
      cowork: "configLibrary/<appliedId>.json",
      "deepseek-tui": "~/.deepseek/config.toml",
      droid: /Win/i.test(navigator.platform) ? "%USERPROFILE%\\.factory\\settings.json" : "~/.factory/settings.json",
      "grok-build": "~/.grok/config.toml",
      kilo: "~/.local/share/kilo/auth.json",
      openclaw: "~/.openclaw/openclaw.json",
    }[tool];
    const manualPaths = {
      cline: ["~/.cline/data/globalState.json", "~/.cline/data/secrets.json"],
      jcode: ["~/.jcode/config.toml", "~/.config/jcode/provider-9router.env"],
    }[tool];
    const manualFile = manualPaths ? element("select") : null;
    if (manualFile) {
      manualFile.id = "cli-manual-file";
      for (const path of manualPaths) {
        const option = element("option", path);
        option.value = path.split("/").at(-1);
        manualFile.append(option);
      }
      const label = element("label", "Configuration file");
      label.htmlFor = manualFile.id;
      sample.before(label, manualFile);
      manualFile.addEventListener("change", previewConfig);
    }
    if (manualPath) {
      sample.before(element("p", manualPath, "muted"));
      copy.textContent = `Copy ${manualPath.split(/[\\/]/).at(-1)}`;
    }
    const output = select("cli-tool-status");
    const router = select("cli-config-form");
    const actions = select("cli-host-actions");
    const additional = select("cli-extra-models");
    const codexSubagent = select("cli-codex-subagent");
    const openCodeDefault = select("cli-opencode-default");
    const openCodeExplorer = select("cli-opencode-explorer");
    let openCodePinned;
    if (tool === "codex") copy.textContent = "Copy config.toml";
    if (tool === "opencode") copy.textContent = "Copy opencode.json";
    const claudeModels = [
      ["ANTHROPIC_DEFAULT_FABLE_MODEL", select("cli-claude-fable")],
      ["ANTHROPIC_DEFAULT_SONNET_MODEL", model],
      ["ANTHROPIC_DEFAULT_OPUS_MODEL", select("cli-claude-opus")],
      ["ANTHROPIC_DEFAULT_HAIKU_MODEL", select("cli-claude-haiku")],
    ];
    const compact = select("cli-claude-compact");
    const context = select("cli-claude-context");
    const naming = select("cli-claude-filter-naming");
    const exa = select("cli-claude-exa");
    const multi = ["copilot", "cowork", "droid", "opencode"].includes(tool);
    const grokWindow = select("cli-grok-context");
    const grokRoles = [...document.querySelectorAll("[data-grok-subagent]")];
    const grokContextWindows = new Map();
    const coworkSelection = select("cowork-connector-selection");
    const coworkPreset = select("cowork-browser-preset");
    const agentGroup = select("cli-openclaw-agents");
    let agentModels = null;
    base.value = window.location.origin;
    const presets = cliEndpoints(base, status);
    request("/api/tunnel/status").then(presets.network).catch(() => {});
    const edited = new Set();
    router.addEventListener("input", (event) => { edited.add(event.target); });

    function models() {
      return [...new Set([model.value, ...(additional?.value.split("\n") ?? []), openCodeExplorer?.value ?? ""]
        .map(value => value.trim()).filter(Boolean))];
    }

    function previewConfig() {
      if (manualFile || manualPath) {
        if (manualFile) copy.textContent = `Copy ${manualFile.value}`;
        sample.textContent = toolFileConfig("<9Router API key>");
        return;
      }
      if (tool === "codex") {
        sample.textContent = codexConfig("<9Router API key>");
        return;
      }
      if (tool === "opencode") {
        syncOpenCodeDefault();
        sample.textContent = JSON.stringify(openCodeConfig("<9Router API key>"), null, 2);
        return;
      }
      const origin = base.value.trim().replace(/\/+$/, "");
      const baseUrl = tool === "claude" || origin.endsWith("/v1") ? origin : `${origin}/v1`;
      const ids = models();
      const shaped = tool === "claude" ? claudeConfig("<9Router API key>") : {
        baseUrl,
        ...(tool === "omp" ? {} : multi
          ? { models: ids.length ? ids : ["provider/model-id"] }
          : { model: model.value.trim() || "provider/model-id" }),
        apiKey: "<9Router API key>",
      };
      sample.textContent = JSON.stringify(shaped, null, 2);
    }

    function toolFileConfig(secret) {
      const origin = base.value.trim().replace(/\/+$/, "");
      const baseUrl = origin.endsWith("/v1") ? origin : `${origin}/v1`;
      const modelId = model.value.trim() || "provider/model-id";
      const apiKey = secret || "sk_9router";
      const ids = models();
      if (!ids.length) ids.push(modelId);
      let config;
      switch (tool) {
        case "cline":
          config = manualFile.value === "secrets.json" ? { openAiApiKey: apiKey } : {
            actModeApiProvider: "openai",
            planModeApiProvider: "openai",
            openAiBaseUrl: origin.replace(/\/v1$/, ""),
            openAiModelId: modelId,
            planModeOpenAiModelId: modelId,
          };
          break;
        case "kilo":
          config = { "openai-compatible": { type: "api-key", apiKey, baseUrl, model: modelId } };
          break;
        case "copilot":
          config = [{
            name: "9Router", vendor: "azure", apiKey,
            models: ids.map(id => ({
              id, name: id, url: `${baseUrl}/chat/completions#models.ai.azure.com`,
              toolCalling: true, vision: false, maxInputTokens: 128000, maxOutputTokens: 16000,
            })),
          }];
          break;
        case "cowork":
          config = {
            inferenceProvider: "gateway",
            inferenceGatewayBaseUrl: baseUrl,
            inferenceGatewayApiKey: apiKey,
            inferenceModels: ids.map(name => ({ name })),
          };
          break;
        case "droid":
          config = { customModels: ids.map((id, index) => ({
            model: id, id: `custom:9Router-${index}`, index, baseUrl, apiKey, displayName: id,
            maxOutputTokens: 131072, noImageSupport: false, provider: "openai",
          })) };
          break;
        case "deepseek-tui":
          return [
            "[providers.openai]",
            `base_url = ${tomlString(baseUrl)}`,
            `api_key = ${tomlString(apiKey)}`,
            `model = ${tomlString(modelId)}`,
            "",
          ].join("\n");
        case "openclaw": {
          const overrides = [...(agentModels ?? [])].map(([id, input]) => [id, input.value.trim()])
            .filter(([, id]) => id);
          const registered = [...new Set([modelId, ...overrides.map(([, id]) => id)])];
          config = {
            agents: {
              defaults: {
                model: { primary: `9router/${modelId}` },
                models: Object.fromEntries(registered.map(id => [`9router/${id}`, {}])),
              },
              ...(overrides.length ? { list: overrides.map(([id, model]) => ({ id, model: `9router/${model}` })) } : {}),
            },
            models: { providers: { "9router": {
              baseUrl, apiKey, api: "openai-completions",
              models: registered.map(id => ({ id, name: id.split("/").at(-1) || id })),
            } } },
          };
          break;
        }
        case "grok-build":
          return grokConfig(baseUrl, apiKey, modelId);
        case "jcode": {
          if (manualFile.value === "provider-9router.env") {
            const quote = /["\\$`]/.test(apiKey) ? "'" : '"';
            if (/\p{Cc}/u.test(apiKey) || quote === "'" && apiKey.includes("'")) {
              throw new Error("The API key must be a supported single-line string.");
            }
            return `JCODE_9ROUTER_API_KEY=${quote}${apiKey}${quote}\n`;
          }
          const selected = model.value.trim() || "cc/claude-opus-4-7";
          return [
            "[providers.9router]",
            'type = "openai-compatible"',
            `base_url = ${tomlString(baseUrl)}`,
            'auth = "bearer"',
            'api_key_env = "JCODE_9ROUTER_API_KEY"',
            'env_file = "provider-9router.env"',
            `default_model = ${tomlString(selected)}`,
            "requires_api_key = true",
            "",
            "[[providers.9router.models]]",
            `id = ${tomlString(selected)}`,
            "",
          ].join("\n");
        }
      }
      return JSON.stringify(config, null, 2);
    }

    function hostPayload(secret) {
      const origin = base.value.trim().replace(/\/+$/, "");
      return tool === "claude" ? claudePayload(secret || "sk_9router") : {
        baseUrl: ["copilot", "cowork"].includes(tool) && !origin.endsWith("/v1") ? `${origin}/v1` : origin,
        ...(tool === "omp" ? {} : { model: model.value.trim() }),
        ...(tool === "codex" ? { subagentModel: codexSubagent.value.trim() } : {}),
        ...(tool === "opencode" ? {
          activeModel: openCodeDefault.value, subagentModel: openCodeExplorer.value.trim(),
        } : {}),
        ...(multi || tool === "jcode" ? { models: models() } : {}),
        ...(tool === "grok-build" ? grokOptions() : {}),
        ...(tool === "cowork" ? coworkOptions() : {}),
        ...(agentModels ? { agentModels: openClawOverrides() } : {}),
        apiKey: secret || (["cowork", "jcode"].includes(tool) ? "" : "sk_9router"),
      };
    }

    function claudePayload(secret) {
      const env = {
        ANTHROPIC_BASE_URL: base.value.trim().replace(/\/+$/, ""),
        ANTHROPIC_AUTH_TOKEN: secret,
      };
      for (const [key, input] of claudeModels) {
        const value = input.value.trim();
        if (value) env[key] = value;
        else if (edited.has(input)) env[key] = null;
      }
      return { env, autoCompactWindow: compact.value || null, exaMcpEnabled: exa.checked };
    }

    function claudeConfig(secret) {
      const payload = claudePayload(secret || "sk_9router");
      const env = Object.fromEntries(Object.entries(payload.env).filter(([, value]) => value !== null));
      if (!env.ANTHROPIC_BASE_URL.endsWith("/v1")) env.ANTHROPIC_BASE_URL += "/v1";
      if (payload.autoCompactWindow) env.CLAUDE_CODE_AUTO_COMPACT_WINDOW = payload.autoCompactWindow;
      return { hasCompletedOnboarding: true, env };
    }

    function codexConfig(secret) {
      const origin = base.value.trim().replace(/\/+$/, "");
      const url = origin.endsWith("/v1") ? origin : `${origin}/v1`;
      const main = model.value.trim() || "provider/model-id";
      const subagent = codexSubagent.value.trim() || main;
      return [
        `model = ${tomlString(main)}`,
        'model_provider = "9router"',
        "",
        "[model_providers.9router]",
        'name = "9Router"',
        `base_url = ${tomlString(url)}`,
        'wire_api = "responses"',
        "",
        "[model_providers.9router.http_headers]",
        `Authorization = ${tomlString(`Bearer ${secret || "sk_9router"}`)}`,
        "",
        "[agents]",
        `default_subagent_model = ${tomlString(subagent)}`,
        "",
      ].join("\n");
    }

    function syncOpenCodeDefault() {
      const ids = models();
      const none = element("option", "Use OpenCode default");
      none.value = "";
      openCodeDefault.replaceChildren(none, ...ids.map((id) => {
        const option = element("option", id);
        option.value = id;
        return option;
      }));
      openCodeDefault.value = openCodePinned === undefined ? ids[0] ?? ""
        : ids.includes(openCodePinned) ? openCodePinned : "";
    }

    function openCodeConfig(secret) {
      const origin = base.value.trim().replace(/\/+$/, "");
      const baseURL = origin.endsWith("/v1") ? origin : `${origin}/v1`;
      const ids = models();
      if (!ids.length) ids.push("provider/model-id");
      const active = openCodeDefault.value;
      const explorer = openCodeExplorer.value.trim() || ids[0];
      return {
        provider: {
          "9router": {
            npm: "@ai-sdk/openai-compatible",
            options: { baseURL, apiKey: secret || "sk_9router" },
            models: Object.fromEntries(ids.map((id) => [id, {
              name: id, modalities: { input: ["text", "image"], output: ["text"] },
            }])),
          },
        },
        model: active ? `9router/${active}` : "",
        agent: { explorer: {
          description: "Fast explorer subagent for codebase exploration",
          mode: "subagent", model: `9router/${explorer}`,
        } },
      };
    }

    function syncClaudeContext() {
      if (context) context.checked = claudeModels.some(([, input]) => /\[1m\]$/i.test(input.value.trim()));
    }

    function grokOptions() {
      const subagentModels = Object.fromEntries(grokRoles.map(row => [row.dataset.grokSubagent, {
        model: row.querySelector("[data-model]").value.trim(),
        contextWindow: row.querySelector("[data-context]").value || null,
      }]));
      return { contextWindow: grokWindow.value || null, subagentModels };
    }

    function grokConfig(baseUrl, apiKey, modelId) {
      const { contextWindow, subagentModels } = grokOptions();
      const slots = Object.entries(subagentModels).filter(([, entry]) => entry.model);
      const blocks = ['[models]\ndefault = "9router"'];
      if (slots.length) {
        blocks.push(["[subagents.models]", ...slots.map(([role]) => `${role} = "9router-${role}"`)].join("\n"));
      }
      function config(slot, name, model, context) {
        const explicit = Number(context);
        const window = Number.isSafeInteger(explicit) && explicit > 0 ? explicit
          : grokContextWindows.get(model) || 200000;
        return [
          `[model.${slot}]`,
          `model = ${tomlString(model)}`,
          `base_url = ${tomlString(baseUrl)}`,
          `name = ${tomlString(name)}`,
          'description = "Routed via 9Router gateway"',
          'api_backend = "chat_completions"',
          `api_key = ${tomlString(apiKey)}`,
          `context_window = ${window}`,
        ].join("\n");
      }
      blocks.push(config("9router", "9Router", modelId, contextWindow));
      for (const [role, entry] of slots) {
        blocks.push(config(`9router-${role}`, `9Router ${role}`, entry.model, entry.contextWindow));
      }
      return blocks.join("\n\n") + "\n";
    }

    function coworkOptions() {
      const local = coworkPreset.disabled ? {} : { localPlugins: coworkPreset.checked ? ["browsermcp"] : [] };
      const input = coworkSelection.value.trim();
      if (!input) return local;
      let entries;
      try { entries = JSON.parse(input); }
      catch { throw new Error("Enter a JSON array of connectors."); }
      if (!Array.isArray(entries)) throw new Error("Enter a JSON array of connectors.");
      return {
        ...local,
        plugins: entries.filter(entry => entry?.custom !== true),
        customPlugins: entries.filter(entry => entry?.custom === true),
      };
    }

    function showCoworkConnectors(configured) {
      const { cowork, localStdioPlugins } = configured;
      const offered = Array.isArray(localStdioPlugins) && localStdioPlugins.some(preset => preset?.name === "browsermcp");
      const saved = Array.isArray(cowork?.localPlugins) && cowork.localPlugins.includes("browsermcp");
      coworkPreset.disabled = select("cowork-browser-preset-choice").hidden = !offered;
      coworkPreset.checked = offered && saved;
      const existing = [...(cowork?.plugins ?? []), ...(cowork?.customPlugins ?? [])];
      const labels = existing.map(item => item.name).filter(Boolean);
      if (offered && saved) labels.push("Browser MCP");
      const info = select("cowork-existing-connectors");
      info.hidden = !labels.length;
      info.textContent = labels.length ? `Current connectors: ${labels.join(", ")}` : "";
    }

    function savedModels(value) {
      return Array.isArray(value) ? value
        .map((item) => typeof item === "string" ? item : item?.id)
        .filter((item) => typeof item === "string" && item) : [];
    }

    function openClawOverrides() {
      return Object.fromEntries([...agentModels].map(([id, input]) => [id, input.value.trim()]));
    }

    function loadOpenClawAgents(agents, defaultModel) {
      if (!Array.isArray(agents)) return;
      agentModels = new Map();
      agentGroup.replaceChildren(element("legend", "Agent models"));
      for (const agent of agents) {
        if (typeof agent?.id !== "string" || !agent.id || agentModels.has(agent.id)) continue;
        const input = element("input");
        input.id = `cli-openclaw-agent-${agentModels.size}`;
        input.setAttribute("list", "cli-model-options");
        input.setAttribute("autocomplete", "off");
        input.placeholder = "Use default model";
        const primary = typeof agent.model === "string" ? agent.model : "";
        if (primary.startsWith("9router/")) {
          input.value = primary.slice(8);
        } else if (!primary && typeof agent.currentModel === "string" && agent.currentModel !== defaultModel) {
          input.value = agent.currentModel;
        }
        const label = element("label", agent.name || agent.id);
        label.htmlFor = input.id;
        input.addEventListener("input", previewConfig);
        agentModels.set(agent.id, input);
        agentGroup.append(label, input);
      }
      agentGroup.hidden = agentModels.size === 0;
    }

    function loadHostConfig(data) {
      const fill = (input, value) => {
        if (input && !edited.has(input)) input.value = value;
      };
      const config = data.config ?? {};
      const settings = data.settings ?? {};
      const provider = config.providers?.["9router"];
      let savedBase;
      let saved = [];
      switch (tool) {
        case "codex":
        case "kilo":
        case "omp":
          savedBase = data.routerConfig?.baseUrl;
          saved = [data.routerConfig?.model];
          if (tool === "codex") fill(codexSubagent, data.routerConfig?.subagentModel || "");
          break;
        case "opencode":
          savedBase = data.opencode?.baseURL;
          saved = savedModels(data.opencode?.models);
          if (saved.length && !edited.has(openCodeDefault)) openCodePinned = data.opencode?.activeModel || "";
          fill(openCodeExplorer, data.opencode?.explorerModel || "");
          break;
        case "cline":
          if (settings.actModeApiProvider === "openai" || settings.planModeApiProvider === "openai") {
            savedBase = settings.openAiBaseUrl;
            saved = [settings.openAiModelId];
          }
          break;
        case "openclaw": {
          const entry = settings.models?.providers?.["9router"];
          const primary = settings.agents?.defaults?.model?.primary;
          const defaultModel = typeof primary === "string" && primary.startsWith("9router/") ? primary.slice(8) : null;
          savedBase = entry?.baseUrl;
          saved = [defaultModel, ...savedModels(entry?.models)];
          loadOpenClawAgents(data.agents, defaultModel ?? saved[1]);
          break;
        }
        case "droid": {
          const entries = Array.isArray(settings.customModels)
            ? settings.customModels.filter((entry) => entry?.id?.startsWith?.("custom:9Router")) : [];
          savedBase = entries[0]?.baseUrl;
          saved = entries.map((entry) => entry.model);
          break;
        }
        case "pi":
        case "crush":
          savedBase = provider?.baseUrl || provider?.base_url;
          saved = savedModels(provider?.models);
          break;
        case "forge":
        case "codewhale":
          if (data.has9Router) {
            savedBase = config.openai?.base_url;
            saved = [config.openai?.model];
          }
          break;
        case "smelt":
          if (data.has9Router) {
            savedBase = config.baseUrl;
            saved = [config.model];
          }
          break;
        case "copilot": {
          const entry = Array.isArray(data.config) ? data.config.find((item) => item?.name === "9Router") : null;
          savedBase = data.currentUrl?.replace(/\/chat\/completions#models\.ai\.azure\.com$/, "");
          saved = savedModels(entry?.models);
          break;
        }
        case "cowork":
          savedBase = data.cowork?.baseUrl;
          saved = savedModels(data.cowork?.models);
          if (!edited.has(coworkPreset)) showCoworkConnectors(data);
          break;
        case "deepseek-tui":
          if (settings.provider === "openai") {
            const openai = settings["providers.openai"];
            savedBase = openai?.base_url;
            saved = [openai?.model];
          }
          break;
        case "jcode":
          savedBase = provider?.base_url;
          saved = [provider?.default_model];
          break;
        case "claude": {
          const env = settings.env ?? {};
          savedBase = env.ANTHROPIC_BASE_URL;
          for (const [name, input] of claudeModels) fill(input, env[name] || "");
          syncClaudeContext();
          fill(compact, env.CLAUDE_CODE_AUTO_COMPACT_WINDOW || "");
          if (!edited.has(exa)) exa.checked = data.exaMcpEnabled === true;
          break;
        }
        case "grok-build":
          savedBase = settings.model?.base_url;
          saved = [settings.model?.model];
          fill(grokWindow, settings.model?.context_window || "");
          for (const row of grokRoles) {
            const prior = settings.subagentModels?.[row.dataset.grokSubagent] ?? {};
            fill(row.querySelector("[data-model]"), prior.model || "");
            fill(row.querySelector("[data-context]"), prior.context_window || "");
          }
          break;
      }
      const ids = [...new Set(saved.filter((id) => typeof id === "string" && id))];
      if (typeof savedBase === "string" && savedBase) fill(base, savedBase);
      if (ids.length) {
        fill(model, ids[0]);
        fill(additional, ids.slice(1).join("\n"));
      }
      presets.sync();
      previewConfig();
    }

    previewConfig();
    base.addEventListener("input", previewConfig);
    if (tool !== "claude") model.addEventListener("input", previewConfig);
    additional?.addEventListener("input", previewConfig);
    codexSubagent?.addEventListener("input", previewConfig);
    openCodeExplorer?.addEventListener("input", previewConfig);
    openCodeDefault?.addEventListener("change", () => {
      openCodePinned = openCodeDefault.value;
      edited.add(openCodeDefault);
      previewConfig();
    });
    for (const input of [compact, exa]) input?.addEventListener("input", previewConfig);
    if (tool === "claude") {
      for (const [, input] of claudeModels) {
        input.addEventListener("input", () => {
          syncClaudeContext();
          previewConfig();
        });
      }
      context.addEventListener("change", () => {
        for (const [, input] of claudeModels) {
          const value = input.value.trim().replace(/(?:\[1m\])+$/i, "");
          if (!value) continue;
          input.value = context.checked ? `${value}[1m]` : value;
          edited.add(input);
        }
        previewConfig();
      });
      request("/api/settings").then((settings) => {
        naming.checked = settings.ccFilterNaming === true;
        naming.disabled = false;
      }).catch(failed);
      naming.addEventListener("change", (event) => run(event.currentTarget, async () => {
        const enabled = naming.checked;
        try {
          await request("/api/settings", "PATCH", { ccFilterNaming: enabled });
        } catch (error) {
          naming.checked = !enabled;
          throw error;
        }
        notice(status, "Naming filter updated.");
      }));
    }
    grokWindow?.addEventListener("input", previewConfig);
    for (const row of grokRoles) row.addEventListener("input", previewConfig);

    async function refresh() {
      const data = await request(query(cliStatus, { tool }));
      output.textContent = stateText(data[tool]);
      output.hidden = false;
      if (tool === "cowork" && !actions.hidden) {
        showCoworkConnectors(await request("/api/cli-tools/cowork-settings"));
      }
    }

    select("cli-refresh").addEventListener("click", (event) => run(event.currentTarget, refresh));
    hints(key, tool === "cowork").then(async (secret) => {
      copy.addEventListener("click", (event) => run(event.currentTarget, async () => {
        if (!base.checkValidity()) throw new Error("Enter a valid router base URL.");
        if (tool === "codex" && !model.value.trim()) throw new Error("Choose a Codex model.");
        if (tool === "opencode" && !models().length) throw new Error("Choose at least one OpenCode model.");
        const config = tool === "codex" ? codexConfig(secret())
          : manualFile || manualPath ? toolFileConfig(secret()) : JSON.stringify(
          tool === "claude" ? claudeConfig(secret())
            : tool === "opencode" ? openCodeConfig(secret()) : hostPayload(secret()),
          null, 2,
        );
        await navigator.clipboard.writeText(config);
        notice(status, "Configuration copied.");
      }));
      if (!actions.hidden) {
        const endpoint = `/api/cli-tools/${encodeURIComponent(tool)}-settings`;
        form("cli-config-form", async () => {
          await request(endpoint, "POST", hostPayload(secret()));
          presets.remember();
          notice(status, "Host settings applied.");
          await refresh();
          if (tool === "opencode") await openCodeModels(openCodeRemoved);
        });
        const configured = await request(endpoint).catch((error) => {
          failed(error);
          return null;
        });
        if (configured) loadHostConfig(configured);
        router.querySelector('[type="submit"]').disabled = false;
        select("cli-restore").addEventListener("click", (event) => {
          if (!window.confirm("Remove 9Router settings for this tool?")) return;
          run(event.currentTarget, async () => {
            await request(endpoint, "DELETE");
            notice(status, "Host settings removed.");
            await refresh();
            if (tool === "opencode") {
              openCodePinned = "";
              model.value = additional.value = openCodeExplorer.value = "";
              previewConfig();
              await openCodeModels(openCodeRemoved);
            }
          });
        });
      }
      copy.disabled = false;
      presets.sync();
      const { aliases } = await request("/api/models/alias");
      const catalog = ["openclaw", "opencode", "grok-build"].includes(tool) ? await request("/v1/models").catch(() => ({})) : {};
      if (tool === "grok-build") {
        for (const { id, context_length } of catalog.data ?? []) {
          if (Number.isSafeInteger(context_length) && context_length > 0) grokContextWindows.set(id, context_length);
        }
        previewConfig();
      }
      const ids = new Set([...Object.keys(aliases ?? {}), ...savedModels(catalog.data)]);
      for (const name of ids) {
        const option = element("option");
        option.value = name;
        select("cli-model-options").append(option);
      }
    }).catch(failed);

    if (tool === "cowork") {
      const catalog = select("cowork-catalog");
      const add = select("cowork-add-connector");
      let choices = [];
      select("cowork-load-catalog").addEventListener("click", (event) => run(event.currentTarget, async () => {
        const data = await request("/api/cli-tools/cowork-mcp-registry");
        choices = data.servers ?? [];
        catalog.replaceChildren(...choices.map((entry, index) => {
          const option = element("option", entry.title || entry.name);
          option.value = String(index);
          return option;
        }));
        catalog.disabled = add.disabled = choices.length === 0;
      }));
      add.addEventListener("click", event => run(event.currentTarget, async () => {
        const selection = coworkOptions();
        const entries = [...(selection.plugins ?? []), ...(selection.customPlugins ?? [])];
        const entry = choices[Number(catalog.value)];
        if (!entry || entries.some(item => item?.name === entry.name)) return;
        const { name, url, transport, oauth } = entry;
        coworkSelection.value = JSON.stringify([...entries, { name, url, transport, oauth }], null, 2);
      }));
    }
    if (tool === "codex") codexProfiles();
    if (tool === "opencode") {
      select("cli-opencode-clear").addEventListener("click", async event => {
        const button = event.currentTarget;
        if (button.disabled) return;
        button.disabled = true;
        try {
          await request("/api/cli-tools/opencode-settings", "PATCH", { clearActiveModel: true });
          openCodePinned = "";
          previewConfig();
          await openCodeModels(openCodeRemoved);
        } catch (error) {
          button.disabled = false;
          failed(error);
        }
      });
      openCodeModels(openCodeRemoved).catch(failed);
    }

    function openCodeRemoved(id, result) {
      const remaining = [model.value, ...additional.value.split("\n")]
        .map((modelId) => modelId.trim()).filter((modelId) => modelId && modelId !== id);
      model.value = remaining[0] || "";
      additional.value = remaining.slice(1).join("\n");
      if (!edited.has(openCodeExplorer) || openCodeExplorer.value.trim() === id) {
        openCodeExplorer.value = result.explorerModel || "";
      }
      openCodePinned = result.activeModel || "";
      previewConfig();
    }
  }

  async function openCodeModels(onRemove) {
    const endpoint = "/api/cli-tools/opencode-settings";
    const result = (await request(endpoint)).opencode ?? {};
    const list = select("cli-opencode-list");
    select("cli-opencode-active").textContent = result.activeModel ?
      `Active: ${result.activeModel}` : "No active 9Router model";
    select("cli-opencode-clear").disabled = !result.activeModel;
    list.replaceChildren();
    for (const model of result.models ?? []) {
      const row = element("article", null, "key-row");
      row.append(element("code", model), action("Remove", event => {
        if (!window.confirm(`Remove ${model} from OpenCode?`)) return;
        run(event.currentTarget, async () => {
          await request(query(endpoint, {model}), "DELETE");
          onRemove(model, await openCodeModels(onRemove));
        });
      }, "button-danger"));
      list.append(row);
    }
    return result;
  }

  function codexProfiles() {
    const endpoint = "/api/cli-tools/codex-profiles";
    const output = select("cli-codex-profile-list");
    const name = select("cli-codex-profile-name");
    const model = select("cli-codex-profile-model");
    async function refresh() {
      const data = await request(endpoint);
      output.replaceChildren();
      for (const profile of data.profiles ?? []) {
        const row = element("article", null, "key-row");
        const label = element("div", null, "key-meta");
        label.append(element("strong", profile.name), element("code", profile.command),
          element("span", profile.model, "muted"));
        row.append(label, action("Delete", event => {
          if (!window.confirm(`Delete the Codex profile ${profile.name}?`)) return;
          run(event.currentTarget, async () => {
            await request(endpoint, "DELETE", { name: profile.name });
            await refresh();
          });
        }, "button-danger"));
        output.append(row);
      }
    }
    form("cli-codex-profile-form", async () => {
      await request(endpoint, "POST", { name: name.value.trim(), model: model.value.trim() });
      name.value = model.value = "";
      await refresh();
      notice(status, "Profile saved.");
    });
    refresh().catch(failed);
  }

  const pages = {
    "media-list": mediaList, "media-detail": mediaDetail, "media-combo": mediaCombo,
    "cli-list": cliList, "cli-detail": cliDetail, "cli-guide": cliGuide,
  };
  pages[document.body.dataset.page]?.();
})();
