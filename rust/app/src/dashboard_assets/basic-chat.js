(async () => {
  "use strict";

  const { select, request, notice, element } = window.nineRouterUi;
  const status = select("chat-status");
  const model = select("chat-model");
  const models = select("chat-models");
  const provider = select("chat-provider");
  const providerOptions = [...select("chat-provider-options").content.querySelectorAll("option")];
  const allProviders = provider.options[0];
  const key = select("chat-key");
  const history = select("chat-history");
  const draft = select("chat-draft");
  const send = select("chat-send");
  const stop = select("chat-stop");
  const stream = select("chat-stream");
  const attachments = select("chat-attachments");
  const pendingImages = select("chat-pending-images");
  const messages = select("chat-messages");
  const newChat = select("chat-new");
  const deleteChat = select("chat-delete");
  const secrets = new Map();
  const storage = {
    sessions: "basic-chat.sessions",
    session: "basic-chat.activeSessionId",
    provider: "basic-chat.activeProviderId",
    draft: "basic-chat.draft",
  };
  let completion;
  let activeRequest;
  let catalogRequest = 0;
  let catalog = new Map();
  let images = [];
  let attachmentRead;

  function load(name) {
    try {
      return localStorage.getItem(storage[name]) || "";
    } catch {
      return "";
    }
  }

  function save(name, value) {
    try {
      localStorage.setItem(storage[name], value);
    } catch {}
  }

  function savedSessions() {
    try {
      const saved = JSON.parse(load("sessions"));
      if (Array.isArray(saved)) {
        return saved.filter((item) => item?.id).map((item) => ({
          ...item,
          messages: Array.isArray(item.messages) ? item.messages.filter((entry) => entry?.role)
            .map((entry) => entry.status === "streaming" ? { ...entry, status: "stopped" } : entry) : [],
        }));
      }
    } catch {}
    return [];
  }

  let sessions = savedSessions();
  let active = sessions.find((item) => item.id === load("session")) || sessions[0];
  draft.value = load("draft");

  function persist() {
    save("sessions", JSON.stringify(sessions));
    save("session", active?.id || "");
    if (active) save("provider", active.providerId || "");
    save("draft", draft.value);
  }

  function createId() {
    return globalThis.crypto?.randomUUID?.() || `chat_${Date.now()}_${Math.random().toString(16).slice(2)}`;
  }

  function createChat(selected = "") {
    const now = new Date().toISOString();
    return {
      id: createId(),
      title: "New chat",
      providerId: selected.includes("/") ? selected.split("/", 1)[0] : "",
      modelId: selected,
      modelName: selected,
      createdAt: now,
      updatedAt: now,
      messages: [],
    };
  }

  function selectChatModel(chosen) {
    if (chosen === active?.modelId) return false;
    if (!active || active.messages.length) {
      active = createChat(chosen);
      sessions.push(active);
    } else {
      active.providerId = chosen.includes("/") ? chosen.split("/", 1)[0] : "";
      active.modelId = active.modelName = chosen;
    }
    return true;
  }

  function renderMessages() {
    const parts = new Map();
    const rows = (active?.messages ?? []).map((message) => {
      const user = message.role === "user";
      const row = element("article", null, user ? "chat-message chat-user" : "chat-message");
      let text = typeof message.content === "string" ? message.content : "";
      if (!text && message.status === "streaming") text = "Waiting for response…";
      if (!text && message.status === "stopped") text = "Response stopped.";
      const body = element("p", text, "chat-body");
      row.append(element("h3", user ? "You" : "Assistant"), body);
      if (Array.isArray(message.attachments) && message.attachments.length) {
        const gallery = element("div", null, "chat-images");
        for (const attachment of message.attachments) {
          const name = attachment?.name || "Image attachment";
          if (typeof attachment?.dataUrl !== "string" || !attachment.dataUrl.startsWith("data:image/")) {
            gallery.append(element("span", name, "muted"));
            continue;
          }
          const image = element("img");
          image.src = attachment.dataUrl;
          image.alt = name;
          image.loading = "lazy";
          image.decoding = "async";
          gallery.append(image);
        }
        row.append(gallery);
      }
      if (message.error) row.append(element("p", message.error, "muted"));
      parts.set(message.id, body);
      return row;
    });
    messages.replaceChildren(...(rows.length ? rows : [element("p", "Start a conversation by sending a message.", "muted")]));
    messages.scrollTop = messages.scrollHeight;
    return parts;
  }

  function renderHistory() {
    const sorted = [...sessions].sort((left, right) => String(right.updatedAt).localeCompare(String(left.updatedAt)));
    if (!sorted.length) history.replaceChildren(element("option", "No conversations"));
    else {
      history.replaceChildren(...sorted.map((item) => {
        const entry = element("option", item.title || "New chat");
        entry.value = item.id;
        return entry;
      }));
      history.value = active?.id ?? "";
    }
    deleteChat.disabled = Boolean(activeRequest) || !active;
  }

  function controls() {
    const running = Boolean(activeRequest);
    send.disabled = running || Boolean(attachmentRead) || !completion || !model.value.trim()
      || (!draft.value.trim() && images.length === 0);
    stop.hidden = !running;
    model.disabled = running;
    provider.disabled = running || provider.options.length < 2;
    key.disabled = running || secrets.size === 0;
    history.disabled = running;
    newChat.disabled = running;
    deleteChat.disabled = running || !active;
    draft.disabled = running;
    attachments.disabled = running || Boolean(attachmentRead);
    pendingImages.inert = running;
    stream.disabled = running;
  }

  function showCurrent() {
    const owner = catalog.get(model.value.trim());
    if (owner) provider.value = owner;
    else provider.value = "";
    renderModels();
    renderHistory();
    renderMessages();
    persist();
    controls();
  }

  function gateway(path, options = {}) {
    const secret = secrets.get(key.value);
    const headers = { ...options.headers };
    if (secret) headers.Authorization = `Bearer ${secret}`;
    return fetch(path, { ...options, headers, cache: "no-store" });
  }

  function redact(text) {
    let safe = text;
    for (const secret of secrets.values()) safe = safe.replaceAll(secret, "[redacted]");
    return safe;
  }

  async function loadKeys() {
    const { keys } = await request("/api/keys");
    for (const item of keys ?? []) {
      if (!item.id || !item.key || item.isActive === false) continue;
      secrets.set(item.id, item.key);
      const option = element("option", item.name || "API key");
      option.value = item.id;
      key.append(option);
    }
    key.value = [...secrets.keys()][0] || "";
  }

  function renderModels() {
    models.replaceChildren(...[...catalog].filter(([, owner]) => !provider.value || owner === provider.value)
      .map(([id]) => {
        const option = element("option");
        option.value = id;
        return option;
      }));
  }

  async function loadModels() {
    const generation = ++catalogRequest;
    notice(status);
    catalog.clear();
    provider.replaceChildren(allProviders);
    renderModels();
    controls();
    try {
      const response = await gateway("/v1/models");
      if (!response.ok) throw new Error("Model catalog unavailable. Enter a model ID manually.");
      const { data } = await response.json();
      if (generation !== catalogRequest) return;
      const knownOwners = new Set(providerOptions.map((option) => option.value));
      catalog = new Map((data ?? []).filter((item) => typeof item?.id === "string" && item.id && knownOwners.has(item.owned_by))
        .map((item) => [item.id, item.owned_by]));
      const owners = new Set(catalog.values());
      const choices = providerOptions.filter((option) => owners.has(option.value));
      provider.append(...choices.map((option) => option.cloneNode(true)));
      if (!model.value) {
        const prior = load("provider");
        const preferredOwner = (choices.find((option) => option.value === prior || option.dataset.provider === prior)
          || choices.find((option) => option.value !== "combo"))?.value;
        model.value = [...catalog].find(([, owner]) => owner === preferredOwner)?.[0] || catalog.keys().next().value || "";
      }
      provider.value = catalog.get(model.value.trim()) ?? "";
      renderModels();
      if (!catalog.size) notice(status, "No connected models are available with this key.");
    } catch (error) {
      if (generation === catalogRequest) notice(status, redact(error.message), true);
    } finally {
      if (generation === catalogRequest) controls();
    }
  }

  function attachment(file) {
    return new Promise((resolve, reject) => {
      const reader = new FileReader();
      reader.onerror = () => reject(new Error("Could not read the selected image."));
      reader.onload = () => resolve({
        id: createId(),
        name: file.name,
        type: file.type,
        dataUrl: String(reader.result),
      });
      reader.readAsDataURL(file);
    });
  }

  function renderAttachments() {
    pendingImages.replaceChildren(...images.map((image) => {
      const button = element("button", `Remove ${image.name || "image"}`, "button");
      button.type = "button";
      button.addEventListener("click", () => {
        images = images.filter((item) => item.id !== image.id);
        renderAttachments();
        controls();
      });
      return button;
    }));
    pendingImages.hidden = images.length === 0;
  }

  function clearAttachments() {
    images = [];
    attachmentRead = undefined;
    attachments.value = "";
    renderAttachments();
  }

  function contextEntry(message) {
    if (message.role !== "user" || !message.attachments?.length) {
      return { role: message.role, content: message.content };
    }
    const images = message.attachments.filter((item) => item?.dataUrl)
      .map((item) => ({ type: "image_url", image_url: { url: item.dataUrl } }));
    const text = message.content ? [{ type: "text", text: message.content }] : [];
    return { role: message.role, content: [...text, ...images] };
  }

  async function complete(event) {
    event.preventDefault();
    if (send.disabled) return;
    const selected = model.value.trim();
    const prompt = draft.value.trim();
    if (!selected) return;
    const controller = new AbortController();
    activeRequest = controller;
    const wantsStream = stream.checked;
    notice(status);
    controls();
    let assistant;
    try {
      selectChatModel(selected);
      const { messages: conversation } = active;
      const now = new Date().toISOString();
      active.updatedAt = now;
      if (!conversation.length) {
        const title = prompt.replace(/\s+/g, " ") || images[0]?.name || "New chat";
        active.title = title.length > 52 ? title.slice(0, 52).trimEnd() + "…" : title;
      }
      conversation.push({
        id: createId(),
        role: "user",
        content: prompt,
        attachments: images,
        createdAt: now,
      });
      const context = conversation.filter((message) =>
        message.role === "user" || message.role === "assistant" && Boolean(message.content)
      ).map(contextEntry);
      assistant = { id: createId(), role: "assistant", content: "", createdAt: now, status: "streaming" };
      conversation.push(assistant);
      draft.value = "";
      clearAttachments();
      renderHistory();
      const output = renderMessages().get(assistant.id);
      persist();

      const response = await gateway("/v1/chat/completions", {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          Accept: wantsStream ? "text/event-stream" : "application/json",
        },
        body: JSON.stringify({ model: selected, messages: context, stream: wantsStream }),
        signal: controller.signal,
      });
      if (!response.ok) {
        const payload = await response.json().catch(() => ({}));
        throw new Error(completion.errorText(payload) || `Request failed (${response.status}).`);
      }
      let raw = "";
      await completion.readCompletion(response, (fragment) => {
        if (!fragment) return;
        raw += fragment;
        assistant.content = redact(raw);
        output.textContent = assistant.content;
        messages.scrollTop = messages.scrollHeight;
      });
      assistant.status = "done";
      if (!assistant.content) notice(status, "The model returned no text.");
    } catch (error) {
      if (assistant) {
        assistant.status = controller.signal.aborted ? "stopped" : "error";
        if (!controller.signal.aborted) assistant.error = redact(error.message);
      }
      if (controller.signal.aborted) notice(status, "Response stopped.");
      else notice(status, redact(error.message), true);
    } finally {
      activeRequest = undefined;
      showCurrent();
      draft.focus();
    }
  }

  select("chat-form").addEventListener("submit", complete);
  stop.addEventListener("click", () => activeRequest?.abort());
  attachments.addEventListener("change", async () => {
    const files = [...attachments.files].filter((file) => file.type.startsWith("image/"));
    attachments.value = "";
    if (!files.length) return;
    const reading = Promise.all(files.map(attachment));
    attachmentRead = reading;
    controls();
    try {
      const added = await reading;
      if (attachmentRead === reading) {
        images.push(...added);
        renderAttachments();
      }
    } catch (error) {
      if (attachmentRead === reading) notice(status, error.message, true);
    } finally {
      if (attachmentRead === reading) attachmentRead = undefined;
      controls();
    }
  });
  draft.addEventListener("input", () => {
    save("draft", draft.value);
    controls();
  });
  draft.addEventListener("keydown", (event) => {
    if (event.key === "Enter" && !event.shiftKey && !event.isComposing) {
      event.preventDefault();
      if (!send.disabled) select("chat-form").requestSubmit();
    }
  });
  key.addEventListener("change", loadModels);
  provider.addEventListener("change", () => {
    const owner = provider.value;
    if (owner && catalog.get(model.value.trim()) !== owner) {
      const chosen = [...catalog].find(([, group]) => group === owner)?.[0];
      if (chosen) {
        model.value = chosen;
        if (selectChatModel(chosen)) showCurrent();
      }
    }
    renderModels();
    controls();
  });
  model.addEventListener("input", controls);
  model.addEventListener("change", () => {
    const chosen = model.value.trim();
    if (chosen && selectChatModel(chosen)) showCurrent();
  });
  history.addEventListener("change", () => {
    active = sessions.find((item) => item.id === history.value);
    model.value = active?.modelId || model.value;
    showCurrent();
  });
  newChat.addEventListener("click", () => {
    active = createChat(model.value.trim());
    sessions.push(active);
    draft.value = "";
    clearAttachments();
    notice(status);
    showCurrent();
    draft.focus();
  });
  deleteChat.addEventListener("click", () => {
    if (!active || !window.confirm("Delete this conversation?")) return;
    sessions = sessions.filter((item) => item.id !== active.id);
    active = sessions[0];
    model.value = active?.modelId || model.value;
    showCurrent();
  });

  model.value = active?.modelId || "";
  showCurrent();
  try {
    completion = await import("/__9router-ui/chat-stream.mjs");
    await loadKeys();
    await loadModels();
  } catch (error) {
    if (error.status === 401) window.location.replace("/login");
    else notice(status, redact(error.message), true);
  } finally {
    controls();
  }
})();
