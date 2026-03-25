import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import type { Plugin } from "vite";
import { readFile } from "fs/promises";

/**
 * Serve /shared/* requests from the /shared directory on disk.
 * In Docker, /shared is a volume mount with rollup.env.
 */
function serveSharedPlugin(): Plugin {
  return {
    name: "serve-shared",
    configureServer(server) {
      server.middlewares.use("/shared", (req, res, next) => {
        const filePath = `/shared${req.url || ""}`;
        readFile(filePath, "utf-8")
          .then((content) => {
            res.setHeader("Content-Type", "text/plain");
            res.end(content);
          })
          .catch(() => next());
      });
    },
  };
}

export default defineConfig({
  plugins: [react(), serveSharedPlugin()],
  server: {
    port: 8080,
    host: "0.0.0.0",
    allowedHosts: true,
  },
  build: {
    outDir: "dist",
  },
});
