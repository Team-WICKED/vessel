// The web UI's build (ADR 0017): a static bundle in `dist/`, which
// vessel-web's build script embeds in the `vessel` binary. Nothing here
// reaches the network at run time; fonts are bundled from `pretendard`.
import react from "@vitejs/plugin-react";
import type { Plugin } from "vite";
import { defineConfig } from "vitest/config";

/** xterm.js's stylesheet gives its screen-reader tree a bare `monospace`;
 * the operator's font rule wants a real face and Pretendard before it
 * (scripts/check-fonts.mjs checks the built CSS too). */
function xtermFont(): Plugin {
  return {
    name: "vessel-xterm-font",
    enforce: "pre",
    transform(code, id) {
      if (!id.endsWith("@xterm/xterm/css/xterm.css")) {
        return null;
      }
      return code.replaceAll(
        "font-family: monospace;",
        'font-family: Consolas, "SF Mono", Menlo, "JetBrains Mono", "Cascadia Mono", "DejaVu Sans Mono", "Liberation Mono", "Noto Sans Mono", Pretendard, monospace;',
      );
    },
  };
}

export default defineConfig({
  plugins: [xtermFont(), react()],
  build: {
    outDir: "dist",
    assetsDir: "assets",
    target: "es2022",
    sourcemap: false,
    // The bundle is served from this machine; one chunk per library would
    // only add requests.
    chunkSizeWarningLimit: 2048,
  },
  test: {
    include: ["src/**/*.test.ts", "src/**/*.test.tsx"],
    environment: "node",
  },
});
