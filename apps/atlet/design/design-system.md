# Atlet · Design System (v2)

> Frozen source of truth. The TweaksPanel mutates runtime values from these tokens;
> this file is the canonical seed. `designer_gate` requires it for iterations.

## Logo
- **Mark** — `AtletMark` (`auth.jsx`): a tilted "A" (left stroke ink, right stroke accent)
  with a tilted crossbar — evokes a sprinter's lean. Geometry (the recognition root,
  preserved unchanged from v1): `M22 62 L40 18`, `M40 18 L58 62`, `M28 48 L54 42`.
- **Wordmark** — `AtletWordmark`: "Atlet" + accent "." dot.
- **Social glyphs** — `GoogleGlyph` + `AppleGlyph` (auth.jsx), brand-faithful.

## Color palette
Warm matte — bone (light) + ink (warm near-black) + one burnt-orange accent.
| Token | Hex | Role |
|---|---|---|
| `--bone` | `#F5F0E8` | app background |
| `--bone-2` | `#EAE3D6` | secondary surface |
| `--paper` | `#FBF8F2` | card/sheet surface |
| `--ink` | `#1A1714` | primary text + dark surface (TimeDetail) |
| `--ink-3` | `#6E6760` | secondary text |
| `--rule` | `#D8CFBE` | hairlines |
| `--accent` | `#D2522B` | burnt orange — the ONE accent |
| `--accent-2` | `#B8431F` | accent pressed |
| `--good` | `#4A7C3A` | success / plant-based |
| `--warn` | `#C68D2E` | ratings |
- **Source** — v1 `styles.css` `:root` (preserved verbatim). Accent is TweaksPanel-mutable.

## Typefaces
| Role | Family |
|---|---|
| Sans (UI) | **Lexend** (300–700) |
| Mono (numerals) | **JetBrains Mono** (400–700) |
HIG scale: largetitle 34 / title2 22 / body 17 / footnote 13.

## Forbidden moves
- **No emoji icons** (stroke `Icon` set; feedback emojis are the one deliberate exception).
- **No rebranding on improve** — mark/wordmark/palette frozen (gate-enforced).
- **No CSS silhouettes** for real products — Shop uses real Unsplash photos.
- **No off-palette hardcoded colors** — every style color routes through a token (gate-enforced).

## Mood
warm · matte · athletic · plant-based · grounded

## Component inventory
- `btn` (filled/tinted/destructive-plain) — shared height 50, radius 14. Commerce uses these.
- `Sheet` (bottom) — grabber/head/body/foot; cart/checkout/PDP reuse it.
- `ListRow`/`ListGroup` — HIG inset lists (account/invoices).
- `Icon` — stroke set, currentColor. Commerce glyphs: cart/truck/pin/card/receipt/leaf.

## Imagery
Shop product photos: real Unsplash URLs (free commercial use, no attribution).
