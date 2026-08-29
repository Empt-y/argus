import { defineConfig } from "vite";
import cesium from "vite-plugin-cesium";

// The dev server proxies /v1 to argusd rather than talking to it cross-origin.
// Two reasons, and neither is convenience: it keeps the daemon's CORS policy
// closed (a permissive one on a process holding every provider credential would
// let any page the operator visits read their whole feed history), and it means
// the dev client and a deployed one use byte-identical URLs.
const ARGUS = process.env.ARGUS_URL ?? "http://127.0.0.1:8787";

export default defineConfig({
  plugins: [cesium()],
  server: {
    port: 5174,
    proxy: {
      "/v1": { target: ARGUS, changeOrigin: true, ws: true },
    },
  },
  build: { target: "es2022", sourcemap: true },
});
