import { fileURLToPath, URL } from 'node:url'
import vue from '@vitejs/plugin-vue'
import { defineConfig } from 'vite'

// 迁入的渲染层代码仍使用 Electron 工程里的别名；这里保持同名，
// 让文件可以原样落盘，改动只发生在进程边界（mainAPI 适配层）。
export default defineConfig({
  plugins: [vue()],
  clearScreen: false,
  resolve: {
    alias: {
      '@renderer': fileURLToPath(new URL('./src/renderer/src', import.meta.url)),
      '@common': fileURLToPath(new URL('./src/common', import.meta.url)),
      '@preload': fileURLToPath(new URL('./src/preload', import.meta.url)),
    },
  },
  server: {
    port: 1420,
    strictPort: true,
    watch: {
      ignored: ['**/src-tauri/**'],
    },
  },
})
