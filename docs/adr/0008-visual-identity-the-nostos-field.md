# ADR-0008: Visual identity — The Nostos Field

Date: 2026-06-26
Status: Accepted (founder-approved)

## Context

Nostos needs a visual identity for the landing + admin (SvelteKit web app). The
founder rejected a first pass ("Alpine Cadastral") that hadn't been approved and
directed a proper design exploration: three differentiated, themeable concepts
(System + Dark + Light), grounded in [Awwwards 2026 SOTD analysis], then a blend
of the two the founder liked.

[Awwwards 2026 SOTD analysis]: https://svilenkovic.com/3d/awwwards-2026-3d

## Decision

**The Nostos Field** — one primitive (the cairn = stacked slabs) at two scales:

- **Macro (hero):** a top-down Canvas2D field where ripples radiate from the
  Postgres origin and each device-nostos fills slab-by-slab (bottom→top, crown
  last) as its LSN checkpoint arrives. "Run the benchmark" floods the field.
- **Micro (chrome):** the same nostos silhouette, flat — the logo glyph, the
  section divider, the page loader, the admin sync-status pill.

**Unifying idea:** ripples are the force that lights cairns; cairns are the
things ripples light. Remove either and the other stops making sense. This is a
blend of Concept 1 (a slab lighting up) and Concept 2 (a device-node lighting up
as a ripple passes) — both are the *same event* (a checkpoint reaching a device),
expressed at two scales.

### Motion craft (founder-directed refinement)

- **Per-slab staggered illumination:** each device-nostos lights its slabs
  one-by-one (~70ms apart, `easeOutBack`) when a ripple reaches *that* stack —
  independent per-device. The crown slab gets a one-shot shadowBlur flash.
- **Discrete ripples, not ambient:** no auto-emit. Ripples expand at 620px/s,
  are sized to reach the furthest nostos + a margin, then fade over 400ms and
  die. The field rests when idle.
- `prefers-reduced-motion` gates both (instant fill + opacity-only ping).

### Theming

System default (follows `prefers-color-scheme`) + explicit Light/Dark overrides,
persisted in `localStorage`, no-flash bootstrap in `app.html`. The terracotta
`--mark` is the only warm note in either theme. Lexend typeface throughout.

## Consequences

- One concept, committed fully (what SOTD rewards) — the nostos recurs at micro
  and macro scale, so the brand reads whether the canvas is running or not.
- Canvas2D (not Three.js) for the hero — 60fps on mid mobile, no shader
  complexity, theme-flips by re-reading CSS tokens.
- The handoff beat (one enlarged side-elevation nostos) is a dedicated section,
  not an in-hero zoom — cleaner separation, easier to read.
- Tokens live in `web/src/lib/styles/tokens.css` (single source, shared by
  landing + admin). The nostos primitive CSS (`.nostos-rule`, `.nostos-loader`,
  `.sync-pill`) lives there too.
