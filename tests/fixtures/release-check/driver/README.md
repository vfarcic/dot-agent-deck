# PTY-driver contract and stand-in controls

SC-06 extends `inbox/doorbell/060`, whose existing SC-01 controls already own the operation-level oracle and rechunking surface. `tests/e2e_release_check_driver.rs` is lane 1, Linux only: the Rust driver starts the Python Claude and Codex composers on a private PTY and feeds their actual output to SC-01. No deck daemon, agent credential, host configuration or model runs. `/bin/true` occupies the unused hook-command argument; these scenarios never submit a prompt. This slice tests capture, release binding, raw/no-echo, ready/typed/deleted screens, resize evidence and the oracle, not the routed-turn mock, the complete release-record trial counts, parser budgets, containment or publication authority. Those remain later integration obligations.

`../driver-frames.json` freezes the default stand-ins' ready, typed, deleted and resized ANSI, initially at 50 × 200, then 40 × 140. The header is deliberately synthetic `0.1.2`; the typed line is `(abcdef) synthetic`. Before freezing these, both modified default stand-ins were run against the parent commit's stand-ins on the same scripted start/type/delete/resize path, with byte-identical results. Templates are fixed test data and never learned from the candidate under test. They assert the synthetic screens only, not that either stand-in is a faithful real release or an SC-15 reference.

macOS reads the same committed ANSI through `doorbell_060_driver_frozen_frames_read_without_a_pty` in `tests/release_check_evaluate.rs`. That offline test selects each frozen screen independently (wrapping Claude's bytes in synthetic synchronized-update markers solely for this fixed-grid check); actual Claude framing remains quiet-gap in the driver tests. The new PTY tests compile to an empty crate on macOS; they provide no evidence for its PTY modes, driver launch, cdhash, Keychain or containment. Nothing in this slice calls a real agent.

## Missing Rust interface

The tests deliberately import the absent `dot_agent_deck::release_check::driver`; no production stub or substitute collector is supplied. They propose this interface, in addition to the existing SC-01 contract in the parent README:

```rust
pub struct DriverPlan { /* serde::Deserialize, schema below */ }
pub struct DriverReference { /* serde::Deserialize, schema below */ }
pub fn collect(plan: &DriverPlan) -> std::io::Result<serde_json::Value>;
pub fn evaluate(reference: &DriverReference, capture: &serde_json::Value)
    -> crate::release_check::evaluate::Evaluation;
```

Names and representations can change in agreement with the tester; the observable assertions must survive. `evaluate` is the driver-to-SC-01 measurement bridge, not a second terminal oracle: it feeds every captured operation to SC-01, measures the header with the production release recogniser, checks the driver's actual termios samples from readiness onward, and checks each settled checkpoint against its required reviewed template. A `Passed` here covers only these measurement facts and grants no authority.

`DriverPlan` contains `program`, `args`, an explicit `env` map, `cwd`, initial `rows`/`cols`, `quiet_ns`, `checkpoint_quiet_ns`, `deadline_ms`, `max_bytes`, `read_size`, `framing` and `steps`. The child receives `env_clear` plus exactly that map, starts in `cwd`, and uses its own PTY. A checkpoint waits for changed output when the preceding step sent input or resized, then for `checkpoint_quiet_ns` of measured silence; mere passage of a deadline cannot satisfy a checkpoint. Steps are `{op: "send", bytes: [u8]}`, `{op: "resize", rows, cols}` and `{op: "checkpoint", name}`. Resize performs the PTY ioctl and records its byte offset before consuming the redraw. The collector has a bounded wait and byte count and always reaps its own child, including on an error. It never connects to the production daemon or invokes an agent. SIGTERM/kill addresses only the child it captured, never a process-name search.

`DriverReference` contains `agent` (`claude` or `codex`), the expected `release`, and ordered `checkpoints`. Each checkpoint has `name`, `expected_template`, `fact` and `oracle` (the SC-01 `OracleReference` schema). A checkpoint oracle includes the previously allowed frames within its geometry segment, so it can classify the whole transition from the segment's blank initial terminal; `expected_template` separately requires the final settled frame. Geometry starts a new oracle segment on the resize redraw, parsed from blank at its new dimensions. In particular, echoing nothing still draws a valid ready frame, but fails the typed checkpoint as `echo`. A wrong release names `release_recogniser`, and echo-enabled termios names `input_mode`, before evaluating template fields that necessarily differ too. Per-operation and unknown-complete-frame failures retain SC-01's fact and nonempty detail.

The returned capture is JSON, to keep the lane-2 recorder's existing evidence usable:

| Field | Meaning |
| --- | --- |
| `bytes` | Every PTY output byte, in order, as `[u8]`. |
| `index` | Lane-2 index: initial rows/cols, `read_lens` of nonempty reads, named byte-offset `points`, ordered resize `edges` (`off`, rows, cols), and `lines`. No timestamp is substituted for an offset. |
| `reads` | Actual reads with monotonic nanoseconds `t`, cumulative byte offset `off`, and `len`. No merging or synthetic timestamps in the saved evidence. |
| `modes` | Samples as the lane-2 recorder uses: `t`, `off`, `bits: [icanon, echo]`; sampled on the driver's own PTY throughout the run, including after readiness. |
| `observations` | Ordered `{name, oracle}` at the checkpoints; `oracle` is an SC-01 `OracleCapture` with read timestamps as `at_ns`, raw bytes, operation-indexed `gap_events`, geometry, phase `idle`, framing, and `phase_ended`. Within one geometry each observation retains the segment prefix, not only the latest redraw. |
| `quiet_observations` | Capture-time terminal silence observations (`off`, `elapsed_ns`), distinguishing measured quiet from an artificial frame at phase end. See the final-gap convention below. |

The parser state survives all actual read boundaries, including boundaries inside CSI and UTF-8. Operation positions count completed operations, not bytes. For each arrival gap at least Q, derive the event once at the operation count immediately before the next read, with its actual elapsed nanoseconds; replay uses that event rather than deriving timing again. The explicit terminal quiet observation supplies the final measured silence boundary for the finite checkpoint capture; a phase end alone must not supply one. **Final-gap convention for coder/design review:** §2.4 defines arrival gaps in terms of a next read, whereas these finite synthetic checkpoints also need a terminal quiet measurement to classify their last frame. If the implementation uses a later actual read to close the checkpoint instead, retain that evidence and adapt this representation by agreement, without pretending a phase end is a measured gap. This is a proposed interface convention, not evidence that the design already settled it.

Rechunking in the tests changes only SC-01's replay reads. It cuts at each saved operation-indexed gap, then emits one-byte, seven-byte or whole-frame chunks within each interval, including escape/UTF-8 splits. It carries the original gap events verbatim, never merges over a gap, and does not recompute gaps from the replay chunks' placeholder timestamps. Raw saved reads/timestamps stay intact. The tests compare outcome, complete-frame count, stalls and accepted T-1 stalls. An independent test-side `vte` counter positions cuts and audits gap positions; it implements no grid or verdict.

## Knobs and expected outcomes

All new knobs default off. The helper `tests/support/composer_driver_knobs.py` loads only when a new knob is enabled, so existing tests that copy a composer script alone remain dependency-free. `DOT_AGENT_DECK_TEST_COMPOSER_RELEASE` already exists and remains the release knob. The shared `DRIVER_FAULT` values below are mutually exclusive and modify real emitted bytes or termios, never a recorded verdict.

| Knob/value | Observable change | Required fact |
| --- | --- | --- |
| `RELEASE=9.8.7` | Header contradicts the triggered synthetic release. | `release_recogniser` |
| `DRIVER_FAULT=echo_mode` | Slave remains noncanonical but ECHO is enabled. | `input_mode` |
| `DRIVER_FAULT=missing_echo` | Typed input is received but the ready placeholder is drawn instead. | `echo` |
| `DRIVER_FAULT=overlay` | `!` is drawn at row 35, column 5, erased and the cursor restored within one `os.write`. That cell is blank in both reviewed frames. | `transient_content` |
| `DRIVER_FAULT=moved_glyph` | The header alphabet's `C` is drawn at that unrelated blank cell and erased within one write. | `transient_content` |
| `DRIVER_FAULT=mode` | Mouse mode 1000 is enabled and restored within one redraw. | `mode_transient` |
| `DRIVER_FAULT=control` | An undeclared OSC 52 is emitted inside the redraw. | `unrecognised_control` |
| `DRIVER_FAULT=modal` | A completed frame retains `Confirm?` outside the composer. | `unrecognised_frame` |
| `TIMING_RESTORE=1` | Claude erases the old placeholder's first two cells, restores `Tr`, then draws the typed composer. | Pass under the captured timing; `transient_content` after adding the new erased-frame gap. |

The timing control reviews a third, erased-placeholder template. Without a gap at that erased state, restoring `Tr` is allowed because those glyphs were at those exact cells in the previous complete frame. Adding the gap makes the erased template the previous complete frame, and the next typed frame has neither glyph at those cells. The restore now fails. Both the faithful control and the added-gap failure are replayed at all three read sizes.

## Executable evidence map

| Test in `tests/e2e_release_check_driver.rs` | Evidence |
| --- | --- |
| `doorbell_060_driver_collects_standin_bytes_modes_and_layout` | Faithful driver/evaluator; all bytes and lane-2 indexes, monotonic read times, raw/no-echo, delete and resize redraw. |
| `doorbell_060_driver_fault_knobs_name_the_failed_fact` | Faithful pass on each agent, then all eight faults with named outcomes. |
| `doorbell_060_driver_overlay_inside_one_replay_read` | Final grid identical to golden; one-read replay still detects the operation-level overlay. |
| `doorbell_060_driver_derives_gaps_once_at_capture` | An actual idle-to-type arrival gap is at the correct operation count with its real elapsed time; every other gap needs terminal quiet evidence. |
| `doorbell_060_driver_timing_preserving_rechunking` | Faithful and overlay/mode/control failures invariant under 1-byte, 7-byte and whole-frame replay. |
| `doorbell_060_driver_added_gap_can_change_the_verdict` | Real stand-in erase/restore bytes pass original timing and fail the added gap at every read size. |
| `doorbell_060_driver_standin_knob_self_checks` | The four new standalone Python controls, callable without compiling the missing driver. |

Standalone command: `python3 tests/support/composer_knob_checks.py`. Its seven retained controls cover all three composers' original knobs. Four SC-06 controls compare frozen default bytes across start/type/delete/resize, verify each output fault is in the same single write (before Codex sync-off), sample the termios and missing-echo faults, and pin the exact erase/restore stream. The default controls also run with the new knobs explicitly disabled.

All Rust tests are interface-RED until SC-01 and SC-06 exist. Their assertions are an implementation contract, not observed behavioral failures or a full self-check pass.
