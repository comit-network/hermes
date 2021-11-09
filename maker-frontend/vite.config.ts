import react from "@vitejs/plugin-react";
import { resolve } from "path";
import { defineConfig } from "vite";

// https://vitejs.dev/config/
export default defineConfig({
    plugins: [react()],
    build: {
        rollupOptions: {
            input: resolve(__dirname, `index.html`),
        },
        outDir: `dist`,
    },
    server: {
        proxy: {
            "/api": "http://localhost:8001",
            "/alive": `http://localhost:8001`,
        },
    },
});
