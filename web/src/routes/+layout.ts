// Prerender the whole site as static HTML (adapter-static → Cloudflare Pages).
// Both routes — the landing (pure static + canvas hero) and the admin (static
// shell that fetches /v1/* client-side) — prerender to real HTML. The
// '200.html' SPA fallback in svelte.config.js catches anything not enumerated
// here without clobbering the prerendered index.html.
export const prerender = true;
export const trailingSlash = 'never';
