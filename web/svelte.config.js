import adapter from '@sveltejs/adapter-static';
import { mdsvex } from 'mdsvex';
import { vitePreprocess } from '@sveltejs/vite-plugin-svelte';

/**
 * Nostos web — SvelteKit config.
 *
 * adapter-static → the whole app is prerendered to plain HTML/CSS/JS in `build/`,
 * deployable to Cloudflare Pages (mirrors the Arxa reference deployment). No SSR
 * runtime: the admin hits the nostos-cloud JSON API (/v1/*) client-side; the
 * landing is pure static + a Three.js hero.
 *
 * mdsvex lets us write docs/blog as .md routes (the "migrate from PowerSync"
 * guides land here later).
 *
 * @type {import('@sveltejs/kit').Config}
 */
const config = {
  extensions: ['.svelte', '.md'],
  preprocess: [
    vitePreprocess(),
    mdsvex({ extensions: ['.md'] })
  ],
  kit: {
    adapter: adapter({ pages: 'build', assets: 'build', fallback: '200.html' }),
    // Both routes are static-capable: the landing is pure static + a canvas hero,
    // the admin is a static shell that fetches /v1/* client-side. Prerender them
    // both (no SSR runtime needed). The '200.html' SPA fallback covers any
    // not-yet-prerendered route without clobbering the prerendered index.html.
    prerender: { handleHttpError: 'warn', handleMissingId: 'ignore' }
  }
};

export default config;
