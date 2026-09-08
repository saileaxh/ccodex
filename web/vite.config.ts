import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import path from "path";

export default defineConfig({
  plugins: [react()],
  base: "./",
  resolve: {
    alias: { "@": path.resolve(__dirname, "src") },
  },
  server: {
    proxy: {
      "/admin": "http://127.0.0.1:8317",
      "/health": "http://127.0.0.1:8317",
    },
  },
  build: {
    outDir: "dist",
    sourcemap: false,
  },
});
