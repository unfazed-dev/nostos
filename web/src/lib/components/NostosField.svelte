<script lang="ts">
  import { onMount } from 'svelte';
  import type { Snippet } from 'svelte';
  import { browser } from '$app/environment';

  /**
   * NostosField — the macro scale of The Nostos Field.
   * -----------------------------------------------------------------------------
   * A top-down canvas: a Postgres commit drops at the origin (bottom-centre),
   * ripples radiate outward, and each device-nostos in a wave's path fills
   * slab-by-slab (bottom→top, crown last) as its LSN checkpoint arrives.
   *
   * This is the blend of Concept 1 (a nostos slab lighting up) and Concept 2
   * (a device-node lighting up as a ripple passes): the same primitive — a
   * checkpoint reaching a device — expressed at two scales. The cairns drawn
   * here share the silhouette of the Nostos logo glyph and the handoff nostos.
   *
   * Contracts:
   *  · exposes reactive `ops` + `synced` for the parent HUD/copy (via callbacks)
   *  · accepts `opsSlot` / `syncedSlot` snippets so the parent controls HUD formatting
   *  · re-reads CSS tokens on theme change so dark/light flip live
   *  · gates the rAF loop on IntersectionObserver + visibilitychange (no burn off-screen)
   *  · respects prefers-reduced-motion (ripples snap, field lights on load)
   */

  let {
    onsynced = (_n: number, _total: number): void => {},
    onops = (_ops: number): void => {},
    opsSlot,
    syncedSlot
  }: {
    onsynced?: (synced: number, total: number) => void;
    onops?: (ops: number) => void;
    opsSlot?: Snippet;
    syncedSlot?: Snippet;
  } = $props();

  let canvas: HTMLCanvasElement;
  let stage: HTMLDivElement;
  let ready = $state(false);

  // expose a flood() to the parent (the "Run the benchmark" button)
  export function flood() {
    send('flood');
  }

  // cross-component message bus (parent clicks the CTA → we flood)
  let bus: ((kind: 'flood' | 'pulse') => void) | null = null;
  function send(kind: 'flood' | 'pulse') {
    bus?.(kind);
  }

  onMount(() => {
    let disposed = false;
    let cleanup: (() => void) | null = null;
    if (!browser) return;

    (async () => {
      const ctx = canvas.getContext('2d');
      if (!ctx) return;

      let W = 0, H = 0, dpr = 1;
      let origin = { x: 0, y: 0 };

      // ---- device-nostoss: deterministic scatter in a fan above the origin ----
      // Each device owns per-slab activation timers so its slabs light
      // STAGGERED, one-by-one, when a ripple reaches THAT stack — independent
      // per-device. (The "one-by-one" the founder asked for: each slab is its
      // own short animation keyed off arrival, not a single eased scalar.)
      type Device = {
        nx: number; ny: number;
        slabs: number;
        hitAt: number;        // timestamp the ripple first reached this stack (0 = never)
        slabLit: number[];    // per-slab 0..1 activation (animated on its own timer)
      };
      const DEVICES: Device[] = [];
      const N = 16;
      for (let i = 0; i < N; i++) {
        const col = i % 5;
        const row = Math.floor(i / 5);
        const fx = (col + 0.5 + ((i * 7) % 3) * 0.18 - 0.6) / 4.2;
        const fy = 0.12 + row * 0.24 + ((i * 13) % 5) * 0.03;
        const d: Device = {
          nx: 0.08 + Math.max(0, Math.min(1, fx)) * 0.84,
          ny: 0.08 + Math.min(0.78, fy) * 0.78,
          slabs: 4 + (i % 4),
          hitAt: 0,
          slabLit: new Array(4 + (i % 4)).fill(0)
        };
        DEVICES.push(d);
      }

      // max distance from origin to any device — ripples are sized so one ring
      // visibly reaches the FURTHEST nostos, then dies. Recomputed on resize.
      let maxDeviceDist = 1;
      function recomputeMaxDist() {
        let m = 0;
        for (const d of DEVICES) {
          m = Math.max(m, Math.hypot(d.nx * W - origin.x, d.ny * H - origin.y));
        }
        maxDeviceDist = m;
      }

      // ---- token reader (re-read on theme change so flips are live) ----
      function vars() {
        const cs = getComputedStyle(document.documentElement);
        const g = (n: string) => cs.getPropertyValue(n).trim();
        return {
          ink: g('--ink'), inkSoft: g('--ink-soft'),
          stone1: g('--stone-1'), stone2: g('--stone-2'), stone3: g('--stone-3'),
          mark: g('--mark'), paper2: g('--paper-2')
        };
      }

      function resize() {
        if (!ctx) return;
        const r = canvas.getBoundingClientRect();
        dpr = Math.min(window.devicePixelRatio || 1, 2);
        canvas.width = Math.max(1, r.width) * dpr;
        canvas.height = Math.max(1, r.height) * dpr;
        ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
        W = r.width; H = r.height;
        origin = { x: W * 0.5, y: H * 0.92 };
        recomputeMaxDist();
      }

      // ---- ripples ----
      // A ripple is a DISCRETE EVENT (one commit reaching its devices), not
      // ambient pond motion. So: each ring expands fast, reaches the furthest
      // nostos, then fades out over 400ms and is gone. No ambient auto-emit —
      // the field rests when idle (clicks + "Run the benchmark" drive events).
      type Ripple = { r: number; born: number; reach: number };
      let ripples: Ripple[] = [];
      const reduce = window.matchMedia('(prefers-reduced-motion: reduce)').matches;

      // expansion speed: fast enough to read as a decisive signal, not water.
      // reach = the radius at which the ring has "delivered" (furthest nostos + margin);
      // after that it fades over RIPPLE_FADE ms and dies.
      const RIPPLE_SPEED = 620;     // px/sec — decisive, not watery
      const RIPPLE_MARGIN = 36;     // px past the furthest nostos
      const RIPPLE_FADE = 400;      // ms fade after reaching `reach`

      function emit() {
        const reach = maxDeviceDist + RIPPLE_MARGIN;
        ripples.push({ r: 0, born: performance.now(), reach });
      }
      function flood() {
        // a burst of staggered ripples — each is its own discrete event
        let n = 0;
        const id = window.setInterval(() => { emit(); if (++n >= 7) window.clearInterval(id); }, 260);
      }
      bus = (kind) => { if (kind === 'flood') flood(); else emit(); };
      if (disposed) return;

      // ---- ops counter (eases toward a target when a ripple reaches a device) ----
      let ops = 0, opsTarget = 0;

      // ---- draw a single device-nostos — THE shared primitive ----
      // Each slab is lit by its own 0..1 value (slabLit[i]) so the stack lights
      // staggered, one slab at a time. The crown (top) slab gets a one-shot
      // shadowBlur flash when it activates — the "checkpoint arrived" signal.
      function drawNostos(
        x: number, y: number,
        slabLit: number[],
        V: ReturnType<typeof vars>,
        now: number, hitAt: number
      ) {
        const slabs = slabLit.length;
        const slabH = 4.5, gap = 1.8, baseW = 17;
        const totalH = slabs * (slabH + gap);
        const top = y - totalH;
        const anyLit = slabLit.some((v) => v > 0.05);
        for (let i = 0; i < slabs; i++) {
          const lit = slabLit[i];
          const isCrown = i === slabs - 1;
          const f = i / (slabs - 1);            // 0 bottom .. 1 top
          const w = baseW * (1 - f * 0.62);     // narrowing silhouette
          const sy = top + i * (slabH + gap);
          if (lit > 0.02) {
            ctx!.fillStyle = isCrown ? V.mark : V.ink;
            ctx!.globalAlpha = lit;
            if (isCrown && hitAt) {
              // crown one-shot flash: bright shadowBlur for 150ms, fading over 250ms
              const sinceCrown = now - (hitAt + (slabs - 1) * 70);
              const flash = sinceCrown > 0 ? Math.max(0, 1 - sinceCrown / 250) : 0;
              ctx!.save();
              ctx!.shadowColor = V.mark;
              ctx!.shadowBlur = 8 + flash * 10; // 8px idle glow, 18px on the flash
              ctx!.fillRect(x - w / 2, sy, w, slabH);
              ctx!.restore();
            } else {
              ctx!.fillRect(x - w / 2, sy, w, slabH);
            }
            ctx!.globalAlpha = 1;
          } else {
            ctx!.fillStyle = V.stone1;
            ctx!.fillRect(x - w / 2, sy, w, slabH);
          }
        }
        // ground shadow — only once the stack has begun to light
        if (anyLit) {
          ctx!.fillStyle = V.ink;
          ctx!.globalAlpha = 0.1;
          ctx!.beginPath();
          ctx!.ellipse(x, y + 2, baseW / 2 + 2, 2.5, 0, 0, Math.PI * 2);
          ctx!.fill();
          ctx!.globalAlpha = 1;
        }
      }

      function drawConnections(V: ReturnType<typeof vars>) {
        ctx!.lineWidth = 1;
        for (const d of DEVICES) {
          const dx = d.nx * W, dy = d.ny * H;
          const litRatio = d.slabLit.reduce((a, b) => a + b, 0) / d.slabs;
          ctx!.strokeStyle = litRatio > 0.15 ? V.mark : V.stone1;
          ctx!.globalAlpha = litRatio > 0.15 ? 0.18 + litRatio * 0.15 : 0.32;
          ctx!.beginPath();
          ctx!.moveTo(origin.x, origin.y);
          ctx!.lineTo(dx, dy);
          ctx!.stroke();
        }
        ctx!.globalAlpha = 1;
      }

      // ---- main loop ----
      let raf: number | null = null;
      let inView = true;

      function frame(now: number) {
        raf = null;
        const V = vars();
        ctx!.clearRect(0, 0, W, H);

        // (no ambient auto-emit — ripples are discrete events: click or flood only)

        ripples = ripples.filter((rp) => {
          const ageMs = now - rp.born;
          rp.r = (ageMs / 1000) * (reduce ? 99999 : RIPPLE_SPEED);
          // alpha: full while expanding, then fade over RIPPLE_FADE ms past reach
          let alpha;
          if (rp.r <= rp.reach) {
            alpha = 0.5;
          } else {
            const fadeT = Math.min(1, ((rp.r - rp.reach) / RIPPLE_SPEED) * (1000 / RIPPLE_FADE));
            alpha = 0.5 * (1 - fadeT);
          }
          if (alpha > 0.01) {
            ctx!.strokeStyle = V.mark;
            ctx!.globalAlpha = alpha;
            ctx!.lineWidth = 1.4;
            ctx!.beginPath();
            ctx!.arc(origin.x, origin.y, Math.max(0.1, rp.r), 0, Math.PI * 2);
            ctx!.stroke();
            ctx!.globalAlpha = 1;
          }
          // when the wavefront reaches a device, mark it hit (once) → its slabs
          // begin staggered activation on their own timers, independent per-device
          for (const d of DEVICES) {
            if (d.hitAt) continue; // already lit by an earlier ripple
            const dist = Math.hypot(d.nx * W - origin.x, d.ny * H - origin.y);
            if (rp.r >= dist) {
              d.hitAt = now;
              opsTarget = 142318;
            }
          }
          // keep the ripple alive until it has fully faded past reach
          return alpha > 0.01;
        });

        drawConnections(V);

        // per-slab staggered activation: each slab lights one-by-one (bottom→top),
        // ~70ms apart, on its own short easeOutBack timer measured from d.hitAt.
        const STAGGER = 70;     // ms between slabs
        const SLAB_DUR = 220;   // ms per slab's 0→1 activation
        let synced = 0;
        for (const d of DEVICES) {
          if (d.hitAt) {
            let allFull = true;
            for (let i = 0; i < d.slabs; i++) {
              const slabStart = d.hitAt + i * STAGGER;
              const t = reduce ? 1 : Math.max(0, Math.min(1, (now - slabStart) / SLAB_DUR));
              // easeOutBack — a small overshoot gives the "snap / click" feel
              const c1 = 1.70158, c3 = c1 + 1;
              const eased = reduce ? 1 : 1 + c3 * Math.pow(t - 1, 3) + c1 * Math.pow(t - 1, 2);
              d.slabLit[i] = Math.max(0, Math.min(1, eased));
              if (d.slabLit[i] < 0.95) allFull = false;
            }
            if (allFull) synced++;
          }
          drawNostos(d.nx * W, d.ny * H, d.slabLit, V, now, d.hitAt);
        }

        // origin — postgres source, warm pulsing core
        const pulse = 0.5 + 0.5 * Math.sin(now / 300);
        ctx!.fillStyle = V.mark;
        ctx!.globalAlpha = 0.18 + pulse * 0.16;
        ctx!.beginPath();
        ctx!.arc(origin.x, origin.y, 12 + pulse * 3, 0, Math.PI * 2);
        ctx!.fill();
        ctx!.globalAlpha = 1;
        ctx!.fillStyle = V.mark;
        ctx!.fillRect(origin.x - 14, origin.y - 2.5, 28, 5);

        // ease ops + decay so it breathes up/down
        ops += (opsTarget - ops) * 0.08;
        opsTarget *= 0.985;
        if (ops < 5) ops = 0;

        onops(Math.round(ops));
        onsynced(synced, DEVICES.length);

        if (inView && !document.hidden) raf = requestAnimationFrame(frame);
      }
      function start() {
        if (raf === null && inView && !document.hidden) raf = requestAnimationFrame(frame);
      }

      // ---- observers: pause when off-screen / tab hidden (no burn) ----
      const ro = new ResizeObserver(resize);
      ro.observe(canvas);
      const io = new IntersectionObserver(
        (e) => { inView = e[0].isIntersecting; if (inView) start(); },
        { threshold: 0.05 }
      );
      io.observe(canvas);
      const onVis = () => start();
      document.addEventListener('visibilitychange', onVis);

      // re-read tokens on theme change (the token blocks flip → live recolour)
      const themeObs = new MutationObserver(() => { /* vars() re-reads each frame */ });
      themeObs.observe(document.documentElement, { attributes: true, attributeFilter: ['data-theme'] });

      canvas.addEventListener('click', emit);

      resize();
      ready = true;
      // welcome pulse
      window.setTimeout(emit, 500);
      start();

      cleanup = () => {
        if (raf !== null) cancelAnimationFrame(raf);
        io.disconnect();
        ro.disconnect();
        themeObs.disconnect();
        document.removeEventListener('visibilitychange', onVis);
        canvas.removeEventListener('click', emit);
        bus = null;
      };
    })();

    return () => {
      disposed = true;
      cleanup?.();
    };
  });
</script>

<div
  class="field-stage"
  bind:this={stage}
  role="img"
  aria-label="Top-down constellation of device-nostoss. A commit at the bottom sends ripples outward; each device-nostos fills its stacked slabs bottom-to-top as the ripple reaches it. Click to send a commit."
>
  <canvas bind:this={canvas} class:ready aria-hidden="true"></canvas>

  <div class="hud tl">
    <div class="k">Throughput</div>
    <div class="v tnum">{#if opsSlot}{@render opsSlot()}{:else}0{/if}</div>
    <div class="k">ops / sec</div>
  </div>
  <div class="hud tr">
    <div class="k">Devices synced</div>
    <div class="v tnum accent">{#if syncedSlot}{@render syncedSlot()}{:else}0 / 0{/if}</div>
  </div>
  <div class="source-tag">postgres · origin</div>
</div>

<style>
  .field-stage {
    position: relative;
    aspect-ratio: 1 / 1;
    border-radius: var(--radius-lg);
    background:
      radial-gradient(ellipse at 50% 88%, color-mix(in srgb, var(--field-veil) 7%, transparent) 0%, transparent 55%),
      var(--paper-2);
    border: 1px solid var(--rule);
    overflow: hidden;
  }
  canvas {
    position: absolute;
    inset: 0;
    width: 100%;
    height: 100%;
    display: block;
    cursor: crosshair;
    opacity: 0;
    transition: opacity 0.6s var(--ease);
  }
  canvas.ready { opacity: 1; }

  .hud {
    position: absolute;
    font-family: var(--font-mono);
    font-size: 11px;
    color: var(--ink-soft);
    letter-spacing: 0.04em;
    pointer-events: none;
    z-index: 2;
  }
  .hud.tl { top: 16px; left: 16px; }
  .hud.tr { top: 16px; right: 16px; text-align: right; }
  .hud .k {
    color: var(--stone-3);
    font-size: 10px;
    text-transform: uppercase;
    letter-spacing: 0.14em;
  }
  .hud .v {
    color: var(--ink);
    font-size: 15px;
    font-weight: 600;
    font-variant-numeric: tabular-nums;
  }
  .hud .v.accent { color: var(--mark); }
  .source-tag {
    position: absolute;
    left: 50%;
    bottom: 14px;
    transform: translateX(-50%);
    font-family: var(--font-mono);
    font-size: 10px;
    letter-spacing: 0.14em;
    text-transform: uppercase;
    color: var(--stone-3);
    pointer-events: none;
    z-index: 2;
    white-space: nowrap;
  }
</style>
