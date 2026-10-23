import { defineConfig } from 'vite';

export default defineConfig({
  root: 'src',
  base: './',
  build: {
    outDir: '../dist',
    emptyOutDir: true,
    // Keep both theme JSON files as inspectable offline assets rather than
    // inlining the smaller vintage palette into the application chunk.
    assetsInlineLimit: 0,
  },
});
