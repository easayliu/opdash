import { defineConfig, type PluginOption } from 'vite'
import react from '@vitejs/plugin-react-swc'
import tailwindcss from '@tailwindcss/vite'
import path from 'node:path'
import { readFileSync } from 'node:fs'

/** 版本号取自 Cargo.toml（唯一真源），读不到就退回 `dev`——只是页脚的一个字符串，不能让构建挂掉。 */
function readAppVersion(): string {
  try {
    const manifest = readFileSync(path.resolve(__dirname, '../Cargo.toml'), 'utf8')
    return manifest.match(/^version\s*=\s*"([^"]+)"/m)?.[1] ?? 'dev'
  } catch {
    return 'dev'
  }
}

/**
 * 剥掉 Fontsource 的 `.woff` 回退，只留 woff2：这些字体文件会跟着 dist 一起被 rust-embed 编进
 * 二进制，多带一份等价格式等于每个发行版塞进 150 KB 永远不会被请求的字节。
 */
function dropWoff1(): PluginOption {
  return {
    name: 'opdash-drop-woff1',
    transform(code, id) {
      if (!id.includes('.css')) return null
      return { code: code.replace(/,\s*url\([^)]*\)\s*format\(["']woff["']\)/g, ''), map: null }
    },
    generateBundle(_options, bundle) {
      for (const name of Object.keys(bundle)) {
        if (name.endsWith('.woff')) delete bundle[name]
      }
    },
  }
}

// 开发时 /api 代理到本地 opdash 后端（cargo run 默认监听 4880）。
// 后端开了 OIDC 时给它配 OPDASH_PUBLIC_URL=http://localhost:5173，回调才会跳回 vite 这边。
export default defineConfig({
  plugins: [react(), tailwindcss(), dropWoff1()],
  base: '/',
  define: {
    __APP_VERSION__: JSON.stringify(readAppVersion()),
  },
  resolve: {
    alias: { '@': path.resolve(__dirname, './src') },
  },
  server: {
    port: 5173,
    proxy: {
      '/api': { target: 'http://127.0.0.1:4880', changeOrigin: true },
    },
  },
  build: {
    outDir: 'dist',
    emptyOutDir: true,
  },
})
