# Desktop release-check RED contract

This test-only slice stacks on SC-10 at `47069589`. `desktop/src/components/InboxSurfaces.test.tsx` extends `inbox/status/030`; `desktop/src/components/ReleaseChecks.test.tsx` extends the desktop coverage of `inbox/release-check/004`, without inventing another catalog ID. They import the existing `tests/fixtures/inbox-display.json` for the seven card sentences, Details results, diagnostics, modes, advice and confirmation words. The ask-waiting sentence remains the design proposal documented in [the TUI contract](tui.md), rather than an agreed new hold cause.

## Production surface and substituted boundary

The tests mount the real `DeckShell`, its real components, `VOICE_ACTIONS` and the real voice-panel dispatcher. The terminal viewport is a text placeholder, never a PTY or an agent. The external runtime action callback supplies synthetic replies; no daemon socket, export filesystem, real agent configuration or credential is opened. The microphone and intent resolver are synthetic. These tests do not measure the command model's understanding of natural language or prove daemon mutation, scrubbing, persistence or agent admission.

The existing card test goes through `mapDesktopSnapshot` with additive `holdDetail.agent`, `release`, `fact` and `reason` fields, using `wakeHold: "cannot_type"` and a `release_check_*` cause. Its oracle is the shared card template filled with fixed fixture data; the production wording function does not compute expected results.

The shell test supplies a proposed frontend projection: `DeckSnapshot.releaseChecks`, `DeckSnapshot.releaseCheckMode`, `ConnectionView.capabilities`, and `AgentSession.agentVersion`. Rows mirror the SC-08 wire contract with camel-case `queuedAtMs` and `queuePosition`: agent, release, identity, nullable state, progress, optional fact/reason. Missing capability, an empty list and an unrelated capability list are distinct inputs. Production must populate this projection from the owning daemon, not manufacture support from the presence of records. A real pane id and incarnation identify the fixture's current idle agent; the snapshot's short record identities remain display abbreviations.

These are failing-first interface proposals, not existing desktop bindings. When SC-11 chooses its bridge projection, adapt the test fixture to that boundary while retaining the visible assertions and release-scoped requests. Ordinary production construction must not gain a second test-specific renderer or action path merely to satisfy this fixture.

## Controls and actions

Details on the hold line opens a dialog named `Release checks`. Records have selectable buttons including their short identity; selection changes the displayed result and diagnostic. Queued/running rows show progress and the actual queue position, including 8 or explicit unavailable-position guidance. Unknown state/progress is explained rather than silently called passed. Unknown fact/reason uses the shared `other` explanation. Details reads the selected row's own fact/reason while the pane deliberately reports a different tripped/layout diagnostic.

The shell's external `runAction` receives the following proposed frontend actions. The existing desktop bridge may translate these into SC-08's wire operations; Export and Mode are local adapter operations, not added wire variants. Every request includes the owning `deckId`, checked independently by the adapter, and the listed fields are exact: an unexpected identity selector fails. The adapter owns a separate copy of the records: its mutations cannot change the initial snapshot by shared reference, so the production reply handler must apply the returned state for the visible assertions to pass.

| Frontend action type | Other fields | Result used by the fixture |
| --- | --- | --- |
| `release_check_run` | agent | current release row becomes running, position 1 |
| `release_check_cancel` | none | the running check becomes inconclusive / runner_stopped |
| `release_check_forget` | agent, release | records of that release become forgotten; another release stays passed |
| `release_check_export_preview` | agent, release | files with name and already-scrubbed inspection content; no write |
| `release_check_export` | agent, release, dir | accepted write reply |
| `release_check_mode` | mode | updated mode and releaseChecks; off cancels the running check |

Q100b governs the scope. Re-check uses the current identity even when an older record is selected. Cancel stops the running check even when a completed record is selected. Forget affects the selected release's records; its confirmation includes the shared release-wide sentence when two identities share the release and omits it for one. Export previews/writes the selected release, including both displayed identities. Forgotten rows retain Re-check and Close and omit unusable Cancel, Forget and Export. No-reference and off holds explain how to proceed without an unusable Re-check.

The agent-details settings surface is opened with `p`, the SC-10 proposal. It has a `Release checks` select with auto/ask/off values and the shared explanatory labels. This key, accessible names and action ids are concrete proposals for implementation to reconcile with existing desktop shortcuts. They do not justify dropping parity. Off during a running check opens the shared confirmation; Escape preserves ask and sends nothing; `Turn off` applies off and the stopped reply. The non-running cycle covers auto → ask → off → auto.

Cancel and Forget open confirmations in this desktop contract. This applies the standing voice D5 rule: a spoken action that stops or deletes opens a confirmation answered by hand. Both click and voice use the same registry entry and confirmation; acceptance is another classified registry action. The fixture checks dismissal before acceptance for Cancel and Escape before Off. Approval through Re-check in ask mode follows SC-10's explicit action.

## Export inspection and inert text

Export opens `Export release check`, lists record.json, trace.txt and README, explains the selected release's scope and shows the shared scrubber limitation. `Open <name>` shows a `File contents` region; `Back to files` returns to the list. `Continue` opens the directory form; `Directory` is editable, `Export destination` echoes the presentation copy, and `Export files` alone writes. `Cancel export` from the list, inspection or directory returns to Details without a write. Adapter assertions distinguish preview from write, and the successful-write test checks a visible exported result.

The inert-display rule from SC-10 applies before creating DOM text. C0 other than LF, DEL and C1 render as lowercase two-digit `\xNN` escapes; LF stays a text line boundary. Preserve ordinary Unicode and literal backslashes. CSI erase/cursor/colour, OSC 0 titles, OSC 52 clipboard writes, C1 CSI, CR and backspace are inert. HTML is text, with no injected script/img/iframe/object element. These are presentation rules, independent of secret-pattern scrubbing and the bytes written to an export.

The audit extensions have explicit expected outputs: bidi marks/overrides/isolates (`U+061C`, `U+200E/F`, `U+202A–E`, `U+2066–9`), zero-width characters (`U+200B/C/D`, `U+2060`, `U+FEFF`) and `U+2028/2029` render as lowercase `\u{hex}` escapes. This inspection policy is deliberately stricter than `displayText.ts`'s ordinary-name stripping policy: an evidence viewer must let the user see the invisible bytes. Adapter file names, errors and the echoed destination use the same escapes. The raw editable directory value is input and is retained; its echo is escaped. Scans include text and DOM attributes of the relevant rendered surface, not just the visible preview's first line.

The proposed inspection budget is at most 16,384 Unicode code points including the truncation marker, applied after escaping. A megabyte line with a bidi prefix and trailing sentinel must show the escaped prefix, a visible truncation marker and no sentinel. The numeric budget is an SC-11 proposal for the previously unspecified bound; adjust it in design and tests together if the implementer chooses another finite budget. The hostile-trace tests compare the surrounding agent card before/after inspection. They do not execute an OSC or claim clipboard behavior from a synthetic terminal viewport.

## Voice decisions while designing the surface

| Visible/spoken action | Proposed registry entry | commands.toml id |
| --- | --- | --- |
| Re-check | `releaseCheckRun` | `release_check_run` |
| Details | `releaseCheckDetails` | `release_check_details` |
| Cancel release check | `releaseCheckCancel` | `release_check_cancel` |
| Forget release check | `releaseCheckForget` | `release_check_forget` |
| Export release check | `releaseCheckExport` | `release_check_export` |
| Release checks auto | `releaseCheckAuto` | `release_check_auto` |
| Release checks ask | `releaseCheckAsk` | `release_check_ask` |
| Release checks off | `releaseCheckOff` | `release_check_off` |

Mode uses three fixed commands so it needs no new open-ended resolver kind. The agent/record is the visible selection; nothing resolves a spoken sixteen-hex abbreviation as authority. The card's Details and Re-check and the dialog's controls dispatch through the same entries used by the voice panel. The tests instrument the real registry callable on its existing object, so dynamic `dispatchVoiceAction` and direct control calls are observed, and they still have to produce their visible result.

The voice test reads the real `commands.toml` as a raw asset and pins each id/invoke pair. Synthetic utterance results enter the real voice panel, cross the registry and assert visible dialogs/results plus the exact external requests. They prove post-resolution dispatch parity for the listed phrases; they are not evidence of intent-model, resolver-kind or meaning-check acceptance. SC-11 must also extend the command table's Rust inventory, control-label outcome cases and paraphrase fixtures when implementing these commands, per voice-first-design §9. Bare Cancel is ambiguous with dismissing a preview or another form; this contract deliberately voices the qualified Cancel release check and Forget release check. If the implemented visible labels remain shorter, record the deliberate vocabulary exception in voice-first-design §5 or choose qualified labels in both clients.

The additional controls need registry entries too. The following are proposed `no_voice` fitness reasons someone may disagree with; a supported voice route is also acceptable. The test requires an entry with voice enabled or a substantial written reason, not a new control-site allowlist escape.

| Entry | Proposed reason if kept off voice |
| --- | --- |
| `releaseCheckSelect` | The short identity is ambiguous display text, not a spoken identity selector; record selection is made directly in the visible Details list. A voice design needs declared numbered candidates before addressing individual records. |
| `releaseCheckInspect` | Inspection opens one file from an export manifest; the current voice declaration has no export-file candidates. A numbered manifest resolver would make this voiceable; matching arbitrary file names outside that declaration would not. |
| `releaseCheckDirectory` | This edits an arbitrary filesystem destination in a local save form. The current resolver has no export-directory authoring context, and speech must not silently turn a path name into a write destination. |
| `releaseCheckExportContinue` | This advances the local inspection list to its destination form. The current voice declaration does not identify the export subview, so a bare Continue cannot distinguish this step from another open form. |
| `releaseCheckExportWrite` | This is the final approval to write files to the chosen directory; D5 keeps the consequential approval manual. Voice Export opens the review, not this approval. |
| `releaseCheckExportCancel` | This dismisses the local export subview without writing. Voice Close can serve that dismissal through the topmost-overlay close action; a distinct Cancel phrase would be ambiguous with cancelling the running check. |
| `releaseCheckConfirmCancel` | This is the manual acceptance of a stop request; D5 requires a human confirmation after a spoken Cancel. |
| `releaseCheckConfirmForget` | This is the manual acceptance of removing the selected release's records; D5 requires a human confirmation after a spoken Forget. |
| `releaseCheckConfirmOff` | This is the manual acceptance of stopping a running check by switching off; D5 requires a human confirmation after the spoken mode request. |

Escape, Back to files, Close and Keep checking are dismissals/back-navigation and may share existing close/back registry actions; the capability entries above are the new ones explicitly pinned here. A green control-site ratchet cannot prove keyboard and voice context wiring. This test-only slice adds no production handler or `voice-control-sites.toml` entry; rule 14 can stay green while the feature tests are RED. When implementing, add capability handlers as single registry dispatches and use a written allowlist reason only for a genuine non-capability or intentional no_voice control, per rule 23.

## RED and GREEN obligations

Today's card tests execute and fail on the generic cannot-type sentence. Details/export/diagnostic cases fail at the absent Details button, so the later assertions are a contract awaiting implementation, not executed safety evidence. Mode/unsupported cases fail at absent agent-settings UI. The per-command registry tests fail because the entries are absent; the voice Details/Re-check tests independently reach the real voice panel and fail because the proposed dispatch has no registered action. These failures must not be described as tested cancellation, sanitisation or export behavior.

After implementing, rerun the failing case in isolation, then both touched vitest files and the frontend suite; run TypeScript checking, linkage-check rule 14 and Scenario generation. The Rust fast tier on this stack is already compile-RED from the accepted SC-08/SC-10 contracts. Those interfaces and the TUI input/rendering seam must become GREEN with their owning implementation before the combined feature is handed off. No new `#[spec]` annotation is introduced here; each new vitest scenario has its Scenario comment.

Recorded on 2026-10-31 against the SC-10 stack at `47069589`: `cd desktop && pnpm test src/components/InboxSurfaces.test.tsx src/components/ReleaseChecks.test.tsx` executes 121 cases: 68 expected failures (7 new card cases and 61 new shell/voice cases), 53 existing passes, no skipped tests. The running card expected `✉1 waiting: checking Claude Code 2.1.313 on this machine.` and received `✉1 waiting for the agent to fetch it. Send it a prompt.` Details-dependent cases report `Unable to find an accessible element with the role "button" and name "Details"`; settings cases report the missing `Release checks` combobox or unsupported guidance. Registry classification reports `Re-check: missing registry entry releaseCheckRun: expected undefined to be defined`. The voice Details and Re-check cases establish that the synthetic utterance reached `resolveVoice`, then report zero calls to the missing action. The other voice cases stop at the absent prerequisite Details control.

`pnpm exec tsc --noEmit --pretty false`, `cargo fmt --check`, `cargo xtask linkage-check` (1836 catalog ids, 2487 annotations, 93 allowlisted, 18 rules) and `cargo xtask docs --tests` exit 0. `cargo clippy --workspace --all-targets --features e2e,e2e-live -- -D warnings` exits 101 on the inherited SC-08 contract: unresolved `daemon_protocol::CAP_RELEASE_CHECK` and absent `DaemonClient.release_check_run`, `release_check_cancel` and `release_check_forget`. `cargo test-fast` exits 101 before running tests on SC-10's absent `ui::render_release_check_key_sequence_to_buffers`. Linkage, Scenario generation and fast compilation ran through RWX and its sandboxes were stopped afterwards; the all-feature clippy invocation stayed local. These intentional stack failures are recorded separately from the desktop behavior RED and require the owning implementation, not test removal or quarantine.
