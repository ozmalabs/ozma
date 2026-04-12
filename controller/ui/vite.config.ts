import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import tailwindcss from '@tailwindcss/vite'
import { resolve } from 'path'

export default defineConfig({
  plugins: [
    react(),
    tailwindcss(),
  ],
  resolve: {
    alias: {
      '@': resolve(__dirname, 'src'),
    },
  },
  server: {
    port: 5173,
    proxy: {
      '/api': {
        target: 'http://localhost:7380',
        changeOrigin: true,
      },
      '/graphql': {
        target: 'http://localhost:7380',
        changeOrigin: true,
      },
      '/ws': {
        target: 'http://localhost:7380',
        ws: true,
        changeOrigin: true,
      },
    },
  },
  build: {
    outDir: '../static/ui',
    emptyOutDir: true,
  },
})
