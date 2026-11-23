const { select, request, notice } = window.nineRouterUi;
const dialog = select("quota-auto-ping");
const form = select("quota-auto-ping-form");
const message = select("quota-auto-ping-message");
const reactive = select("quota-ping-reactive");
const anchored = select("quota-ping-anchored");
const fields = select("quota-ping-fields");
const anchor = select("quota-ping-anchor");
const zone = select("quota-ping-zone");
const windowHours = select("quota-ping-window");
const slots = select("quota-ping-slots");
const submit = form.querySelector("[type=submit]");
const cancel = select("quota-ping-cancel");
let target;

function syncControls() {
  fields.disabled = !anchored.checked;
  slots.max = Math.max(1, Math.floor(24 / (windowHours.valueAsNumber || 5)));
}

export async function openQuotaAutoPing(account, label, output) {
  if (dialog.open) return;
  const current = {
    id: account.id,
    key: account.provider === "codex" ? "codexAutoPing" : "claudeAutoPing",
    output,
  };
  target = current;
  select("quota-auto-ping-title").textContent = `Quota auto-ping · ${label}`;
  submit.disabled = true;
  notice(message, "Loading schedule…");
  dialog.showModal();
  try {
    const config = (await request("/api/settings"))[current.key] ?? {};
    if (target !== current || !dialog.open) return;
    const schedule = config.schedules?.[current.id] ?? {};
    reactive.checked = config.connections?.[current.id] === true;
    anchored.checked = schedule.enabled === true;
    anchor.value = schedule.anchor ?? "09:30";
    zone.value = schedule.timezone ?? Intl.DateTimeFormat().resolvedOptions().timeZone ?? "UTC";
    windowHours.value = schedule.windowHours ?? 5;
    slots.value = schedule.slotsPerDay ?? 3;
    syncControls();
    submit.disabled = false;
    notice(message);
  } catch (error) {
    if (target === current && dialog.open) notice(message, error.message, true);
  }
}

anchored.addEventListener("change", syncControls);
windowHours.addEventListener("input", syncControls);
cancel.addEventListener("click", () => { if (!cancel.disabled) dialog.close(); });
dialog.addEventListener("cancel", (event) => { if (cancel.disabled) event.preventDefault(); });
dialog.addEventListener("close", () => { if (!dialog.open) target = undefined; });

form.addEventListener("submit", async (event) => {
  event.preventDefault();
  if (!target || !dialog.open || submit.disabled) return;
  const { id, key, output } = target;
  const timezone = zone.value.trim();
  if (anchored.checked) {
    try { new Intl.DateTimeFormat("en", { timeZone: timezone }); }
    catch { notice(message, "Enter a valid time zone.", true); return; }
  }
  const schedule = anchored.checked ? {
    enabled: true, anchor: anchor.value, timezone,
    windowHours: windowHours.valueAsNumber, slotsPerDay: slots.valueAsNumber,
  } : null;
  const pingOnReset = reactive.checked;
  submit.disabled = cancel.disabled = true;
  try {
    const prior = (await request("/api/settings"))[key] ?? {};
    const connections = { ...prior.connections };
    const schedules = { ...prior.schedules };
    if (pingOnReset) connections[id] = true; else delete connections[id];
    if (schedule) schedules[id] = schedule; else delete schedules[id];
    await request("/api/settings", "PATCH", { [key]: { ...prior, connections, schedules } });
    dialog.close();
    notice(output, "Quota auto-ping settings saved.");
  } catch (error) { notice(message, error.message, true); }
  finally { submit.disabled = cancel.disabled = false; }
});
