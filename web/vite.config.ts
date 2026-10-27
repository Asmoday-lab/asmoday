/// <reference types="vite/client" />

import tailwindcss from '@tailwindcss/vite'
import vue from '@vitejs/plugin-vue'
import { defineConfig } from 'vite'

/**
 * Video.js 10 以 HTML 自定义元素方式接入（官方 Vue 指南）：
 * Vue 编译器默认把未知标签当组件解析并告警，这里显式声明。
 * 只列模板中真正用到的标签，避免误吞拼错的 Vue 组件。
 */
const videoJsElements = new Set(['video-player', 'video-neutral-skin'])

// 前端产物输出到仓库根的 dist/，供 rust-embed 打包进单二进制（见设计文档 2.3 / 9.1）
export default defineConfig({
  plugins: [
    vue({
      template: {
        compilerOptions: {
          isCustomElement: (tag) => videoJsElements.has(tag),
        },
      },
    }),
    tailwindcss(),
  ],
  /**
   * 子路径部署适配（设计文档 3.1）：base_url 由 config.toml 运行时决定，
   * 构建期无法写死。JS/CSS 内部的资源引用（含动态 import 与 modulepreload）
   * 一律改成相对所在文件的路径，配合 import.meta.url 在任意前缀下都能解析；
   * index.html 仍输出绝对 /assets/...，由后端 serve 时注入真实前缀。
   */
  experimental: {
    renderBuiltUrl(_filename, { hostType }) {
      return hostType === 'html' ? { relative: false } : { relative: true }
    },
  },
  build: {
    outDir: '../dist',
    emptyOutDir: true,
  },
  server: {
    port: 5173,
    // 开发期把 /api 代理到 Axum，避免跨域（ws: true 供终端 WebSocket 使用）
    proxy: {
      '/api': {
        target: 'http://127.0.0.1:4242',
        changeOrigin: true,
        ws: true,
      },
    },
  },
})
