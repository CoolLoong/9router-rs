(() => {
  "use strict";

  const { select, request } = window.nineRouterUi;
  const status = select("console-status");
  const output = select("console-lines");
  const clear = select("console-clear");
  const endpoint = "/api/translator/console-logs";
  const logs = new EventSource(`${endpoint}/stream`);
  let lines = [];

  function render() {
    output.textContent = lines.length ? lines.join("\n") : "No console logs";
    output.scrollTop = output.scrollHeight;
  }

  logs.onopen = () => { status.textContent = "Connected"; };
  logs.onerror = () => { status.textContent = "Reconnecting…"; };
  logs.onmessage = event => {
    let message;
    try { message = JSON.parse(event.data); } catch { return; }
    if (message.type === "init") lines = message.logs;
    else if (message.type === "line") lines.push(message.line);
    else if (message.type === "lines") lines.push(...message.lines);
    else if (message.type === "clear") lines = [];
    lines = lines.slice(-200);
    render();
  };
  clear.addEventListener("click", async () => {
    clear.disabled = true;
    try {
      await request(endpoint, "DELETE");
      lines = [];
      render();
    } catch (error) { status.textContent = error.message; }
    finally { clear.disabled = false; }
  });
  window.addEventListener("pagehide", () => logs.close());
  render();
})();
