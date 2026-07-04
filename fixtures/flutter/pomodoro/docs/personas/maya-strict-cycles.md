# Maya — the strict-cycle deep worker

**Profile:** Backend developer; runs textbook pomodoros all morning. Trusts
the timer completely — never pauses, never skips, starts every phase herself
(auto-advance off), and expects the long break exactly when the cycle policy
says so.

**Goals:** uninterrupted focus blocks; an accurate session count at day's end.
**Frustrations:** timers that credit sessions wrongly or surprise her with the
wrong break type.

## Journey (config: `TimerConfig.demo()` — work 3s / short 2s / long 4s / long break every 2)

| # | Step | Expected state (asserted by key) |
|---|------|----------------------------------|
| 1 | Open the app | phase=Focus, display=00:03, sessions=0 |
| 2 | Start; let work phase complete | phase=Short break, sessions=1, stopped |
| 3 | Start the break; let it complete | phase=Focus, display=00:03 |
| 4 | Start; let 2nd work phase complete | phase=Long break (cycle policy), sessions=2 |
| 5 | Start the long break; let it complete | phase=Focus, sessions=2 |

**Invariants:** session count only increments on *completed work phases*;
break type is decided by `completedWork % cyclesPerLongBreak`; timer never
runs without Maya pressing start.
