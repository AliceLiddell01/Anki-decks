import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import { fileURLToPath } from "node:url";
export default defineConfig({
  plugins: [react(), tailwindcss()],
  resolve: { alias: { "@": fileURLToPath(new URL("./src", import.meta.url)) } },
  server: {
    proxy: {
      "/api": {
        target: "http://127.0.0.1:3085",
        changeOrigin: true,
        configure(proxy) {
          proxy.on("proxyReq", (request) => {
            request.removeHeader("origin");
          });
        },
      },
    },
  },
  build: {
    outDir: "dist",
    rollupOptions: {
      output: {
        manualChunks(id) {
          if (
            id.includes("/node_modules/recharts/") ||
            id.includes("/node_modules/d3-") ||
            id.includes("/node_modules/victory-vendor/")
          )
            return "charts";
        },
      },
    },
  },
});
