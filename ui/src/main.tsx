import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { BrowserRouter } from "react-router-dom";
import { Toaster } from "sonner";
import { SessionProvider } from "@/lib/session";
import App from "./App";
import "./index.css";

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <BrowserRouter>
      <SessionProvider>
        <App />
        <Toaster
          position="bottom-right"
          toastOptions={{
            style: {
              borderLeft: "3px solid var(--ok)",
              borderRadius: "8px",
              fontSize: "12.5px",
            },
          }}
        />
      </SessionProvider>
    </BrowserRouter>
  </StrictMode>,
);
