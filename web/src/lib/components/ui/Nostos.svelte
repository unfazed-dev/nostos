<script lang="ts">
  /**
   * Nostos — the brand glyph. A stack of offset slabs (the trail-marker-of-stones
   * metaphor, expressed structurally — never as literal rocks). The top "crown"
   * slab is the trail-flag mark colour.
   *
   * Pure SVG so it scales anywhere and inherits currentColor for the stone body.
   */
  let { size = 20, crown = true }: { size?: number; crown?: boolean } = $props();
</script>

<svg
  width={size}
  height={size * 1.3}
  viewBox="0 0 32 42"
  fill="none"
  xmlns="http://www.w3.org/2000/svg"
  aria-hidden="true"
  class="nostos-glyph"
>
  {#if crown}
    <rect x="10" y="0.4" width="9" height="3.4" rx="1" fill="var(--mark)" />
  {/if}
  <rect x="6" y="5.6" width="14" height="3.4" rx="1" fill="currentColor" opacity="0.5" />
  <rect x="4" y="11.2" width="22" height="3.4" rx="1" fill="currentColor" />
  <rect x="7" y="16.8" width="17" height="3.4" rx="1" fill="currentColor" opacity="0.78" />
  <rect x="9" y="22.4" width="13" height="3.4" rx="1" fill="currentColor" opacity="0.5" />
  <rect x="5" y="28" width="20" height="3.4" rx="1" fill="currentColor" />
  <rect x="8" y="33.6" width="15" height="3.4" rx="1" fill="currentColor" opacity="0.6" />
</svg>

<style>
  .nostos-glyph {
    display: inline-block;
    flex: none;
    color: var(--ink);
  }
</style>
