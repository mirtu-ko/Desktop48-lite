import { fileURLToPath, URL } from 'node:url'
import { defineConfig } from 'vitest/config'

// 与 vite.config.ts 保持同一组别名，测试里可以直接用 @renderer / @common 导入。
export default defineConfig({
  resolve: {
    alias: {
      '@renderer': fileURLToPath(new URL('./src/renderer/src', import.meta.url)),
      '@common': fileURLToPath(new URL('./src/common', import.meta.url)),
      '@preload': fileURLToPath(new URL('./src/preload', import.meta.url)),
    },
  },
  test: {
    include: ['test/**/*.test.ts'],
    environment: 'node',
    setupFiles: ['./test/setup-jsdom.ts'],
    // jsdom 未实现 Window.open / 跨文档导航，会对它们打 "Not implemented"；
    // 这两条路径在用例里断言过了，jsdom 专用的测试文件在开头自行静音。
  },
})
