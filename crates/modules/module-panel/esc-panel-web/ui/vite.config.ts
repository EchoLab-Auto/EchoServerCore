import { defineConfig } from 'vite'
import vue from '@vitejs/plugin-vue'

export default defineConfig({
  plugins: [vue()],
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    cssCodeSplit: false,
    rollupOptions: {
      input: { tunnel: 'src/tunnel.ts', server: 'src/server.ts' },
      output: {
        entryFileNames: '[name].js',
        assetFileNames: 'panel.[ext]',
        manualChunks: undefined,
      },
    },
  },
})
