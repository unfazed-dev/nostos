<script lang="ts">
  import { page } from '$app/stores';
  import { Nostos, Button, ThemeToggle } from '$lib/components/ui';

  // On the landing we render a transparent hero nav; elsewhere a solid bar.
  let isLanding = $derived($page.url.pathname === '/');
</script>

<header class:landing={isLanding}>
  <a class="brand" href="/">
    <Nostos size={20} />
    <span>Nostos</span>
  </a>
  <nav>
    <a href="/#how">How it works</a>
    <a href="/#benchmark">Benchmark</a>
    <a href="/#pricing">Pricing</a>
    <a href="/docs">Docs</a>
  </nav>
  <div class="actions">
    <ThemeToggle />
    <Button href="/admin" variant="ghost" size="sm">Sign in</Button>
    <Button href="/#waitlist" variant="mark" size="sm">Get started</Button>
  </div>
</header>

<style>
  header {
    display: flex;
    align-items: center;
    gap: var(--gutter);
    padding: 18px max(var(--gutter), calc((100% - var(--wrap)) / 2 + var(--gutter)));
    border-bottom: 1px solid var(--rule);
    background: color-mix(in srgb, var(--paper) 86%, transparent);
    backdrop-filter: blur(12px);
    position: sticky;
    top: 0;
    z-index: 50;
  }
  header.landing { border-bottom-color: transparent; background: transparent; backdrop-filter: none; }
  .brand {
    display: inline-flex;
    align-items: center;
    gap: 9px;
    font-weight: 800;
    font-size: var(--t-20);
    letter-spacing: -0.02em;
    margin-right: auto;
  }
  nav { display: flex; gap: 26px; }
  nav a { font-size: var(--t-14); font-weight: 500; color: var(--ink-soft); }
  nav a:hover { color: var(--ink); }
  .actions { display: flex; gap: 10px; align-items: center; margin-left: 26px; }
  @media (max-width: 760px) {
    nav { display: none; }
  }
</style>
