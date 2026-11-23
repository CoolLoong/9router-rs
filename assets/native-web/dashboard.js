(() => {
  "use strict";

  const select = (id) => document.getElementById(id);
  const themeMedia = window.matchMedia("(prefers-color-scheme: dark)");

  function theme() {
    try {
      const selected = JSON.parse(localStorage.getItem("theme"))?.state?.theme;
      if (selected === "light" || selected === "dark") return selected;
    } catch {}
    return "system";
  }

  function applyTheme(value) {
    document.documentElement.dataset.theme = value === "system" ? (themeMedia.matches ? "dark" : "light") : value;
  }

  applyTheme(theme());
  themeMedia.addEventListener("change", () => applyTheme(theme()));

  async function request(path, method = "GET", body) {
    const response = await fetch(path, {
      method,
      cache: "no-store",
      ...(body === undefined ? {} : {
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(body),
      }),
    });
    const data = await response.json().catch(() => ({}));
    if (!response.ok) {
      const error = new Error(data.error || data.message || `Request failed (${response.status}).`);
      error.status = response.status;
      error.retryAfter = data.retryAfter;
      throw error;
    }
    return data;
  }

  function notice(output, message = "", failed = false) {
    output.textContent = message;
    output.hidden = !message;
    output.dataset.error = failed ? "true" : "false";
  }

  function login() {
    if (document.body.dataset.ready === "true") {
      window.location.replace("/dashboard");
      return;
    }

    const form = select("login-form");
    const password = select("password");
    const submit = select("login-submit");
    const output = select("login-status");

    form.addEventListener("submit", async (event) => {
      event.preventDefault();
      if (submit.disabled || form.hidden || form.parentElement.hidden) return;
      submit.disabled = true;
      notice(output);
      try {
        await request("/api/auth/login", "POST", { password: password.value });
        password.value = "";
        window.location.replace("/dashboard");
      } catch (error) {
        notice(output, error.message, true);
        const delay = Number(error.retryAfter);
        if (Number.isFinite(delay) && delay > 0) {
          window.setTimeout(() => { submit.disabled = false; }, delay * 1000);
        } else {
          submit.disabled = false;
          password.focus();
          password.select();
        }
      }
    });
  }

  function element(tag, text, className) {
    const node = document.createElement(tag);
    if (text != null) node.textContent = text;
    if (className) node.className = className;
    return node;
  }

  function action(text, handler, className = "") {
    const button = element("button", text, `button ${className}`);
    button.type = "button";
    button.addEventListener("click", handler);
    return button;
  }

  function sessionActions() {
    const button = select("sign-out");
    button.addEventListener("click", async () => {
      button.disabled = true;
      try {
        await request("/api/auth/logout", "POST");
        window.location.replace("/login");
      } catch (error) {
        notice(select("session-status"), error.message, true);
        button.disabled = false;
      }
    });
  }

  window.nineRouterUi = Object.freeze({ select, request, notice, element, action });
  if (document.body.dataset.page === "login") login();
  else sessionActions();
})();
