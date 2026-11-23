import { useMemo, useSyncExternalStore } from "react";
import { isDashboardPage, paramsFor } from "./paths";

const change = "9router:navigation";

function subscribe(listener) {
  window.addEventListener("popstate", listener);
  window.addEventListener(change, listener);
  return () => {
    window.removeEventListener("popstate", listener);
    window.removeEventListener(change, listener);
  };
}

function readUrl() {
  return window.location.href;
}

function useUrl() {
  const href = useSyncExternalStore(subscribe, readUrl);
  return useMemo(() => new URL(href), [href]);
}

function visit(href, options, replace) {
  const destination = new URL(href, window.location.href);
  if (destination.origin !== window.location.origin || !isDashboardPage(destination.pathname)) {
    window.location[replace ? "replace" : "assign"](destination.href);
    return;
  }
  window.history[replace ? "replaceState" : "pushState"](null, "", destination);
  window.dispatchEvent(new Event(change));
  if (options?.scroll !== false) {
    document.querySelector("main > .custom-scrollbar")?.scrollTo(0, 0);
    window.scrollTo(0, 0);
  }
}

const router = {
  push: (href, options) => visit(href, options, false),
  replace: (href, options) => visit(href, options, true),
};

export function useRouter() {
  return router;
}

export function usePathname() {
  return useUrl().pathname;
}

export function useSearchParams() {
  return useUrl().searchParams;
}

export function useParams() {
  const pathname = usePathname();
  return useMemo(() => paramsFor(pathname), [pathname]);
}

export function notFound() {
  return <p className="text-text-muted py-12 text-center">Page not found</p>;
}

export function redirect(href) {
  window.location.replace(href);
  return null;
}

export default function Link({ href, replace, scroll, prefetch, onClick, ...props }) {
  return <a {...props} href={href} onClick={event => {
    onClick?.(event);
    if (event.defaultPrevented || event.button !== 0 || event.altKey || event.shiftKey ||
      event.ctrlKey || event.metaKey || props.download || (props.target && props.target !== "_self")) return;
    const destination = new URL(event.currentTarget.href);
    if (destination.origin !== window.location.origin || !isDashboardPage(destination.pathname)) return;
    event.preventDefault();
    visit(destination.href, { scroll }, replace);
  }} />;
}
