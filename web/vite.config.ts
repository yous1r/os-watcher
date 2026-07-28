import { defineConfig } from 'vite';
import solid from 'vite-plugin-solid';

// Vite 配置：SolidJS 前端，构建产物输出到 dist/，由 Rust 服务托管。
// 开发时通过 proxy 把 /api 转发到本地运行的 os-watcher 节点（默认 7980）。
export default defineConfig({
  plugins: [solid()],
  server: {
    port: 5173,
    proxy: {
      '/api': {
        target: 'http://127.0.0.1:7980',
        changeOrigin: true,
      },
    },
  },
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    target: 'es2020',
  },
});
