import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import path from "node:path";

export default defineConfig({
  // The Rust binary serves the embedded bundle under /ui/, so every
  // emitted asset URL is prefixed with /ui/.
  base: "/ui/",
  plugins: [react(), tailwindcss()],
  resolve: {
    alias: { "@": path.resolve(__dirname, "src") },
  },
  build: {
    outDir: "dist",
    assetsDir: "assets",
  },
  server: {
    port: 5173,
    proxy: {
      "/api": "http://127.0.0.1:8788",
      "/v1": "http://127.0.0.1:8788",
      "/healthz": "http://127.0.0.1:8788",
    },
  },
});
