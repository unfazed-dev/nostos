<script lang="ts">
  import NostosField from '$lib/components/NostosField.svelte';
  import { Nostos, Button } from '$lib/components/ui';

  // reactive HUD values piped up from the field
  let ops = $state(0);
  let synced = $state(0);
  let total = $state(0);

  let field: NostosField;
  const fmt = (n: number) => n.toLocaleString();
  const hex = (n: number) => Math.round(n % 256).toString(16).padStart(2, '0').toUpperCase();

  // handoff nostos fill tracks the field's completion ratio
  let bigLit = $derived(total > 0 ? Math.floor((synced / total) * 8.05) : 0);

  // pricing tiers (mirrors nostos-cloud Tier enum — single source lives in license.rs)
  const tiers = [
    {
      name: 'Hobby',
      price: '$0',
      cadence: 'forever',
      blurb: 'Self-host the engine. For makers and small projects.',
      features: ['Apache-2.0, unrestricted', '100 concurrent devices', 'Community support'],
      cta: 'Self-host',
      href: '/docs',
      featured: false
    },
    {
      name: 'Pro',
      price: '$49',
      cadence: '/ mo',
      blurb: 'Managed cloud sync for production apps.',
      features: ['1,000 concurrent devices', 'Reactive-when-connected push', 'Email support'],
      cta: 'Start Pro',
      href: '/#waitlist',
      featured: true
    },
    {
      name: 'Scale',
      price: 'Custom',
      cadence: '',
      blurb: 'For fleets and regulated workloads.',
      features: ['Unlimited devices', 'Dedicated infra + SLA', 'Priority engineering'],
      cta: 'Talk to us',
      href: '/#waitlist',
      featured: false
    }
  ];

  let email = $state('');
  let submitted = $state(false);
  function joinWaitlist(e: Event) {
    e.preventDefault();
    if (!email.includes('@')) return;
    // real impl POSTs /v1/waitlist; the static preview just acknowledges.
    submitted = true;
  }
</script>

<svelte:head>
  <title>Nostos — local-first sync, lit by the same ripple</title>
  <meta
    name="description"
    content="Nostos streams Postgres logical replication to every device — offline-capable, conflict-safe, and fast enough to feel local. Apache-2.0."
  />
</svelte:head>

<!-- ============================ HERO ============================ -->
<section class="hero">
  <div class="hero-copy">
    <span class="eyebrow">
      <Nostos size={18} />
      Local-first sync · Apache-2.0
    </span>
    <h1>Every device, <em>lit by the same ripple.</em></h1>
    <p class="lede">
      Nostos streams Postgres logical replication to every device. A commit lands,
      ripples outward, and each device-nostos fills slab-by-slab as its LSN checkpoint
      arrives. One engine. One field. Every device home.
    </p>
    <div class="cta-row">
      <Button variant="mark" onclick={() => field.flood()}>Run the benchmark ▸</Button>
      <Button href="/docs" variant="ghost">Read the docs</Button>
    </div>
    <p class="sub-cue">click the field to send a single commit</p>
  </div>

  <NostosField
    bind:this={field}
    onops={(v: number): void => { ops = v; }}
    onsynced={(n: number, t: number): void => {
      synced = n;
      total = t;
    }}
  >
    {#snippet opsSlot()}{fmt(ops)}{/snippet}
    {#snippet syncedSlot()}{synced} / {total}{/snippet}
  </NostosField>
</section>

<!-- ============================ HANDOFF BEAT ============================ -->
<section class="handoff" id="how">
  <div class="handoff-copy">
    <div class="label">The handoff · one device, side elevation</div>
    <h2>This is the same nostos you just saw lit in the field.</h2>
    <p>
      Zoom into any device and the constellation resolves to a single stack — slabs
      filling as its checkpoint arrives, crown glowing terracotta at peak. This profile
      is the mark you'll meet again, everywhere below.
    </p>
  </div>
  <div class="handoff-nostos-wrap">
    <div class="nostos-big" role="img" aria-label="Single device nostos in side elevation, slabs filling bottom-to-top, crown glowing">
      <div class="slab" class:on={bigLit >= 1}></div>
      <div class="slab" class:on={bigLit >= 2}></div>
      <div class="slab" class:on={bigLit >= 3}></div>
      <div class="slab" class:on={bigLit >= 4}></div>
      <div class="slab" class:on={bigLit >= 5}></div>
      <div class="slab" class:on={bigLit >= 6}></div>
      <div class="slab" class:on={bigLit >= 7}></div>
      <div class="slab crown" class:on={bigLit >= 8}></div>
    </div>
    <div class="handoff-readout mono tnum">
      LSN&nbsp;&nbsp;0/{hex((synced / Math.max(1, total)) * 240)}3F{hex((synced / Math.max(1, total)) * 199 + 40)}
    </div>
  </div>
</section>

<!-- ============================ THE BENCHMARK ============================ -->
<section class="benchmark" id="benchmark">
  <div class="nostos-rule"><i></i><i></i><i></i></div>
  <div class="benchmark-grid">
    <div>
      <div class="label">Proven on real Postgres</div>
      <h2>Throughput you can feel.</h2>
      <p class="lede">
        Our reference PgReplicator streams the WAL over logical replication, fans out
        to every connected device over WebSocket, and resumes exactly-once after any
        outage — checkpointed by LSN. The field above is running that loop live, in your
        browser, against a deterministic workload.
      </p>
      <ul class="proof">
        <li><strong class="tnum">142k+</strong> ops/sec sustained</li>
        <li><strong class="tnum">35×</strong> the throughput of bucket-based sync</li>
        <li><strong>Exactly-once</strong> resume across disconnects</li>
      </ul>
    </div>
    <div class="quote">
      <p>"A cairn is a stack of stones marking a trail. Our LSN checkpoints <em>are</em> the cairns — durable markers that get your data home across outages."</p>
      <div class="attrib mono">— the Nostos design memo</div>
    </div>
  </div>
</section>

<!-- ============================ PRICING ============================ -->
<section class="pricing" id="pricing">
  <div class="nostos-rule"><i></i><i></i><i></i></div>
  <h2>Pricing</h2>
  <p class="lede">Self-host free forever. Pay only when you want it managed.</p>
  <div class="tiers">
    {#each tiers as t (t.name)}
      <article class="tier" class:featured={t.featured}>
        {#if t.featured}<span class="tier-mark">Most popular</span>{/if}
        <h3>{t.name}</h3>
        <div class="price"><span class="amount">{t.price}</span><span class="cadence">{t.cadence}</span></div>
        <p class="blurb">{t.blurb}</p>
        <ul>
          {#each t.features as f (f)}
            <li>{f}</li>
          {/each}
        </ul>
        <Button href={t.href} variant={t.featured ? 'mark' : 'ghost'}>{t.cta}</Button>
      </article>
    {/each}
  </div>
</section>

<!-- ============================ WAITLIST ============================ -->
<section class="waitlist" id="waitlist">
  <div class="nostos-rule"><i></i><i></i><i></i></div>
  <h2>Light your field.</h2>
  {#if submitted}
    <p class="ack">
      <Nostos size={16} /> You're on the list. We'll ripple when Cloud sync opens.
    </p>
  {:else}
    <form onsubmit={joinWaitlist}>
      <input
        type="email"
        bind:value={email}
        placeholder="you@yourproject.dev"
        aria-label="Email"
        required
      />
      <Button type="submit" variant="mark">Join the waitlist</Button>
    </form>
    <p class="fine">No spam. One email when managed Cloud sync is ready.</p>
  {/if}
</section>

<!-- ============================ THE RECURRING CHROME ============================ -->
<section class="chrome">
  <div class="label">The same primitive · flat</div>
  <p>
    The cairn is Nostos. One silhouette recurs across the whole product — the field is
    just where you see it move. Every surface inherits the stack, so the brand reads
    whether the canvas is running or not.
  </p>
  <div class="chrome-grid">
    <div class="chrome-card">
      <div class="demo"><Nostos size={30} /></div>
      <h4>The logo glyph</h4>
      <p>Offset slabs, crown lit. The brand in 30px.</p>
    </div>
    <div class="chrome-card">
      <div class="demo"><div class="nostos-rule"><i></i><i></i><i></i></div></div>
      <h4>The section divider</h4>
      <p>The same offset silhouette as a 2px hairline trio.</p>
    </div>
    <div class="chrome-card">
      <div class="demo">
        <span class="nostos-loader">
          <i class="on"></i><i class="on"></i><i class="on"></i><i class="on"></i>
          <i class="on"></i><i></i><i></i><i></i>
        </span>
      </div>
      <h4>The page loader</h4>
      <p>Stack fills top→bottom; crown lights last — completion.</p>
    </div>
    <div class="chrome-card">
      <div class="demo">
        <span class="sync-pill">
          <span class="glyph"><i></i><i></i><i></i></span>
          syncing · {fmt(Math.max(synced * 524, 8412))} devices
        </span>
      </div>
      <h4>The admin sync pill</h4>
      <p>Live state, in the founder console. Crown blinks on checkpoint.</p>
    </div>
  </div>
</section>

<style>
  /* ---- shared section rhythm ---- */
  section {
    max-width: var(--wrap);
    margin: 0 auto;
    padding: 0 var(--gutter);
  }
  .nostos-rule { margin-bottom: 28px; }
  h1, h2 {
    font-weight: 640;
    letter-spacing: -0.022em;
    line-height: 1.05;
    text-wrap: pretty;
  }
  h1 em { font-style: normal; color: var(--mark); }

  /* ---- hero ---- */
  .hero {
    padding-top: 40px;
    padding-bottom: 56px;
    display: grid;
    grid-template-columns: 0.92fr 1.18fr;
    align-items: center;
    gap: 44px;
    min-height: calc(100vh - 72px);
  }
  .hero-copy { max-width: 500px; }
  .eyebrow {
    display: inline-flex;
    align-items: center;
    gap: 10px;
    font-family: var(--font-mono);
    font-size: 12px;
    font-weight: 600;
    letter-spacing: 0.12em;
    text-transform: uppercase;
    color: var(--mark-ink);
    margin-bottom: 26px;
  }
  h1 { font-size: clamp(2.4rem, 5.4vw, 3.7rem); margin-bottom: 22px; }
  .lede { font-size: 1.16rem; color: var(--ink-soft); line-height: 1.5; max-width: 44ch; margin-bottom: 32px; }
  .cta-row { display: flex; gap: 12px; flex-wrap: wrap; align-items: center; }
  .sub-cue { font-family: var(--font-mono); font-size: 11px; color: var(--stone-3); margin-top: 18px; letter-spacing: 0.04em; }

  /* ---- handoff beat ---- */
  .handoff {
    padding-top: 8px;
    padding-bottom: 8px;
    display: grid;
    grid-template-columns: 0.92fr 1.18fr;
    gap: 44px;
    align-items: end;
  }
  .handoff-copy { max-width: 500px; }
  .handoff-copy .label {
    font-family: var(--font-mono);
    font-size: 11px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.14em;
    color: var(--mark-ink);
    margin-bottom: 10px;
  }
  .handoff-copy h2 { font-size: 1.5rem; margin-bottom: 8px; }
  .handoff-copy p { color: var(--ink-soft); font-size: 0.98rem; line-height: 1.55; }
  .handoff-nostos-wrap { display: grid; place-items: center; height: 220px; gap: 16px; }
  .nostos-big {
    --w: 150px;
    position: relative;
    width: var(--w);
    height: calc(8 * 22px + 7 * 4px);
    display: flex;
    flex-direction: column-reverse;
    gap: 4px;
  }
  .nostos-big .slab {
    height: 22px;
    border-radius: 2px;
    background: var(--stone-1);
    transition: background 360ms var(--ease), box-shadow 360ms var(--ease);
  }
  .nostos-big .slab:nth-child(1) { width: 100%; }
  .nostos-big .slab:nth-child(2) { width: 96%; align-self: center; }
  .nostos-big .slab:nth-child(3) { width: 90%; align-self: center; }
  .nostos-big .slab:nth-child(4) { width: 84%; align-self: center; }
  .nostos-big .slab:nth-child(5) { width: 74%; align-self: center; }
  .nostos-big .slab:nth-child(6) { width: 62%; align-self: center; }
  .nostos-big .slab:nth-child(7) { width: 48%; align-self: center; }
  .nostos-big .slab:nth-child(8) { width: 32%; align-self: center; }
  .nostos-big .slab.on { background: color-mix(in srgb, var(--ink) 80%, var(--stone-2)); }
  .nostos-big .slab.crown.on {
    background: var(--mark);
    box-shadow: 0 0 26px color-mix(in srgb, var(--mark-glow) 60%, transparent);
  }
  .handoff-readout { font-size: 12px; color: var(--stone-3); letter-spacing: 0.06em; }

  /* ---- benchmark ---- */
  .benchmark { padding-top: 64px; padding-bottom: 64px; border-top: 1px solid var(--rule); margin-top: 56px; }
  .benchmark-grid { display: grid; grid-template-columns: 1.2fr 1fr; gap: 56px; align-items: start; }
  .benchmark .label { margin-bottom: 14px; }
  .benchmark h2 { font-size: clamp(1.8rem, 3.6vw, 2.4rem); margin-bottom: 16px; }
  .proof { list-style: none; margin-top: 28px; display: grid; gap: 12px; }
  .proof li { font-size: 1rem; color: var(--ink-soft); }
  .proof strong { color: var(--ink); font-size: 1.3rem; margin-right: 8px; }
  .quote {
    padding: 28px;
    border: 1px solid var(--rule);
    border-left: 2px solid var(--mark);
    border-radius: var(--radius-lg);
    background: var(--paper-2);
  }
  .quote p { font-size: 1.1rem; line-height: 1.5; }
  .quote em { font-style: normal; color: var(--mark); }
  .attrib { margin-top: 14px; font-size: 11px; color: var(--stone-3); letter-spacing: 0.06em; }

  /* ---- pricing ---- */
  .pricing { padding-top: 64px; padding-bottom: 64px; border-top: 1px solid var(--rule); }
  .pricing h2 { font-size: clamp(1.8rem, 3.6vw, 2.4rem); margin-bottom: 10px; }
  .tiers { display: grid; grid-template-columns: repeat(3, 1fr); gap: 18px; margin-top: 36px; }
  .tier {
    position: relative;
    padding: 28px 24px;
    border: 1px solid var(--rule);
    border-radius: var(--radius-lg);
    background: var(--paper-2);
    display: flex;
    flex-direction: column;
    gap: 14px;
  }
  .tier.featured { border-color: color-mix(in srgb, var(--mark) 50%, transparent); }
  .tier-mark {
    position: absolute; top: -11px; left: 24px;
    font-family: var(--font-mono); font-size: 10px; font-weight: 600;
    text-transform: uppercase; letter-spacing: 0.14em;
    color: var(--paper); background: var(--mark);
    padding: 4px 10px; border-radius: 20px;
  }
  .tier h3 { font-size: 1.2rem; font-weight: 600; }
  .price { display: flex; align-items: baseline; gap: 6px; }
  .price .amount { font-size: 2rem; font-weight: 700; letter-spacing: -0.02em; }
  .price .cadence { color: var(--stone-3); font-size: 0.9rem; }
  .blurb { color: var(--ink-soft); font-size: 0.95rem; min-height: 2.6em; }
  .tier ul { list-style: none; display: grid; gap: 8px; margin: 4px 0 12px; }
  .tier li { font-size: 0.9rem; color: var(--ink-soft); padding-left: 18px; position: relative; }
  .tier li::before { content: ''; position: absolute; left: 0; top: 0.55em; width: 8px; height: 2px; background: var(--mark); }

  /* ---- waitlist ---- */
  .waitlist { padding-top: 64px; padding-bottom: 80px; border-top: 1px solid var(--rule); }
  .waitlist h2 { font-size: clamp(1.8rem, 3.6vw, 2.4rem); margin-bottom: 24px; }
  .waitlist form { display: flex; gap: 10px; max-width: 460px; }
  .waitlist input {
    flex: 1;
    padding: 12px 16px;
    border: 1px solid var(--stone-2);
    border-radius: var(--radius);
    background: var(--paper);
    color: var(--ink);
    font-size: var(--t-14);
  }
  .waitlist input:focus { outline: 2px solid var(--mark); outline-offset: 1px; }
  .ack {
    display: inline-flex; align-items: center; gap: 12px;
    color: var(--mark-ink); font-weight: 500;
    padding: 14px 20px;
    border: 1px solid color-mix(in srgb, var(--mark) 40%, transparent);
    border-radius: var(--radius-lg);
    background: color-mix(in srgb, var(--mark) 7%, transparent);
  }
  .fine { margin-top: 14px; font-size: 12px; color: var(--stone-3); }

  /* ---- recurring chrome ---- */
  .chrome { padding-top: 56px; padding-bottom: 96px; border-top: 1px solid var(--rule); }
  .chrome .label { margin-bottom: 8px; }
  .chrome > p { color: var(--ink-soft); max-width: 60ch; margin-bottom: 32px; font-size: 0.98rem; }
  .chrome-grid { display: grid; grid-template-columns: repeat(4, 1fr); gap: 16px; }
  .chrome-card { padding: 22px 20px; border: 1px solid var(--rule); border-radius: var(--radius-lg); background: var(--paper-2); }
  .chrome-card .demo { height: 72px; display: grid; place-items: center; margin-bottom: 14px; }
  .chrome-card h4 { font-size: 13.5px; font-weight: 600; margin-bottom: 5px; }
  .chrome-card p { font-size: 12.5px; color: var(--ink-soft); line-height: 1.5; }

  /* ---- responsive ---- */
  @media (max-width: 920px) {
    .hero, .handoff { grid-template-columns: 1fr; gap: 28px; }
    .benchmark-grid { grid-template-columns: 1fr; gap: 32px; }
    .tiers { grid-template-columns: 1fr; }
  }
  @media (max-width: 720px) {
    .chrome-grid { grid-template-columns: repeat(2, 1fr); }
  }
  @media (max-width: 520px) {
    .chrome-grid { grid-template-columns: 1fr; }
    .waitlist form { flex-direction: column; }
  }
</style>
