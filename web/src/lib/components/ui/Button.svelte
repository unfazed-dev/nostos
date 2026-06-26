<script lang="ts">
  import type { Snippet } from 'svelte';

  /**
   * Button — the one button primitive for the whole Nostos web app.
   * Variants: `solid` (ink-on-paper), `mark` (trail-flag accent — ≤3% area rule),
   * `ghost` (hairline outline). Renders as <a> when `href` is given.
   */
  let {
    href,
    variant = 'solid',
    type = 'button',
    size = 'md',
    class: klass = '',
    children,
    ...rest
  }: {
    href?: string;
    variant?: 'solid' | 'mark' | 'ghost';
    type?: 'button' | 'submit';
    size?: 'sm' | 'md';
    class?: string;
    children?: Snippet;
    [key: string]: unknown;
  } = $props();
</script>

{#if href}
  <a {href} class="btn {variant} {size} {klass}" {...rest}>{@render children?.()}</a>
{:else}
  <button {type} class="btn {variant} {size} {klass}" {...rest}>{@render children?.()}</button>
{/if}

<style>
  .btn {
    display: inline-flex;
    align-items: center;
    gap: 8px;
    font-weight: 600;
    letter-spacing: 0.005em;
    white-space: nowrap;
    border: 1px solid transparent;
    border-radius: var(--radius);
    transition:
      background var(--dur) var(--ease),
      border-color var(--dur) var(--ease),
      color var(--dur) var(--ease),
      transform 0.12s var(--ease);
  }
  .btn:active { transform: translateY(1px); }
  .md { padding: 11px 20px; font-size: var(--t-14); }
  .sm { padding: 7px 14px; font-size: var(--t-12); }

  .solid { background: var(--ink); color: var(--paper); }
  .solid:hover { background: oklch(0.22 0.02 250); }

  .mark { background: var(--mark); color: var(--paper); }
  .mark:hover { background: var(--mark-ink); }

  .ghost { border-color: var(--stone-2); color: var(--ink); }
  .ghost:hover { border-color: var(--ink); }
</style>
