import { defineConfig, UserConfig } from 'vite'
import react from '@vitejs/plugin-react'
import path from 'path'

// https://vitejs.dev/config/
export default defineConfig(({ command }) => {
  const isProduction = command === 'build'

  const config: UserConfig = {
    plugins: [react()],
    resolve: {
      alias: {
        '@': path.resolve(__dirname, './src'),
        '@api': path.resolve(__dirname, './src/api'),
        '@store': path.resolve(__dirname, './src/store'),
        '@components': path.resolve(__dirname, './src/components'),
        '@pages': path.resolve(__dirname, './src/pages'),
        '@hooks': path.resolve(__dirname, './src/hooks'),
        '@types': path.resolve(__dirname, './src/types'),
        '@utils': path.resolve(__dirname, './src/utils'),
      },
    },
    server: {
      port: 5173,
      proxy: {
        '/api': {
          target: 'http://localhost:7380',
          changeOrigin: true,
          secure: false,
        },
        '/ws': {
          target: 'http://localhost:7380',
          ws: true,
          changeOrigin: true,
          secure: false,
        },
      },
    },
    build: {
      outDir: 'dist',
      sourcemap: !isProduction,
      minify: isProduction,
      reportCompressedSize: isProduction,
      rollupOptions: {
        output: {
          manualChunks: {
            react: ['react', 'react-dom', 'react-router-dom'],
            zustand: ['zustand'],
          },
        },
      },
    },
    preview: {
      port: 4173,
    },
    css: {
      preprocessorOptions: {
        css: {},
      },
    },
    optimizeDeps: {
      include: ['react', 'react-dom', 'react-router-dom', 'zustand'],
      exclude: [],
    },
    esbuild: {
      loader: 'tsx',
      include: /src\/.*\.tsx$/,
      exclude: /node_modules/,
    },
  }

  return config
})
