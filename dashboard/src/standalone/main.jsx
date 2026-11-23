import { useEffect, useState } from "react";
import { createRoot } from "react-dom/client";
import "material-symbols/outlined.css";
import "@/app/globals.css";
import "./inter.css";
import { RuntimeI18nProvider } from "@/i18n/RuntimeI18nProvider";
import { ThemeProvider } from "@/shared/components/ThemeProvider";
import DashboardLayout from "@/shared/components/layouts/DashboardLayout";
import { CLI_TOOLS } from "@/shared/constants/cliTools";
import { useParams, usePathname, notFound } from "./navigation";
import Endpoint from "@/app/(dashboard)/dashboard/endpoint/EndpointPageClient";
import BasicChat from "@/app/(dashboard)/dashboard/basic-chat/page";
import CliTools from "@/app/(dashboard)/dashboard/cli-tools/CLIToolsPageClient";
import CliTool from "@/app/(dashboard)/dashboard/cli-tools/[toolId]/ToolDetailClient";
import Combos from "@/app/(dashboard)/dashboard/combos/page";
import ConsoleLog from "@/app/(dashboard)/dashboard/console-log/page";
import Media from "@/app/(dashboard)/dashboard/media-providers/[kind]/page";
import MediaProvider from "@/app/(dashboard)/dashboard/media-providers/[kind]/[id]/page";
import MediaCombo from "@/app/(dashboard)/dashboard/media-providers/combo/[id]/page";
import WebMedia from "@/app/(dashboard)/dashboard/media-providers/web/page";
import Mitm from "@/app/(dashboard)/dashboard/mitm/page";
import Profile from "@/app/(dashboard)/dashboard/profile/page";
import Provider from "@/app/(dashboard)/dashboard/providers/[id]/page";
import NewProvider from "@/app/(dashboard)/dashboard/providers/new/page";
import Providers from "@/app/(dashboard)/dashboard/providers/page";
import ProxyPools from "@/app/(dashboard)/dashboard/proxy-pools/page";
import Pxpipe from "@/app/(dashboard)/dashboard/pxpipe/page";
import Quota from "@/app/(dashboard)/dashboard/quota/page";
import Skills from "@/app/(dashboard)/dashboard/skills/page";
import TokenSaver from "@/app/(dashboard)/dashboard/token-saver/page";
import Translator from "@/app/(dashboard)/dashboard/translator/page";
import Usage from "@/app/(dashboard)/dashboard/usage/page";
import Pricing from "@/app/dashboard/settings/pricing/page";

const pages = {
  "/dashboard/basic-chat": BasicChat,
  "/dashboard/combos": Combos,
  "/dashboard/console-log": ConsoleLog,
  "/dashboard/media-providers/web": WebMedia,
  "/dashboard/mitm": Mitm,
  "/dashboard/profile": Profile,
  "/dashboard/providers/new": NewProvider,
  "/dashboard/providers": Providers,
  "/dashboard/proxy-pools": ProxyPools,
  "/dashboard/pxpipe": Pxpipe,
  "/dashboard/quota": Quota,
  "/dashboard/skills": Skills,
  "/dashboard/token-saver": TokenSaver,
  "/dashboard/translator": Translator,
  "/dashboard/usage": Usage,
  "/dashboard/settings/pricing": Pricing,
};

function Page({ machineId }) {
  const pathname = usePathname();
  const params = useParams();
  if (pathname === "/dashboard" || pathname === "/dashboard/endpoint") return <Endpoint machineId={machineId} />;
  if (pathname === "/dashboard/cli-tools") return <CliTools machineId={machineId} />;
  const Component = pages[pathname];
  if (Component) return <Component />;
  if (pathname.startsWith("/dashboard/cli-tools/") && CLI_TOOLS[params.toolId]) {
    return <CliTool toolId={params.toolId} machineId={machineId} />;
  }
  if (pathname.startsWith("/dashboard/providers/") && params.id) return <Provider />;
  if (pathname.startsWith("/dashboard/media-providers/combo/") && params.id) return <MediaCombo />;
  if (params.kind && params.id) return <MediaProvider />;
  if (params.kind) return <Media />;
  return notFound();
}

function App() {
  const pathname = usePathname();
  const [context, setContext] = useState(null);
  const [failed, setFailed] = useState(false);
  useEffect(() => {
    fetch("/api/dashboard/context", { cache: "no-store" })
      .then(response => {
        if (response.status === 401) {
          window.location.replace("/login");
          return null;
        }
        if (!response.ok) throw new Error("Dashboard context unavailable");
        return response.json();
      })
      .then(setContext)
      .catch(() => setFailed(true));
  }, []);
  if (failed) return <p role="alert">Could not load the dashboard. Reload to try again.</p>;
  if (!context) return null;
  return (
    <ThemeProvider>
      <RuntimeI18nProvider>
        <DashboardLayout><Page key={pathname} machineId={context.machineId} /></DashboardLayout>
      </RuntimeI18nProvider>
    </ThemeProvider>
  );
}
const showIcons = () => document.documentElement.classList.add("fonts-loaded");
if (document.fonts?.load) {
  document.fonts.load('24px "Material Symbols Outlined"').then(showIcons, showIcons);
  window.setTimeout(showIcons, 3000);
} else {
  showIcons();
}

createRoot(document.getElementById("app")).render(<App />);
