# Doorbell admission

Maintainer reference for how the deck decides whether it may type the doorbell line into an agent's pane, and what it reads to decide. User behaviour belongs in [Orchestration](../orchestration.md#delivery-in-the-tui-and-desktop-app) and [Troubleshooting](../troubleshooting.md); the messaging model around it is in [Agent messaging](agent-messaging.md).

## The parsers' work budgets

Two `vt100` parsers run inside the PTY reader's push, under the pane's output lock (`AgentBus::push` in `src/agent_pty.rs`): the release scanner (`src/release_scanner.rs`), which reads a generation's release off its start-up header, and the evidence parser (`src/evidence_state.rs`), which reads the agent's composer. Whatever they cost is time every subscriber of that pane, its resizes and its evidence reads wait for. Most output costs them a few dozen nanoseconds a byte. Some `vt100` operations work over the whole grid, so a pane at 512 × 512 that prints erase-display without pause costs each erase 262 144 cell clears: measured before the budgets, 600 ms of scanner CPU over a 256 KiB window and 19.9 ms of added lock hold per push for the scanner, 18.3 ms for the evidence parser.

### What is counted

Both parsers are fed through `src/vt_work.rs`'s `MeteredParser`, one byte at a time. Before a byte reaches `vt100`, a shadow `vte` tokenizer (the one `vt100` itself runs on, checked by `the_meter_and_vt100_share_one_vte` to be a single version in `Cargo.lock`) prices it in **work units**: 64 for every byte's own parse and check, plus one unit for each cell the operation it completes can write or move. The prices are upper bounds read from `vt100` 0.18.1's `screen.rs`, `grid.rs` and `row.rs`; the module documentation lists them operation by operation. A byte whose price would take the caller past its limit is **not parsed**, and nothing after it is.

The count is of work, not of time, so a loaded machine cannot make a normal launch look expensive, and the bound holds by construction: no push gives a parser more than its limit's worth of units, however the bytes were split, because the check runs before each byte and not after the push.

What a unit costs in time is measured, not derived. `scanner_budget_calibration` in `src/agent_pty_scanner_budget.rs` prices every recorded launch and fifteen pathological traces with no limit and reports their CPU time per unit; `scanner_budget_measurement` replays them through the real push path. On the measuring machine (16 cores, release profile, 2026-10-25) the dearest figures were 0.44 to 0.57 ns per unit for a short recorded launch, varying run to run, 0.43 for the dearest pathological trace priced to its end, and 0.57 for a push that trips a limit through the push path, where a fresh grid's page faults and the push's own work fall inside the same 5 million units. The first pricing runs found three operations priced low, and each was repriced before the limits were set: a full reset (`ESC c`, 2.9 ns per unit, from reallocating every row) and the first switch to the alternate screen (which allocates its grid) now cost `16 × R × C`, and erase-in-line (1.2 ns, erased cell by cell) `4 × C`.

### The limits

| parser | limit | units | at 0.57 ns per unit | the most a recorded launch uses |
| --- | --- | --- | --- | --- |
| release scanner | per push (`SCANNER_WORK_PER_PUSH`) | 5 000 000 | 2.9 ms | 559 680 |
| release scanner | per generation (`SCANNER_WORK_TOTAL`) | 85 000 000 | 48 ms | 12 643 784; a whole 256 KiB window of launches, 19 963 226 (7.8 ms of CPU) |
| evidence parser | per push (`EVIDENCE_WORK_PER_PUSH`) | 5 000 000 | 2.9 ms | none reaches it (`doorbell_044_recorded_launches_stay_within_the_evidence_bound`) |

The two per-push limits together keep a push's added lock hold under the 10 ms maximum plan §4.6 sets, with room for a machine slower than the measuring one. The evidence parser has no per-generation limit: it has no byte window and runs for the generation's life, so only its per-push cost is bounded.

A normal launch reaches the per-push limit only in a pane near the cell budget: a first switch to the alternate screen at 512 × 512 costs 4.5 million units on its own, so the read that carries it and a redraw can pass 5 million. That is the case the `scanner_budget` clause tells a user about (make the window smaller and start the agent afresh).

### What a trip does

- **Release scanner.** The scanner is dropped and its window ends (`pane release` says `scanner dropped: work over budget`, and the `release scanner timing` line names the limit, `push` or `total`, beside `work` and `push_work_max`). A generation with no release latched yet reads as `scanner_budget`, which the activation table holds with that clause; a release already latched stays latched.
- **Evidence parser.** The parser is dropped and the state is `WorkOverBudget` (`work_over_budget` in the `pane layout` line), which recognises nothing, so nothing admitted on a Clean frame can be. The next layout edge, a resize or a repaint nudge, starts the evidence over, exactly as after any edge.

Resizes are not charged to either budget. A resize is the deck's own ioctl, not the agent's output, and its cost is recorded separately (`resize_us_max`; up to 6.4 ms measured, to 512 × 512 among other sizes).

### Measured with the budgets in

Plan §9.1's harness, `scanner_budget_measurement`, replayed every recorded launch through the real push path at its recorded read sizes, with and without each parser, on the measuring machine (15 runs; p99 is the nearest rank over every push of every run):

| trace | added lock hold per push | scanner CPU per generation | outcome |
| --- | --- | --- | --- |
| 16 recorded launches at 50 × 200 and 80 × 300 (129 900 pushes) | p99 69 µs, max 501 µs | at most 5.0 ms | no limit reached (the fast tests below) |
| a whole 256 KiB window of recorded launches, 50 × 200 and 80 × 300 | p99 43 µs, max 447 µs | at most 7.8 ms | no limit reached |
| eight panes at once, 50 × 200 (31 116 pushes) | p99 88 µs, max 963 µs | — | no limit reached |
| evidence parser, every recorded launch | p99 349 µs, max 484 µs | — | no limit reached |
| the fifteen pathological traces, scanner | at most 2.9 ms | at most 33.9 ms | dropped at a limit, or, for delete and insert characters, inside both to the byte limit (10.7 ms) |
| the fifteen pathological traces, evidence parser | at most 2.7 ms | — | dropped at the per-push limit where a read reaches it |

The pathological traces are erase-display with header-like rows at 512 × 512, line feeds at the bottom and reverse index at the top, scroll-up and insert-line by 4096, a 65 535-character repeat, eight alternate-screen switches in one sequence, full reset, erase-in-line twice (cursor anywhere and at the row's start), erase, delete and insert characters, erase below the cursor, and prints at the row end, each at the geometry within the cell budget that makes it dearest. Before the budgets, the first of them cost the scanner 600 ms of CPU and 19.9 ms per push. `doorbell_055_recorded_launches_stay_within_the_scanner_budget` and `doorbell_044_recorded_launches_stay_within_the_evidence_bound` keep the recorded launches inside the limits in `cargo test-fast`, and `doorbell_055_the_scanner_works_to_a_budget` and `doorbell_044_the_evidence_parser_works_to_a_per_push_bound` pin what a trip does.

### What the budgets do not claim

- The time figures are units times the cost per unit measured on one machine. A machine half as fast spends twice the time on the same units; the work bound is the same everywhere.
- A pane whose every push stays under the evidence parser's per-push limit costs that much on every push for as long as it prints. The bound is per push, as decided, not a rate limit.
- The prices are upper bounds read from `vt100` 0.18.1. A `vt100` update that adds an operation over many cells needs a price for it, and the calibration rerun.
