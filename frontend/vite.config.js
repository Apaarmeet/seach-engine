import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

export default defineConfig({
  plugins: [react()],
  server: {
    port: 5173,
    // Proxy search requests to the Rust API so the browser sees a single
    // origin in dev (no CORS preflight, and the prod deployment can put
    // both behind one reverse proxy the same way).
    proxy: {
      '/search': 'http://localhost:8080',
      '/resolve': 'http://localhost:8080',
      '/nearby': 'http://localhost:8080',
      '/health': 'http://localhost:8080',
      '/stats': 'http://localhost:8080',
      '/places-stats': 'http://localhost:8080',
    },
  },
})
