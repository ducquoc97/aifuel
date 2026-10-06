import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useRef,
  useState,
  type ReactNode,
} from "react";
import { Link, NavLink, Outlet, useLocation } from "react-router-dom";
import {
  Fuel,
  Gauge,
  Webhook,
  Network,
  Route as RouteIcon,
  Key,
  ScrollText,
  KeyRound,
  Search,
  ChevronLeft,
  Menu,
  Power,
  LogOut,
  type LucideIcon,
} from "lucide-react";
import { useSession } from "@/lib/session";
import { cn } from "@/lib/utils";

export const VERSION =
  (typeof document !== "undefined" &&
    document.querySelector('meta[name="aifuel-version"]')?.getAttribute("content")) ||
  "dev";

interface NavItem {
  to: string;
  nav: string;
  label: string;
  icon: LucideIcon;
  group: "top" | "gateway" | "manage";
  title: string;
  desc: string;
}

export const NAV: NavItem[] = [
  { to: "/", nav: "usage", label: "Usage", icon: Gauge, group: "top",
    title: "Usage", desc: "Remaining quota across your AI coding providers, soonest reset first." },
  { to: "/gateway", nav: "connect", label: "Connect", icon: Webhook, group: "gateway",
    title: "Connect", desc: "Point OpenAI- or Anthropic-compatible clients at this gateway." },
  { to: "/gateway/providers", nav: "providers", label: "Providers", icon: Network, group: "gateway",
    title: "Providers", desc: "Executable integrations the gateway can route requests to." },
  { to: "/gateway/routes", nav: "routes", label: "Routes", icon: RouteIcon, group: "gateway",
    title: "Routes", desc: "Named aliases and failover combos stored in gateway.json." },
  { to: "/gateway/keys", nav: "keys", label: "API Keys", icon: Key, group: "gateway",
    title: "API Keys", desc: "Gateway keys that authenticate clients, with model allowlists." },
  { to: "/gateway/logs", nav: "logs", label: "Logs", icon: ScrollText, group: "gateway",
    title: "Request Log", desc: "Latest gateway requests, newest first." },
  { to: "/credentials", nav: "credentials", label: "Credentials", icon: KeyRound, group: "manage",
    title: "Credentials", desc: "Store API keys or session credentials for integrations." },
];

const GROUPS: { key: NavItem["group"]; label?: string }[] = [
  { key: "top" },
  { key: "gateway", label: "Gateway" },
  { key: "manage", label: "Manage" },
];

// Pages pin their topbar actions (refresh button, status text) through
// this context; the shell renders them so the header stays consistent.
const HeaderActionsCtx = createContext<{ setActions: (n: ReactNode) => void }>({
  setActions: () => {},
});
export const useHeaderActions = () => useContext(HeaderActionsCtx);

const COLLAPSED_KEY = "aifuel.sidebar.collapsed";

export function AppShell() {
  const location = useLocation();
  const { authenticated, configured, signOut } = useSession();
  const [collapsed, setCollapsed] = useState(
    () => localStorage.getItem(COLLAPSED_KEY) === "1",
  );
  const [drawer, setDrawer] = useState(false);
  const [query, setQuery] = useState("");
  const [actions, setActions] = useState<ReactNode>(null);
  const searchRef = useRef<HTMLInputElement>(null);

  const pageMeta =
    NAV.find((n) => n.to === location.pathname) ||
    NAV.find((n) => n.to === "/gateway" && location.pathname.startsWith("/gateway")) ||
    NAV[0];

  const toggleCollapsed = () => {
    setCollapsed((c) => {
      localStorage.setItem(COLLAPSED_KEY, c ? "0" : "1");
      return !c;
    });
  };

  // "/" focuses the nav filter, OmniRoute-style.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const tag = (document.activeElement?.tagName || "").toLowerCase();
      if (e.key === "/" && !/^(input|textarea|select)$/.test(tag)) {
        e.preventDefault();
        searchRef.current?.focus();
      }
      if (e.key === "Escape") setDrawer(false);
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, []);

  useEffect(() => setDrawer(false), [location.pathname]);
  useEffect(() => setActions(null), [location.pathname]);

  const q = query.trim().toLowerCase();
  const visible = NAV.filter(
    (n) => !q || n.label.toLowerCase().includes(q),
  );

  const quit = async () => {
    if (!window.confirm("Stop the aifuel server? The dashboard and /v1 gateway will shut down."))
      return;
    try {
      await fetch("/api/shutdown", { method: "POST" });
    } catch {
      /* the socket dies with the server - that is the point */
    }
    document.getElementById("root")!.innerHTML =
      '<div style="max-width:1160px;margin:60px auto;padding:40px;text-align:center;color:#6b6b70;background:#fff;border:.5px solid #e0e0e0;border-radius:18px">aifuel has stopped. You can close this tab.</div>';
  };

  return (
    <div className="flex min-h-screen">
      {/* Scrim for the mobile drawer */}
      {drawer && (
        <div
          className="fixed inset-0 z-40 bg-ink/30 lg:hidden"
          onClick={() => setDrawer(false)}
        />
      )}

      <aside
        aria-label="Primary"
        className={cn(
          "fixed inset-y-0 left-0 z-50 flex flex-col border-r border-line bg-parchment transition-all duration-200",
          collapsed ? "w-16" : "w-[232px]",
          "max-lg:translate-x-[-100%] max-lg:w-[232px] max-lg:shadow-2xl",
          drawer && "max-lg:translate-x-0",
        )}
      >
        <div className={cn("flex items-center justify-between px-3.5 pt-3.5 pb-2.5", collapsed && "justify-center")}>
          <Link to="/" className="flex items-center gap-2.5 text-ink min-w-0" aria-label="aifuel usage dashboard">
            <span className="flex size-7 flex-none items-center justify-center rounded-lg bg-accent">
              <Fuel className="size-4 text-white" />
            </span>
            {!collapsed && <span className="text-[15px] font-bold tracking-[-0.011em] whitespace-nowrap">aifuel</span>}
          </Link>
          {!collapsed && (
            <button
              className="inline-flex size-7 items-center justify-center rounded-lg text-ink-48 hover:bg-ink/[0.06] hover:text-ink max-lg:hidden"
              aria-label="Collapse sidebar"
              onClick={toggleCollapsed}
            >
              <ChevronLeft className="size-4" />
            </button>
          )}
        </div>
        {collapsed && (
          <div className="flex justify-center pb-2">
            <button
              className="inline-flex size-7 items-center justify-center rounded-lg text-ink-48 hover:bg-ink/[0.06] hover:text-ink max-lg:hidden"
              aria-label="Expand sidebar"
              onClick={toggleCollapsed}
            >
              <ChevronLeft className="size-4 rotate-180" />
            </button>
          </div>
        )}

        {!collapsed && (
          <div className="mx-3 mt-1 mb-3 flex items-center gap-1.5 rounded-lg border-[0.5px] border-line bg-ink/[0.04] px-2.5 py-1.5">
            <Search className="size-3.5 text-ink-ter" />
            <input
              ref={searchRef}
              type="search"
              placeholder="Search"
              aria-label="Filter navigation"
              autoComplete="off"
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              className="w-full min-w-0 appearance-none border-none bg-transparent text-[12.5px] tracking-[-0.004em] text-ink outline-none placeholder:text-ink-ter [&::-webkit-search-cancel-button]:hidden"
            />
            <kbd className="rounded border-[0.5px] border-line px-1.5 text-[11px] leading-5 text-ink-ter">/</kbd>
          </div>
        )}

        <nav className="flex-1 overflow-y-auto px-2 pb-2">
          {GROUPS.map(({ key, label }) => {
            const items = visible.filter((n) => n.group === key);
            if (!items.length) return null;
            return (
              <div key={key}>
                {label && !collapsed && (
                  <div className="mx-2.5 mt-3.5 mb-1 text-[10.5px] font-bold uppercase tracking-[0.07em] text-ink-ter">
                    {label}
                  </div>
                )}
                {label && collapsed && <div className="my-2 h-px bg-line mx-2" />}
                {items.map((n) => (
                  <NavLink
                    key={n.to}
                    to={n.to}
                    end={n.to === "/" || n.to === "/gateway"}
                    title={n.label}
                    className={({ isActive }) =>
                      cn(
                        "my-0.5 flex items-center gap-2.5 rounded-lg px-2.5 py-[7px] text-[13px] font-medium tracking-[-0.008em] text-ink-80 whitespace-nowrap transition-colors",
                        isActive
                          ? "bg-accent-tint text-accent font-semibold"
                          : "hover:bg-ink/[0.06] hover:text-ink",
                        collapsed && "justify-center px-2",
                      )
                    }
                  >
                    <n.icon className="size-[19px] shrink-0 text-current opacity-80" />
                    {!collapsed && <span className="truncate">{n.label}</span>}
                  </NavLink>
                ))}
              </div>
            );
          })}
        </nav>

        <div className="flex items-center justify-between border-t border-line px-3 py-2.5">
          {!collapsed && (
            <span className="text-[11px] text-ink-ter whitespace-nowrap tabular-nums">
              aifuel v{VERSION}
            </span>
          )}
          <div className={cn("flex items-center gap-1", collapsed && "mx-auto flex-col")}>
            {authenticated && configured && (
              <button
                className="inline-flex items-center gap-1.5 rounded-lg px-2 py-1 text-xs font-medium text-ink-48 hover:bg-err-tint hover:text-err"
                title="Sign out of the dashboard"
                onClick={signOut}
              >
                <LogOut className="size-3.5" />
                {!collapsed && "Sign out"}
              </button>
            )}
            <button
              className="inline-flex items-center gap-1.5 rounded-lg px-2 py-1 text-xs font-medium text-ink-48 hover:bg-err-tint hover:text-err"
              title="Stop the aifuel server"
              onClick={quit}
            >
              <Power className="size-3.5" />
              {!collapsed && "Quit"}
            </button>
          </div>
        </div>
      </aside>

      <div
        className={cn(
          "flex min-h-screen min-w-0 flex-1 flex-col transition-[margin] duration-200",
          collapsed ? "lg:ml-16" : "lg:ml-[232px]",
        )}
      >
        <header className="sticky top-0 z-30 flex min-h-[61px] items-center gap-3.5 border-b border-line bg-white/85 px-6 backdrop-blur-md backdrop-saturate-150 max-lg:px-4">
          <button
            className="hidden size-7 items-center justify-center rounded-lg text-ink-48 hover:bg-ink/[0.06] max-lg:inline-flex"
            aria-label="Open navigation"
            onClick={() => setDrawer(true)}
          >
            <Menu className="size-4" />
          </button>
          <div className="flex min-w-0 items-center gap-3">
            <span className="flex size-[30px] flex-none items-center justify-center rounded-lg bg-accent-tint">
              <pageMeta.icon className="size-[17px] text-accent" />
            </span>
            <div className="min-w-0">
              <h1 className="m-0 truncate text-[14.5px] font-bold leading-[1.3] tracking-[-0.01em]">
                {pageMeta.title}
              </h1>
              <p className="m-0 max-w-[58vw] truncate text-xs text-ink-48 tracking-[-0.004em] max-sm:hidden">
                {pageMeta.desc}
              </p>
            </div>
          </div>
          <div className="ml-auto flex flex-none items-center gap-2.5">{actions}</div>
        </header>

        <main className="content-grid flex-1 p-6 max-lg:p-4">
          <div className="mx-auto max-w-[1160px]">
            <HeaderActionsCtx.Provider value={{ setActions }}>
              <Outlet />
            </HeaderActionsCtx.Provider>
          </div>
        </main>
      </div>
    </div>
  );
}
