import { defineConfig } from "vite";

// Tauri expects a fixed dev-server port and watches `src-tauri` separately.
export default defineConfig({
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    watch: {
      ignored: ["**/src-tauri/**"],
    },
  },
  build: {
    // Two pages: the switcher overlay and the settings window.
    rollupOptions: {
      input: {
        main: "index.html",
        settings: "settings.html",
      },
    },
  },
});
