// The dev server: the app, with chlive's client from its checkout ($CHLIVE_CLIENT), and two
// proxies so the browser talks to one origin: `/live` to chlive, `/api` to control's HTTP API.
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

const client = process.env.CHLIVE_CLIENT ?? "/root/chlive/client";
const chlive = new URL(process.env.CHLIVE_URL ?? "ws://127.0.0.1:8080/live");
const control = process.env.CONTROL ?? "http://127.0.0.1:8090";

export default defineConfig({
  plugins: [react()],
  resolve: {
    alias: [
      { find: /^@chlive\/client\/react$/, replacement: `${client}/src/react.ts` },
      { find: /^@chlive\/client$/, replacement: `${client}/src/index.ts` },
    ],
    // the client's react.ts, outside this package, must get this one's React
    dedupe: ["react", "react-dom"],
  },
  // sqlite-wasm loads its .wasm next to itself; pre-bundling would break that
  optimizeDeps: { exclude: ["@sqlite.org/sqlite-wasm"] },
  worker: { format: "es" },
  server: {
    fs: { allow: [".", client] },
    // also reached through `tailscale serve` (https://<machine>.<tailnet>.ts.net)
    allowedHosts: [".ts.net"],
    proxy: {
      "/live": { target: chlive.origin.replace(/^ws/, "http"), ws: true },
      "/api": { target: control, rewrite: (p) => p.replace(/^\/api/, "") },
    },
  },
});
