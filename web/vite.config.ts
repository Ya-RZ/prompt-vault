import react from '@vitejs/plugin-react';
import { defineConfig } from 'vite';

/** 主服务（Rust/Salvo）监听地址；开发期由 Vite 代理过去，生产由后端直接服务 dist。 */
const BACKEND_ORIGIN = 'http://127.0.0.1:8918';

export default defineConfig({
  // INTERFACES.md §0：/assets/* 由 rust-embed 提供，静态资源必须挂在根路径下。
  base: '/',
  plugins: [react()],
  build: {
    // 产物固定落在 web/dist/，Rust 侧 rust-embed 打包该目录。
    outDir: 'dist',
    emptyOutDir: true,
    sourcemap: false,
    chunkSizeWarningLimit: 1500,
    rollupOptions: {
      output: {
        // Vite 8 底层是 Rolldown，manualChunks 只接受函数形式
        manualChunks: (id: string) => {
          if (!id.includes('node_modules')) return undefined;
          if (
            id.includes('/antd/') ||
            id.includes('@ant-design') ||
            id.includes('@rc-component') ||
            id.includes('/rc-')
          ) {
            return 'antd';
          }
          if (
            id.includes('/react/') ||
            id.includes('/react-dom/') ||
            id.includes('/scheduler/') ||
            id.includes('react-router')
          ) {
            return 'react';
          }
          return 'vendor';
        },
      },
    },
  },
  server: {
    // 注意：任务要求不长期占用 dev server，本配置仅用于按需手动 `pnpm dev`。
    port: 5273,
    strictPort: false,
    proxy: {
      '/api': { target: BACKEND_ORIGIN, changeOrigin: true },
      '/media': { target: BACKEND_ORIGIN, changeOrigin: true },
    },
  },
  preview: {
    port: 5273,
    proxy: {
      '/api': { target: BACKEND_ORIGIN, changeOrigin: true },
      '/media': { target: BACKEND_ORIGIN, changeOrigin: true },
    },
  },
});
