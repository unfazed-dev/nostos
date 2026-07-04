# Rio — the custom-duration long-hauler

**Profile:** Writer; ignores the 25/5 default and configures long 50/10
blocks with a long break every 3. Runs many consecutive cycles in one
sitting. (Journey uses a compressed custom config with the same *shape*:
work 4s / short 2s / long 5s / every 3 — same policy, demo scale.)

**Goals:** the machine respects custom durations and the custom cycle policy
across a long run.
**Frustrations:** apps that hard-code 25/5 or mis-place the long break under
non-default policies.

## Journey (config: work 4s, short 2s, long 5s, cyclesPerLongBreak 3)

| # | Step | Expected state |
|---|------|----------------|
| 1 | Open with custom config | display=00:04 (custom work length respected) |
| 2 | Complete work #1 and its short break | sessions=1, back to Focus |
| 3 | Complete work #2 and its short break | sessions=2, back to Focus |
| 4 | Complete work #3 | phase=Long break (policy: every 3), sessions=3 |
| 5 | Complete the long break | phase=Focus, sessions=3 |

**Invariants:** durations come from config everywhere (display proves it);
long-break placement follows `cyclesPerLongBreak`, not a hard-coded 4.
