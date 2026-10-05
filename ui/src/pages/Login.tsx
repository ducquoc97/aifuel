import { useState, type FormEvent } from "react";
import { Navigate, useLocation, useNavigate } from "react-router-dom";
import { Fuel } from "lucide-react";
import { VERSION } from "@/components/AppShell";
import { useSession } from "@/lib/session";

export default function Login() {
  const [password, setPassword] = useState("");
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const navigate = useNavigate();
  const location = useLocation();
  const { refresh, authenticated, configured, loading } = useSession();

  // No Admin Credential configured - nothing to sign in to.
  if (!loading && (authenticated || !configured)) return <Navigate to="/" replace />;

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setError("");
    setBusy(true);
    try {
      const r = await fetch("/api/login", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ password }),
      });
      if (r.ok) {
        await refresh();
        const from = (location.state as { from?: string } | null)?.from;
        navigate(from && from !== "/login" ? from : "/", { replace: true });
        return;
      }
      let message = "sign-in failed";
      try {
        const body = await r.json();
        if (body && body.error) message = body.error;
      } catch {
        /* keep the generic message */
      }
      setError(message);
    } catch {
      setError("could not reach the server");
    } finally {
      setBusy(false);
    }
  };

  return (
    <main className="login-grid grid min-h-screen place-items-center">
      <div className="w-[min(360px,calc(100vw-32px))] rounded-[18px] border border-line bg-canvas px-7 py-8 shadow-[0_1px_2px_rgba(29,29,31,0.05)]">
        <div className="mb-1.5 flex items-center gap-2.5 text-[15px] font-semibold">
          <span className="flex size-[30px] items-center justify-center rounded-lg bg-accent">
            <Fuel className="size-4 text-white" />
          </span>
          aifuel
        </div>
        <h1 className="mt-4 mb-1 text-[17px] font-semibold">Sign in</h1>
        <p className="mb-5 text-ink-48">This aifuel dashboard requires the admin password.</p>
        <form onSubmit={submit}>
          <label htmlFor="password" className="mb-1.5 block font-medium">
            Admin password
          </label>
          <input
            id="password"
            type="password"
            autoComplete="current-password"
            autoFocus
            required
            value={password}
            onChange={(e) => setPassword(e.target.value)}
            className="w-full rounded-lg border border-line bg-canvas px-3 py-2.5 focus:border-accent focus:outline-none focus:ring-[3px] focus:ring-accent/15"
          />
          <button
            type="submit"
            disabled={busy}
            className="mt-4 w-full rounded-lg bg-accent px-3 py-2.5 font-medium text-white hover:bg-accent-hover disabled:opacity-55"
          >
            Sign in
          </button>
          {error && (
            <div role="alert" className="mt-3.5 rounded-lg bg-err-tint px-3 py-2 text-err">
              {error}
            </div>
          )}
        </form>
        <p className="mt-4 text-center text-xs text-ink-48">aifuel v{VERSION}</p>
      </div>
    </main>
  );
}
