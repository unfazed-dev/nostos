# Sam — the interrupt-driven starter

**Profile:** Support engineer; gets pulled away constantly. Pauses mid-phase,
resumes, sometimes gives up and resets, and skips breaks he doesn't want.
The app must keep exact state through all of it.

**Goals:** the timer is exactly where he left it after every interruption.
**Frustrations:** pause/resume drift; resets that leave ghost state; skipped
phases that steal or grant session credit.

## Journey (config: `TimerConfig.demo()`)

| # | Step | Expected state |
|---|------|----------------|
| 1 | Start work; pause after ~1s | display frozen, stopped |
| 2 | Wait; confirm display unchanged | display identical (no drift while paused) |
| 3 | Resume; let work complete | phase=Short break, sessions=1 |
| 4 | Skip the break | phase=Focus, sessions=1 (skip grants nothing) |
| 5 | Start work, then reset mid-phase | phase=Focus, display=00:03, sessions=0, stopped |
| 6 | Background the app while running (lifecycle) | auto-paused, display frozen |

**Invariants:** pause is lossless; reset is total (state, count, phase);
skip never credits a session; backgrounding never lets time leak.
