import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// In development the API runs separately (apitorrentd on 127.0.0.1:8742); Vite proxies to
// it so the UI is same-origin, exactly as when the daemon serves the built UI itself.
const api = process.env.APITORRENT_API ?? "http://127.0.0.1:8742";

export default defineConfig({
  plugins: [react()],
  server: {
    host: "localhost",
    proxy: {
      "/api": { target: api, ws: true },
    },
  },
  build: {
    target: "es2022",
    sourcemap: true,
  },
});
