import { Navigate, Route, Routes, useLocation } from "react-router-dom";
import { AppShell } from "@/components/AppShell";
import { useSession } from "@/lib/session";
import Login from "@/pages/Login";
import Usage from "@/pages/Usage";
import Credentials from "@/pages/Credentials";
import Connect from "@/pages/gateway/Connect";
import Providers from "@/pages/gateway/Providers";
import RoutesPage from "@/pages/gateway/Routes";
import Keys from "@/pages/gateway/Keys";
import Logs from "@/pages/gateway/Logs";

function RequireAuth({ children }: { children: React.ReactNode }) {
  const { authenticated, loading } = useSession();
  const location = useLocation();
  if (loading) return null;
  if (!authenticated)
    return <Navigate to="/login" state={{ from: location.pathname }} replace />;
  return children;
}

export default function App() {
  return (
    <Routes>
      <Route path="/login" element={<Login />} />
      <Route
        element={
          <RequireAuth>
            <AppShell />
          </RequireAuth>
        }
      >
        <Route path="/" element={<Usage />} />
        <Route path="/credentials" element={<Credentials />} />
        <Route path="/gateway" element={<Connect />} />
        <Route path="/gateway/connect" element={<Connect />} />
        <Route path="/gateway/providers" element={<Providers />} />
        <Route path="/gateway/routes" element={<RoutesPage />} />
        <Route path="/gateway/keys" element={<Keys />} />
        <Route path="/gateway/logs" element={<Logs />} />
        <Route path="*" element={<Navigate to="/" replace />} />
      </Route>
    </Routes>
  );
}
