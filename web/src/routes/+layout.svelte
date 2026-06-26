<script lang="ts">
  import '$lib/styles/app.css';
  import Nav from '$lib/components/Nav.svelte';
  let { children } = $props();
</script>

<div class="app">
  <Nav />
  <main>
    {@render children?.()}
  </main>
</div>

<style>
  .app { display: flex; flex-direction: column; min-height: 100dvh; }
  main { flex: 1; }
</style>
