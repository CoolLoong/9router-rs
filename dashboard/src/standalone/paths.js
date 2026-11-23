const pages = new Set([
  "/dashboard",
  "/dashboard/endpoint",
  "/dashboard/providers",
  "/dashboard/providers/new",
  "/dashboard/basic-chat",
  "/dashboard/combos",
  "/dashboard/usage",
  "/dashboard/quota",
  "/dashboard/console-log",
  "/dashboard/cli-tools",
  "/dashboard/media-providers/web",
  "/dashboard/token-saver",
  "/dashboard/pxpipe",
  "/dashboard/mitm",
  "/dashboard/skills",
  "/dashboard/translator",
  "/dashboard/proxy-pools",
  "/dashboard/profile",
  "/dashboard/settings/pricing",
]);

const dynamic = [
  [/^\/dashboard\/providers\/([^/]+)$/, ["id"]],
  [/^\/dashboard\/cli-tools\/([^/]+)$/, ["toolId"]],
  [/^\/dashboard\/media-providers\/combo\/([^/]+)$/, ["id"]],
  [/^\/dashboard\/media-providers\/([^/]+)\/([^/]+)$/, ["kind", "id"]],
  [/^\/dashboard\/media-providers\/([^/]+)$/, ["kind"]],
];

export function isDashboardPage(pathname) {
  return pages.has(pathname) || dynamic.some(([pattern]) => pattern.test(pathname));
}

export function paramsFor(pathname) {
  for (const [pattern, names] of dynamic) {
    const match = pathname.match(pattern);
    if (match) {
      try {
        return Object.fromEntries(names.map((name, index) => [name, decodeURIComponent(match[index + 1])]));
      } catch {
        return {};
      }
    }
  }
  return {};
}
