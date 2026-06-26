<script lang="ts">
  import { theme, type ThemeMode } from '$lib/stores/theme.svelte';

  /**
   * ThemeToggle — a three-way segmented control (System / Light / Dark).
   * System follows the OS; Light/Dark are explicit overrides that persist.
   * Shares design tokens with the rest of the chrome.
   */
  const modes: { id: ThemeMode; label: string }[] = [
    { id: 'system', label: 'System' },
    { id: 'light', label: 'Light' },
    { id: 'dark', label: 'Dark' }
  ];
</script>

<div class="theme-switch" role="group" aria-label="Theme">
  {#each modes as m (m.id)}
    <button
      type="button"
      aria-pressed={theme.mode === m.id}
      class:active={theme.mode === m.id}
      onclick={() => theme.set(m.id)}>{m.label}</button>
  {/each}
</div>

<style>
  .theme-switch {
    display: inline-flex;
    border: 1px solid var(--stone-2);
    border-radius: var(--radius);
    overflow: hidden;
    font-family: var(--font-mono);
    font-size: 11px;
  }
  .theme-switch button {
    background: none;
    border: 0;
    padding: 6px 10px;
    color: var(--ink-soft);
    transition: background var(--dur) var(--ease), color var(--dur) var(--ease);
  }
  .theme-switch button:hover { color: var(--ink); }
  .theme-switch button.active { background: var(--ink); color: var(--paper); }
</style>
