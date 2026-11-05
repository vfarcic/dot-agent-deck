# Doorbell gate removal map (PRD #1666 part 3, `gaterm`)

The maintainer's decision: the doorbell (the typed line, and the input-box check with its probes, helper and fence) works from **one set of facts per agent** (Claude Code, Codex), not from a hand-measured record per release. A release nobody has measured is assumed to behave like its agent's facts. The per-release measurements stay as **conformance tests**: a recorded release whose measurements contradict its agent's facts fails a test, and the facts or the code are fixed then, without users being blocked meanwhile. There is no "unmeasured release" note anywhere. A per-agent config switch turns the doorbell off for a user who hits a break first. The per-session checks (the start-up header latch, the screen profile, the input-mode evidence, the settle window, the probes, the fence, `cannot_check`, fail-closed on a refusal) are unchanged. No per-release timing record is added.

This page is the map written before the change: what exists, who reads it, and what happens to each piece.

## `ReleaseRecord`'s fields

| Field | Read by (production) | Disposition |
| --- | --- | --- |
| `agent` | `activation` (lookup), `generation_activation` (any record for the agent), `composer_probe::permission`, observer's keymap evidence, `doorbell off` log's measured list | Agent-level: `AgentFacts.agent`. Also kept on a recorded release (conformance input). |
| `release` | `activation` (exact lookup), `typing_for` (record lookup for caps), `permission` (exact lookup), observer (editor probe lookup), `measured_releases` and the `doorbell off` log | **Deleted as a key.** Kept only on the conformance data (`RecordedRelease.release`). The latched release is still logged on `doorbell attempt` (`release=`), which reads the latch, not a record. |
| `profile` | `typing_for` (screen profile), `Activation::Go` | Agent fact. Conformance: every recorded release names its agent's profile. |
| `ready_composer`, `echo`, `submit_route`, `layout`, `input_mode`, `parser_cost` | `exact_verdict` (required) | Agent facts (all verified for both agents). Conformance: a recorded release's measurement must not be `Failed` where the fact is verified. |
| `key_effects` | `exact_verdict` (first-stage commits, staging) | Agent fact, per dialog the union of the recorded releases: a key's effect is the recorded one where the releases agree and `Commits` where they differ (Codex folder trust `2`: `Dismisses` on 0.164.0, `Commits` on 0.160.0, so the fact says `Commits`). Conformance: every recorded key effect is the fact's or the fact is `Commits`; every recorded dialog and key is in the facts. |
| `first_modal` | `exact_verdict` (settle window) | Agent fact, the slowest over the recorded releases (Codex 4461 ms, from 0.160.0; settle 6.69 s, was 6.03 s from 0.164.0's 4020 ms). Not per release, and nothing new is measured for it. Conformance: no recorded release's slowest first dialog is later than the fact's. |
| `release_recogniser` | `exact_verdict` (required) | Agent fact: the agent's start-up header is read by `release_scanner`'s recogniser. The recogniser itself stays, because the latch stays (see "The header latch stays" below); it latches any `MAJOR.MINOR.PATCH`, so it is not a version gate. Conformance: unchanged (`doorbell_036_release_headers_latch_from_real_launches`). |
| `head_echo` | `exact_verdict` (required when staged) | Agent fact: Codex verified (from 0.164.0), Claude absent (Claude's line is not staged). 0.160.0's absent measurement is "not measured", which conformance accepts; only `Failed` contradicts. |
| `placeholder_empty`, `delete_key`, `transcript_echo` | `typing_for` (caps) | Agent facts (Claude `placeholder_empty` stays `Failed`, so withdrawal is still unconfirmable for Claude). |
| `editor_probe` | `composer_probe::permission`, observer settlement (keymap evidence), the e2e assumption | Agent fact: Claude's is the measured `CLAUDE_2_1_317_EDITOR_PROBE` (2.1.318's is equal); Codex's stays absent until the Codex activation unit. Conformance: a recorded release's verified `editor_probe` equals its agent's. |

## `RELEASE_RECORDS` entries

| Entry | Disposition |
| --- | --- |
| Claude Code 2.1.310 | Conformance input. Its fields are Claude's facts except `editor_probe` (absent: not measured). |
| Claude Code 2.1.312 | Conformance input, as 2.1.310. |
| Claude Code 2.1.317 | Conformance input; source of Claude's `editor_probe` fact. |
| Claude Code 2.1.318 | Conformance input; its `editor_probe` equals 2.1.317's. |
| Codex 0.160.0 (`missing_measurement`: `head_echo` absent) | Conformance input. No longer a verdict: a 0.160.0 latch gets Codex's facts and is `Go`, staged. Its slower first dialog sets Codex's settle fact. |
| Codex 0.164.0 | Conformance input; source of Codex's `head_echo` fact. |

The table is renamed `RECORDED_RELEASES` and no production code reads it.

## Code that goes, and what replaces it

- `activation` rule 2 (exact record) and rule 3 (`unmeasured_release`): replaced by "the agent's facts decide" (`facts_verdict`, was `exact_verdict`). An agent with no facts (OpenCode, Pi, Devin) is `agent_not_supported`; a generation whose agent type is not known yet is `unknown_agent` (both were `unmeasured_release`).
- `NoGo::UnmeasuredRelease`, `Measurement::Unraisable` (whose only meaning was `unmeasured_release`), `measured_releases`, the `measured=` field of the `doorbell off` log line: deleted.
- `UNRECORD_CLAUDE_RELEASES_ENV`, `parse_unrecorded_claude_releases`, `unrecorded_claude_releases`, `without_unrecorded_claude` (an e2e seam that made the installed Claude unmeasured): deleted, there is nothing left for them to exclude.
- `composer_probe::InertCause::UnmeasuredRelease`: becomes `UnsupportedAgent` (an agent with no facts). The permission needs a latched header (any release) and the agent's verified `editor_probe`.
- `with_assumed_editor_probes` (records in, records out): replaced by `editor_probe_for(agent, release, assumed)`. The e2e assumption keeps its `agent@release` entries, because the stand-ins draw a fixed header: a generation that latched exactly that release gets the stand-in's editor probe. It no longer requires a record for the release (any bare release of `claude` or `codex`).
- New: `NoGo::TurnedOff` (`turned_off`), from the per-agent switch.

## The header latch stays

Rule 1 (`unknown_release`, `release_conflict`, `scanner_geometry`, `scanner_budget`) is a per-session check: it says this generation drew its agent's start-up screen when it started, which is how the deck tells a fresh start from a resumed session or something else in the pane. It does not choose the profile (the agent type does) and does not compare versions: the recogniser latches any `MAJOR.MINOR.PATCH`. It stays, with its user docs. Residual: a pre-release header (`2.2.0-beta`) does not latch, so it reads `unknown_release`. That is unchanged by this unit and named in the report.

## The switch

`doorbell.claude` and `doorbell.codex` in the deck's `config.toml` (default `true`; `dot-agent-deck config set doorbell.claude false`), overridden by `DOT_AGENT_DECK_DOORBELL_CLAUDE` / `DOT_AGENT_DECK_DOORBELL_CODEX` (`0`/`false`/`off`, `1`/`true`/`on`; env wins, as `DOT_AGENT_DECK_EXPERIMENTAL` does). The daemon reads it when it starts an agent, so it applies to agents started after the change. Off: no helper is installed (no input-box check), and the doorbell types nothing (`turned_off`), so the card shows `waiting for the agent to fetch it. Send it a prompt.`; hooks, native wake and `inbox fetch` still deliver. It is daemon-side, so the TUI and the desktop app show the same thing. The clause stays in the daemon (log and hold facts): only the hold `cannot_type`, which already exists, reaches a client, so no wire shape or meaning changes.

## Tests that pin the per-release gate, and the plan for each

Assertion edits that only pin the old gate are the bounded exception to "workers don't edit the tester's assertions"; each is listed in the unit's report with before and after.

| Test (catalog ID) | What it pins | Plan |
| --- | --- | --- |
| `tests/doorbell_activation_table.rs` (`inbox/doorbell/053`): `go_iff_every_required_measurement_is_verified`, `unlatched_releases_and_capabilities`, `key_effects_and_own_settle`, `a_staged_line_needs_its_head_echo_measured`, `every_recorded_dialog_leaves_the_token_head_inert`, `an_editor_record_decides_no_ring`, `editor_records_move_no_verdict_in_any_table` | the verdict over one record | Re-pointed at `facts_verdict` over agent facts; the same enumerations. `Unraisable` leaves the enumeration. |
| same file: `only_an_exact_record_admits`, `claude_and_other_agents_need_their_own_record` | rule 3 (`unmeasured_release`) | Rewritten to the new behaviour: any latched release of Claude/Codex gets its agent's facts (newer, patch, older, distance 1–5); other agents are `agent_not_supported`. |
| same file: `claude_record_exclusion`, `exclusion_never_reaches_codex`, `exclusion_only_ever_disables` | the e2e Claude exclusion | Deleted with the seam. |
| same file: `the_shipped_records_verdicts`, `the_measured_2_1_317_editor_record`, `the_measured_2_1_318_editor_record`, `the_editor_probe_assumption_moves_no_shipped_verdict` | the shipped records' verdicts | Re-pointed: the shipped facts' verdicts (Claude Go unstaged, Codex Go staged with settle 6.69 s), and every recorded release conforms to its agent's facts; mutating a fact the recordings contradict fails conformance (red/green). |
| `tests/editor_probe_permission.rs` (`inbox/doorbell/053`): `permission_needs_the_header_a_verified_record_and_the_root`, `only_the_activated_release_is_armable`, `the_editor_probe_assumption_refuses_unknown_entries`, `the_editor_probe_assumption_never_supplies_a_release` | permission by exact record; assumption parsing against records | Rows for "unmeasured, a patch past it / older" become armable with Claude's facts; "Codex's release number" becomes inert for `NoEditorRecord` (Codex has no verified `editor_probe` yet); the assumption accepts any bare release of a known agent. |
| `tests/doorbell_screen_profiles.rs` (`inbox/doorbell/036`): `go_no_go_over_measured_key_effects` | reads `RELEASE_RECORDS` | Iterates `RECORDED_RELEASES`; asserts unchanged. |
| `tests/codex_head_echo_evidence.rs` (`inbox/doorbell/036`): `codex_token_head_bytes_are_inert_in_every_dialog` | reads `RELEASE_RECORDS` | Iterates the recorded releases and Codex's facts. |
| `tests/editor_keymap.rs`, `tests/editor_observer_processes.rs`, `src/daemon_composer_reader_tests.rs` | types from the module | Import updates only. |
| `tests/editor_installer.rs` (`inbox/probe/064`) | `INSTALL_ROLLOUT == [Direct(Claude)]` | Unchanged; adds the switch refusing installation. |
| `tests/e2e_claude_unmeasured_release.rs` (`inbox/doorbell/056`, lane 1), renamed `tests/e2e_claude_doorbell_turned_off.rs` | a Claude stand-in at an unrecorded release is held `unmeasured_release` | Rewritten: the switch turned off (through the deck's `config.toml`) holds it as `turned_off` with the `cannot_type` words, and a prompt still recovers it; switched back on, the same unrecorded release is rung on Claude Code's facts. |
| `tests/e2e_inbox_claude_live.rs` `claude_050_live_unmeasured_release_is_held_and_a_prompt_delivers` (`inbox/claude/050`, lane 2) | the exclusion env and `unmeasured_release` | Re-pointed at the switch: `DOT_AGENT_DECK_DOORBELL_CLAUDE=0` holds the doorbell as `turned_off`, a prompt delivers. Helpers `measured_releases`/the `RELEASE_RECORDS` precondition go. |
| `tests/e2e_editor_installation_claude_live.rs` `probe_044_live_an_unmeasured_release_is_installed_and_never_checked` (lane 2) | an unrecorded Claude release is installed but never armable | Rewritten: an unrecorded release is armable like any Claude; the never-checked half moves to the switch (not installed). |
| `tests/support/claude_installed.rs`, `tests/e2e_chain_smoke_claude.rs`, `tests/support/codex_inbox_live.rs`, `tests/e2e_inbox_codex_live.rs`, `tests/e2e_inbox_pi_live.rs` (lane 2 preconditions and log reads) | "the installed release has a Go record", `measured_releases`, `unmeasured_release` in a log | Preconditions become "the agent's facts are Go"; Pi's log read expects `agent_not_supported`; Codex live no longer skips an unrecorded release. |
| `tests/e2e_inbox_codex.rs` `codex_024_a_pane_its_hooks_call_codex_gets_no_byte` (lane 1) | `held_as == [unmeasured_release, unknown_release]` | `[unknown_agent, unknown_release]`. |
| `src/doorbell_generation.rs` tests, `src/agent_pty.rs` latch/budget tests, `src/doorbell.rs` obstacle table, `src/inbox_hub.rs` typed-fallback tests | `unmeasured_release` as a clause value or rule-3 verdict | The clause string becomes `agent_not_supported`/`unknown_agent`; the hub tests use the clause only as an opaque value. |
| `tests/e2e_editor_assumption.rs` (`inbox/probe/066`) | assumption reader refuses a release with no record | A release with no record is accepted; the other refusals stay. |
| `xtask/cross-version` (`breaks.rs`, `outer.rs`, `probes.rs`, `report.rs`) | mentions `INSTALL_ROLLOUT` / records in prose or probes | Checked; changed only where it reads the removed API. |

## Docs

- `docs/orchestration.md`, `docs/troubleshooting.md`: the `unmeasured_release` and "a version the deck has measured" wording goes; the switch is added with what the user sets and sees; "after an agent update the doorbell misbehaves" says to turn the switch off and report it. `docs/configuration.md` lists the two keys.
- `docs/develop/doorbell-admission.md`: the rule list becomes agent facts plus conformance.
- `tests/CATALOG.md`: every entry above whose assertions change.
- `changelog.d/1666-*`: a sibling fragment for the user-visible change.
