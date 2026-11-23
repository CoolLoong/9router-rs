(() => {
  "use strict";
  const query = new URLSearchParams(window.location.search);
  const fragment = new URLSearchParams((window.location.hash || "").slice(1));
  const hasBearer = (params) => params.has("token") || params.has("access_token");
  const isToken = (params) => hasBearer(params) || params.get("state")?.startsWith("kimchi_");
  const useFragment = !query.has("code") && !query.has("error") && !hasBearer(query) &&
    (hasBearer(fragment) || ((isToken(query) || isToken(fragment)) &&
      (fragment.has("code") || fragment.has("error"))));
  const params = useFragment ? fragment : query;
  const code = params.get("code");
  const token = params.get("token") || params.get("access_token");
  const state = params.get("state") || (params === fragment ? query.get("state") : null);
  const error = params.get("error");
  const sensitive = isToken(query) || isToken(fragment);
  let clearLocation = false;
  if (sensitive) {
    try { window.history.replaceState(null, "", window.location.pathname); }
    catch { clearLocation = true; }
  }
  if ((code || token || error) && state) {
    const data = { code: token || code, token, state, error: sensitive && error ? "authorization_failed" : error,
      errorDescription: sensitive ? null : params.get("error_description"),
      ...(sensitive ? {} : { fullUrl: window.location.href }) };
    try {
      window.opener?.postMessage({ type: "oauth_callback", data }, window.location.origin);
    } catch {}
    try {
      const channel = new BroadcastChannel("oauth_callback");
      channel.postMessage(data);
      channel.close();
    } catch {}
  }
  if (sensitive) {
    document.getElementById("oauth-callback-status").textContent =
      "Return to the provider setup to finish authorization.";
    if (clearLocation) window.location.replace(window.location.pathname);
  } else if (code && state && !error) {
    document.getElementById("oauth-callback-status").textContent =
      "Return to the provider setup. If it asks for the callback URL, copy it from this tab.";
  }
})();
