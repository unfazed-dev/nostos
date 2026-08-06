<script lang="ts">
  import { onMount } from 'svelte';
  import { Nostos, Button } from '$lib/components/ui';

  /**
   * /admin — the founder console. Client-side only: calls the nostos-cloud
   * JSON API (/v1/*) which Vite proxies to localhost:9090 in dev and which
   * Cloudflare Pages routes to the nostos-cloud origin in prod. Same tokens +
   * nostos primitive as the landing (one brand, two surfaces).
   *
   * Auth: email/password signup+login sets a session cookie (nostos-cloud owns
   * the hash + cookie). Until you sign in we show the login/signup form; after,
   * we show the dashboard (overview + projects + keys + license + checkout).
   */

  type Me = { email: string; role: string } | null;
  let me = $state<Me>(null);
  let email = $state('');
  let password = $state('');
  let mode = $state<'login' | 'signup'>('signup');
  let busy = $state(false);
  let err = $state<string | null>(null);

  let projects = $state<{ id: string; name: string; tier: string }[]>([]);
  let keys = $state<{ id: string; prefix: string; created_at: string }[]>([]);

  async function api<T>(path: string, init?: RequestInit): Promise<T> {
    const r = await fetch(path, {
      credentials: 'include',
      headers: { 'content-type': 'application/json' },
      ...init
    });
    if (!r.ok) {
      const t = await r.text();
      throw new Error(t || r.statusText);
    }
    return r.status === 204 ? (undefined as T) : ((await r.json()) as T);
  }

  async function submit(e: Event) {
    e.preventDefault();
    busy = true; err = null;
    try {
      await api(`/v1/${mode}`, {
        method: 'POST',
        body: JSON.stringify({ email, password })
      });
      await loadMe();
    } catch (e2) {
      err = (e2 as Error).message;
    } finally {
      busy = false;
    }
  }
  async function logout() {
    try { await api('/v1/logout', { method: 'POST' }); } catch { /* ignore */ }
    me = null; projects = []; keys = [];
  }

  async function loadMe() {
    try {
      me = await api<Me>('/v1/me');
      if (me) {
        try { projects = await api<typeof projects>('/v1/projects'); } catch { /* seeded later */ }
        try { keys = await api<typeof keys>('/v1/keys'); } catch { /* seeded later */ }
      }
    } catch {
      me = null;
    }
  }

  onMount(loadMe);
</script>

<svelte:head>
  <title>Nostos · Admin</title>
</svelte:head>

<section class="admin">
  <header class="admin-head">
    <a class="brand" href="/"><Nostos size={20} /> <span>Nostos</span></a>
    <a class="rules-link" href="/admin/rules">Sync rules</a>
    <span class="sync-pill">
      <span class="glyph"><i></i><i></i><i></i></span>
      {#if me}console · {me.email}{:else}signed out{/if}
    </span>
  </header>

  {#if !me}
    <!-- ===================== auth ===================== -->
    <form class="auth" onsubmit={submit}>
      <h1>{mode === 'signup' ? 'Create your account' : 'Sign in'}</h1>
      <p class="lede">Manage projects, API keys, and your license from one console.</p>
      {#if err}<p class="err">{err}</p>{/if}
      <label>
        Email
        <input type="email" bind:value={email} required autocomplete="email" />
      </label>
      <label>
        Password
        <input type="password" bind:value={password} required autocomplete={mode === 'signup' ? 'new-password' : 'current-password'} />
      </label>
      <Button type="submit" variant="mark" disabled={busy}>{busy ? '…' : mode === 'signup' ? 'Create account' : 'Sign in'}</Button>
      <button type="button" class="switch" onclick={() => (mode = mode === 'signup' ? 'login' : 'signup')}>
        {mode === 'signup' ? 'Already have an account? Sign in' : "Don't have one? Sign up"}
      </button>
    </form>
  {:else}
    <!-- ===================== dashboard ===================== -->
    <div class="dash">
      <div class="panel">
        <div class="label">Overview</div>
        <h2>Welcome back, founder.</h2>
        <p class="lede">Your sync engine is running. Here's the field at a glance.</p>
        <dl class="stats">
          <div><dt>Projects</dt><dd class="tnum">{projects.length}</dd></div>
          <div><dt>API keys</dt><dd class="tnum">{keys.length}</dd></div>
          <div><dt>Role</dt><dd>{me.role}</dd></div>
        </dl>
      </div>

      <div class="panel">
        <div class="label">Projects</div>
        {#if projects.length}
          <ul class="list">
            {#each projects as p (p.id)}
              <li><span class="mono">{p.name}</span> <span class="tier-tag">{p.tier}</span></li>
            {/each}
          </ul>
        {:else}
          <p class="empty">No projects yet. Create one from the CLI: <code>nostos project create</code></p>
        {/if}
      </div>

      <div class="panel">
        <div class="label">API keys</div>
        {#if keys.length}
          <ul class="list">
            {#each keys as k (k.id)}
              <li><span class="mono">{k.prefix}…</span> <span class="muted">added {k.created_at}</span></li>
            {/each}
          </ul>
        {:else}
          <p class="empty">No keys yet.</p>
        {/if}
      </div>

      <div class="panel cta-panel">
        <div class="label">License &amp; billing</div>
        <p class="lede">Upgrade to Pro for managed Cloud sync and reactive-when-connected push.</p>
        <div class="cta-row">
          <Button href="/#pricing" variant="mark">See plans</Button>
          <Button onclick={logout} variant="ghost">Sign out</Button>
        </div>
      </div>
    </div>
  {/if}
</section>

<style>
  .admin { max-width: 880px; margin: 0 auto; padding: 48px var(--gutter) 96px; }
  .admin-head {
    display: flex; align-items: center; gap: 20px;
    margin-bottom: 48px;
  }
  .brand { display: inline-flex; align-items: center; gap: 9px; font-weight: 800; font-size: var(--t-20); letter-spacing: -0.02em; }
  .rules-link { color: var(--ink-soft); font-size: var(--t-14); text-decoration: none; }
  .rules-link:hover { color: var(--mark); }
  .sync-pill { margin-left: auto; }

  .auth {
    max-width: 420px;
    display: grid;
    gap: 16px;
    padding: 36px 32px;
    border: 1px solid var(--rule);
    border-radius: var(--radius-lg);
    background: var(--paper-2);
  }
  .auth h1 { font-size: 1.6rem; font-weight: 640; letter-spacing: -0.02em; }
  .auth label { display: grid; gap: 6px; font-size: 13px; color: var(--ink-soft); }
  .auth input {
    padding: 11px 14px;
    border: 1px solid var(--stone-2);
    border-radius: var(--radius);
    background: var(--paper);
    color: var(--ink);
  }
  .auth input:focus { outline: 2px solid var(--mark); outline-offset: 1px; }
  .err { color: var(--mark-ink); font-size: 13px; }
  .switch { background: none; border: 0; color: var(--ink-soft); font-size: 13px; text-decoration: underline; cursor: pointer; }

  .dash { display: grid; grid-template-columns: 1fr 1fr; gap: 18px; }
  .panel {
    padding: 28px 26px;
    border: 1px solid var(--rule);
    border-radius: var(--radius-lg);
    background: var(--paper-2);
  }
  .panel .label { margin-bottom: 10px; }
  .panel h2 { font-size: 1.35rem; font-weight: 600; letter-spacing: -0.015em; margin-bottom: 8px; }
  .stats { display: grid; grid-template-columns: repeat(3, 1fr); gap: 12px; margin-top: 22px; }
  .stats dt { font-size: 11px; color: var(--stone-3); text-transform: uppercase; letter-spacing: 0.1em; }
  .stats dd { font-size: 1.4rem; font-weight: 600; margin-top: 4px; }
  .list { list-style: none; display: grid; gap: 10px; margin-top: 16px; }
  .list li { display: flex; justify-content: space-between; align-items: center; padding: 10px 14px; border: 1px solid var(--rule); border-radius: var(--radius); }
  .tier-tag { font-family: var(--font-mono); font-size: 11px; color: var(--mark-ink); border: 1px solid color-mix(in srgb, var(--mark) 35%, transparent); padding: 2px 8px; border-radius: 20px; }
  .muted { color: var(--stone-3); font-size: 12px; }
  .empty { color: var(--ink-soft); font-size: 0.95rem; line-height: 1.5; }
  .cta-panel { grid-column: 1 / -1; }
  .cta-row { display: flex; gap: 10px; margin-top: 20px; }

  @media (max-width: 720px) {
    .dash { grid-template-columns: 1fr; }
    .stats { grid-template-columns: 1fr; gap: 8px; }
  }
</style>
