import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';

// Tauri 开发时固定端口；TAURI_DEV_HOST 用于手机真机调试
const host = process.env.TAURI_DEV_HOST;

export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    host: host || false,
    hmr: host ? { protocol: 'ws', host, port: 1421 } : undefined,
    watch: { ignored: ['**/src-tauri/**'] },
  },
  build: {
    target: 'es2021',
    chunkSizeWarningLimit: 1200,
  },
});
