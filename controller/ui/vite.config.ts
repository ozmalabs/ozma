import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

export default defineConfig({
  plugins: [react()],
  base: './',
  server: {
    port: 5173,
    proxy: {
      '/api': {
        target: 'http://localhost:7380',
        changeOrigin: true,
      },
      '/ws': {
        target: 'http://localhost:7380',
        changeOrigin: true,
        ws: true,
      },
    },
  },
  build: {
    outDir: '../static/ui',
    emptyOutDir: true,
    assetsInlineLimit: 0,
  },
})
