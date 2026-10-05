import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useState,
  type ReactNode,
} from "react";
import { apiPost } from "./api";

// Admin session state: /api/auth/session reports whether a cookie
// session is held. When an Admin Credential is configured, every page
// renders the sign-in route instead until one exists.
interface Session {
  authenticated: boolean;
  configured: boolean;
  loading: boolean;
  refresh: () => Promise<void>;
  signOut: () => Promise<void>;
}

const SessionCtx = createContext<Session>({
  authenticated: false,
  configured: true,
  loading: true,
  refresh: async () => {},
  signOut: async () => {},
});

export function useSession() {
  return useContext(SessionCtx);
}

export function SessionProvider({ children }: { children: ReactNode }) {
  const [authenticated, setAuthenticated] = useState(false);
  const [configured, setConfigured] = useState(true);
  const [loading, setLoading] = useState(true);

  const refresh = useCallback(async () => {
    try {
      const r = await fetch("/api/auth/session");
      const state = r.ok ? await r.json() : null;
      setConfigured(Boolean(!state || state.configured));
      // No Admin Credential configured means the backend never gates.
      setAuthenticated(Boolean(state && (state.authenticated || !state.configured)));
    } catch {
      /* no session endpoint - leave unauthenticated */
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    refresh();
  }, [refresh]);

  const signOut = useCallback(async () => {
    try {
      await apiPost("/api/logout", {});
    } finally {
      window.location.href = "/login";
    }
  }, []);

  return (
    <SessionCtx.Provider value={{ authenticated, configured, loading, refresh, signOut }}>
      {children}
    </SessionCtx.Provider>
  );
}
