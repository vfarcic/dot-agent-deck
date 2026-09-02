# PRD #803: A settings surface for the desktop app

**Status**: Planning — awaiting confirmation of the plan
**Priority**: High (three PRDs are blocked on it)
**Created**: 2026-09-02

## Problem Statement

Three PRDs each need somewhere to put a setting, and none of them owns that surface.

- [#741](https://github.com/vfarcic/dot-agent-deck/issues/741) — daemon endpoints: add, name, select, remove; and with [#742](https://github.com/vfarcic/dot-agent-deck/issues/742), several at once.
- [#743](https://github.com/vfarcic/dot-agent-deck/issues/743) — follow the OS light/dark appearance, with an in-app override that has to persist.
- [#802](https://github.com/vfarcic/dot-agent-deck/issues/802) — which speech-to-text backend, which intent model, local versus remote per stage, an optional API key, and ~2.5 GB of model downloads.

Left alone, each grows its own half of a settings surface and they will not agree with one another. That is the failure this PRD exists to prevent, and it is cheap to prevent now and expensive later.

**The desktop app has no application-settings mechanism of any kind today.** Measured on `main` at `46ac28c`: the Rust crate `desktop/src-tauri` writes no file of its own anywhere — the only `fs::write` in it is inside `#[cfg(test)]` at `lib.rs:1253`. The frontend persists exactly four keys, all in `window.localStorage`, all namespaced by `modeScopedKey` (`desktop/src/lib/bridge.ts:184-192`) with a `.live`/`.fixture` suffix: workflow role order, projects, agent profiles, and the prompt library. No Tauri plugin of any kind is enabled — `tauri` is declared `features = []` and the capability set is `["core:default"]` only (`desktop/src-tauri/capabilities/default.json`) — so `tauri-plugin-store` is a new dependency plus a new capability, not a configuration toggle.

**A `Settings` rail button already exists and is a dead stub.** `desktop/src/App.tsx:305`:

```tsx
<RailButton icon={Settings2} label="Settings" onClick={() => setNotice("Runtime settings will use the same local configuration seam.")} />
```

It is the only one of the six rail buttons with no `active` prop and no `testId`, and it opens nothing. That toast is a promise with no implementation behind it: there is no "local configuration seam" for application preference anywhere in the desktop app. This PRD fills a slot that was deliberately reserved.

## Solution Overview

A per-installation settings store owned by the desktop app's Rust core, and a Settings surface that renders sections contributed by features.

This PRD defines **where settings live, how they persist, and how a feature registers one — and stops there.** It ships no opinion about what a daemon endpoint, an appearance override or a voice backend should be; those belong to #741, #743 and #802 respectively. A container that grows opinions about its contents blocks all three of them, which is the outcome this document is written to avoid.

The spine is not a screen. It is an **ownership rule**, recorded in #741 on 2026-09-02 and adopted here as the criterion every future setting is tested against:

> **The desktop app gets everything from the daemon, wherever that daemon runs. The only thing it owns is its own settings.**

#803 is the "its own settings" half of that boundary. Saying plainly what qualifies — and what does not, because it belongs to the daemon — is the most valuable thing this PRD produces, and it is useful to the three dependents before a single line of the container is written.

## Scope

### In Scope

- **The ownership boundary as a written, applicable rule**: what is client-owned, what is daemon-owned, and the criterion a future feature uses to tell which side its setting lands on.
- **A typed settings document** owned by `desktop/src-tauri`, persisted as TOML in the per-platform config directory, loaded at startup and written atomically with owner-only permissions.
- **A Settings surface** reachable from the existing rail stub, the command palette and Escape, rendering a registry of feature-contributed sections.
- **The registration contract**: what a feature adds — and where — to acquire a setting, documented well enough that #741, #743 and #802 each implement theirs without consulting this PRD's author.
- **A named approach for credential storage**, with the one hard rule this PRD sets and a guard that enforces it. The backend implementation is #802's.
- **One container-owned preference**, end to end, so the contract is proven by a real consumer rather than only by tests (see Open Question 2).

### Out of Scope

- **Every setting the three dependents need.** Daemon endpoints are #741's, the appearance override is #743's, the voice backends and the API key are #802's. This PRD gives each a place to land and a contract to follow; it does not define, design, or pre-create their sections.
- **Model downloads** — progress, retry, disk-usage visibility and removal. They are #802's, and the reason is concrete rather than a boundary preference: the webview's CSP is `connect-src ipc: http://ipc.localhost` (`desktop/src-tauri/tauri.conf.json:25`) and nothing else, so a download cannot be initiated frontend-side at all. It has to be Rust-side work in the feature that needs it, and the settings screen is where its *controls* are rendered, not where the download lives.
- **Migrating the four existing `localStorage` keys.** They hold projects, agent profiles, prompts and workflow order — per-project draft content, not per-installation preference. Under the boundary rule they belong daemon-side ([#819](https://github.com/vfarcic/dot-agent-deck/issues/819)), and moving them into the settings store now would entrench them on the wrong side. Named explicitly because it is a tempting and wrong move.
- **Syncing settings between machines.** Everything here is deliberately per-machine (see Technical Approach).
- **A driver-level test tier for the desktop app.** None exists in this repo and this PRD does not build one (see Testing).
- **Any daemon protocol change.** See Cross-version safety.

## Technical Approach

### The ownership boundary

The rule, and the two lists it produces. This is the part #741, #743 and #802 consume directly.

| | |
| --- | --- |
| **Daemon-owned** | the project config, the coordinator context, cwd and project paths, available orchestrations/modes/roles, dispatch targets, the agent list and PTY streams, and telemetry |
| **Client-owned** | settings — endpoints, appearance, model backends — plus genuinely presentational state: window size and position, focused tab, zoom |

The criterion for a new item, stated so it can be applied without re-deriving the reasoning: **a setting is client-owned when it describes the client itself — this machine, this display, this installation. If it describes the work, the project, or the machine the agents run on, it is daemon-owned and does not belong in this store, however convenient it would be to put it here.**

The failure this prevents already shipped once. Reading `.dot-agent-deck.toml` client-side gives the right answer when the daemon is local, so nothing looked wrong until a real remote session put a local `/Users/...` path in the header beside the remote's `/home/vfarcic/...` panes. A rule that applies only to remote leaves the same trap for the next feature; a rule that applies always cannot.

### Per-machine, deliberately

Everything in this store is about *this installation on this machine*, and that is a decision rather than an accident of implementation. An endpoint is a description of reachability from this machine — a socket path, an ssh host, a forwarded socket — and does not travel. An appearance override is about this display. Downloaded models are on this disk.

The precedent, and the cautionary tale, is the TUI's. `keybindings_path()` (`src/keybindings.rs:891-901`) and `config_path()` (`src/config.rs:198-203`) both resolve through `platform::paths::config_dir()`, which is `$HOME/.config/dot-agent-deck` on the machine the *process* runs on. `dot-agent-deck connect` ssh's into the remote and runs the TUI there (`src/connect.rs:1-20`, command built at `:752-786`), passing only `DOT_AGENT_DECK_VIA_DAEMON=1` — `SendEnv`/`AcceptEnv` is deliberately not used (`src/connect.rs:746-748`). So a `keybindings.toml` edited on the laptop is never read, and a real user lost time to exactly that.

The desktop app never ssh's, so its settings are read on the machine that wrote them — the correct side of that split. The lesson to carry forward is the general one: **never store desktop settings daemon-side, and never read daemon-owned state from the client's disk.** The moment #741 lets the app point at a remote daemon, that rule is what keeps the two halves from crossing.

### Where the document lives

`platform::paths::config_dir().join("desktop.toml")` — a **sibling of** the TUI's `config.toml` and `keybindings.toml`, never a section inside them. `DOT_AGENT_DECK_DESKTOP_CONFIG` overrides the whole path, matching the `DOT_AGENT_DECK_CONFIG` convention (`src/config.rs:199`) and giving tests a seam.

**Sharing the TUI's `config.toml` is rejected, and the reason is mechanical rather than aesthetic.** `DashboardConfig::save()` serialises the struct — `toml::to_string_pretty(self)` at `src/config.rs:144` — so a `[desktop]` table the TUI does not know about would be **silently deleted** on the next TUI write. The TUI would have to learn the desktop's schema for sharing to be safe, which is the coupling the split exists to avoid. The field lists are disjoint in any case: `DashboardConfig` is `default_command`, four `bell.*` flags and `auto_config_prompt` (`src/config.rs:99-105`, `:63-70`), none of which has a counterpart in anything #741, #743 or #802 would set.

**`localStorage` is rejected as the source of truth**, though the four existing keys stay where they are. Four reasons, in order of weight: an API key must never sit in webview-local plaintext, and #802 needs one; the store is invisible to the user, uneditable outside the app, and cleared by a webview data reset; it is mode-scoped by `modeScopedKey`, and fixture/live scoping is right for project drafts but wrong for app preference — #743's own issue already flags that a theme choice "is genuinely global and probably wants to sit outside that scoping"; and the Rust core resolves the daemon endpoint per operation (`daemon_bridge.rs:181-194`), so #741's setting has to be readable from Rust.

**`tauri-plugin-store` is rejected** as the considered alternative: it would put the store frontend-side — the wrong side for both of the above — and it adds the app's first plugin dependency and first capability beyond `core:default`, against an existing repo convention for per-platform TOML config with a reusable path helper.

**The macOS path is a knowing divergence.** `config_dir()` falls into its `#[cfg(unix)]` arm on macOS and returns `$HOME/.config/dot-agent-deck`, not `~/Library/Application Support` (`src/platform/paths.rs:1341-1353`). That is deliberate for the TUI and looks less right for a bundled `.app`. Taken as-is here for consistency — one directory per machine, so a user running both front-ends finds both configs in one place — and recorded as Open Question 3 because it is cheap to change now and a migration later.

### The document's shape and its failure behaviour

A serde struct with one nested struct per feature section, `#[serde(default)]` throughout, and **no `deny_unknown_fields`** — matching the deliberate choice `DashboardConfig` records for issue #519 (`src/config.rs:96-99`). Two consequences, both wanted: a field added by a newer build is tolerated by an older one, and a missing file yields defaults rather than an error.

**Loading never fails.** A missing file, an unparseable file or an unreadable file all yield the defaults, exactly as `DashboardConfig::load()` does (`src/config.rs:119-135`). A settings file is not worth failing an app launch over.

**Writing is atomic and owner-only** — temp file in the same directory, then rename, with mode `0o600` on Unix. The repo has precedent for the permission assertion (`src/codex_hooks_manage.rs:1023`, "a deck-created config.toml must be owner-only").

Unlike `DashboardConfig::save()`, the writer must not be able to drop a table it does not know about. The struct owns every section, so within one build that cannot happen; across builds the serde defaults cover it. A **pinned-shape test** in the idiom of `agent_mapping_is_frontend_stable` (`desktop/src-tauri/src/dto.rs:639-647`) asserts the exact serialised document, so adding a field is a deliberate act that shows up in a diff and forces the ownership question to be answered in review.

### How a feature registers a setting

The whole contract, and it is deliberately small.

1. **Storage** — add a field to your feature's section struct in `desktop/src-tauri/src/settings.rs`, or add a new section struct and one line to the parent. `#[serde(default)]` gives it a default.
2. **UI** — add one row to the frontend's section registry (`id`, `label`, `icon`, `component`) and own your panel component. Panels are ordinary React components rendered inside the settings sheet; nothing about a panel is generic, because #741's endpoint list and #802's model manager are not key/value widgets and pretending otherwise would produce a renderer that fits neither.
3. **Secrets** — never in the document. Use the seam below.

Adding a *setting* is step 1 plus an edit to your own panel. Adding a *section* is one registry row plus a component.

### The Settings surface: a sheet, not a screen

The app has **no screen or navigation model on `main`** — one always-mounted `ControlDeck` (`desktop/src/App.tsx:61-63`), and five of the six rail buttons toggle overlay booleans over it. The established pattern for "configure a thing" is the right-hand `config-sheet`: `<div className="sheet-backdrop">` wrapping `<section className="config-sheet …" role="dialog" aria-modal="true">`, used by all four panels in `ConfigurationPanels.tsx` (`:45`, `:118`, `:181`, `:352`), styled at `desktop/src/styles.css:294-295` as a full-height panel `width: min(780px, calc(100vw - 74px))`.

Settings becomes the sixth overlay. 780px full-height is enough for an endpoint list or a model manager, the pattern is what a user of this app already recognises, and — decisively — **it makes this PRD independent of PR #779's landing** (see below). Three existing mechanics must be respected or the surface will be subtly broken: the single Escape handler that closes everything (`desktop/src/App.tsx:173`), the command palette's panel list (`:284-292`), and the backdrop `onMouseDown` close.

This is reversible on purpose. If #745 iteration 3 converts the rail into real navigation, Settings becomes a `DeckView` variant and the panel component moves unchanged.

### Credential storage

**The rule this PRD sets, and the only thing about credentials it settles: a secret never goes in `desktop.toml` and never in `localStorage`.** The document may hold a non-secret reference — which backend holds the key, or a boolean saying one is stored — and nothing more. A unit test over the serialised default document's key names, with an allowlist, keeps that honest in the same idiom as `xtask/linkage-check`.

There is **no credential storage anywhere in this repo today** — `keyring`, `secret-service`, `security-framework` and `stronghold` return zero matches workspace-wide. What exists is credential *detection* in the e2e harness, which reads and never writes (`tests/common/mod.rs:2882` for `~/.claude/.credentials.json`, `:2739` for the macOS Keychain probe via `security`). So #802 has no precedent to follow and no dependency to reuse, which is precisely why naming the approach here is worth doing.

The approach, named and not built: a **`SecretStore` seam** in the desktop crate — store, load, delete, keyed by a stable identifier — with the OS keychain as the intended implementation (`keyring`: macOS Keychain, Windows Credential Manager, Linux Secret Service). Two things must be designed rather than assumed when it is built, and they are recorded here so #802 does not rediscover them: a Linux box with no Secret Service needs a documented, non-silent failure path, and an owner-only file in the config directory is the viable fallback if one is wanted. "Read it from the environment", which is what every existing path in this repo does, is not available to an app launched from Finder.

### Cross-version safety

**CLAUDE.md rule 12 — did this change the TUI↔daemon contract? No.** `PROTOCOL_VERSION` stays **8** (`src/daemon_protocol.rs:227`). No `.breaking.md`, no cross-version manual test.

The reasoning, stated rather than asserted: settings are client-owned by definition, so the document is read and written entirely within the desktop process, the daemon cannot observe it, and no verb is added or changed. The Tauri commands this PRD adds are desktop-crate↔webview IPC, which is not the daemon wire.

The converse is the boundary rule doing its job, and it is worth writing down: **if a future settings item ever needs the daemon to know about it, that item is by definition daemon-owned and belongs on the other side of the line** — at which point rule 12 applies to it, in its own PRD.

Patch bump.

### Testing: what rule 4 means here

CLAUDE.md rule 4's L1/L2 vocabulary is the Rust TUI's, so the mapping is stated rather than assumed — and the honest answer includes a gap.

- **The Rust half runs in the per-task gate and in a required check.** `desktop/src-tauri` is a workspace member (`Cargo.toml:2-8`) and `cargo test-fast` is `nextest run --workspace`, so its tests run there and are linted by rule 2's clippy command; the `build` job is one of the four required checks. This covers document round-trip, defaults, unknown-key tolerance, load-never-fails, atomic write, `0o600`, path resolution and the env override, the pinned-shape test, and the no-secrets guard. **This is the half that actually blocks a merge**, and it is where the properties that matter live.
- **The frontend half is vitest + jsdom + Testing Library** (`desktop/vite.config.ts:20-24`), currently 7 files and 46 cases. It covers the sheet opening from the rail button and the palette, closing on Escape and on backdrop click, the section registry rendering, an edit round-tripping through the bridge, and the reset action routing through the existing confirmation. Note the CI job that runs it, `desktop-web` (`.github/workflows/ci.yml:132-153`), is **advisory** — it is not one of the four required checks, so a red vitest run does not block a merge.
- **Rule 4's PTY/L2/real-agent bar cannot be met here, and this PRD does not pretend otherwise.** That bar is a TUI-harness concept — vt100 driving a spawned binary, recording a `.cast` — and it cannot drive a WebKitGTK webview. There is no driver-level tier in this repo at all: `playwright`, `tauri-driver`, `webdriver` and `selenium` return **zero** matches workspace-wide. Building one is a repo-wide investment that #745 also declined, and folding it into a container PRD would be the scope creep this document exists to resist. Recorded as a named gap with a follow-up issue to file, and the compensating control is the manual smoke check in `docs/develop/desktop-gui.md:146+`, extended to cover opening Settings, changing a value, restarting the app and confirming it survived.

### Sequencing with PR #779

[PR #779](https://github.com/vfarcic/dot-agent-deck/pull/779) (PRD #745, agent overview) is **open as a draft**, +7725/−153 across 32 files, and it restructures the top of `desktop/src/App.tsx`: a new `DeckShell` owning `view: DeckView` (`{kind:"deck"} | {kind:"overview"}`), an `onNavigate` prop threaded into `ControlDeck`, and an **Overview** rail button inserted one line above the Settings stub.

Choosing the sheet pattern keeps the collision **low and textual**, and there are exactly two touchpoints worth planning around:

- **The rail `<nav>` block.** #779 inserts a line at `App.tsx:302`; this PRD changes the line at `App.tsx:305`. Same hunk, adjacent lines — git will report a conflict if both land as written, and the resolution is to keep both lines.
- **`App.test.tsx`'s shared `runtime()` helper.** #779 adds `setShownTerminals` to it. This is *not* a merge conflict; it is a type error that appears only once both sides are on one branch. Whoever lands second fixes it, and it is worth expecting rather than debugging.

Two things #779 adds should be built **on** rather than duplicated once it lands: `desktop/src/components/ConfirmDialog.tsx` (which extracts `ConfirmDialog` and `ConfirmState`, currently un-exported functions inside `App.tsx`), and `desktop/src/lib/displayText.ts` — whose `displayPath`, `homeRelative` and `shortDaemonLabel` are exactly what a settings surface and #741's endpoint list want. Until then this PRD **does not extract `ConfirmDialog` itself**: its one destructive action routes through `ControlDeck`'s existing `confirm` state slot (`desktop/src/App.tsx:83`) via a prop, which duplicates nothing and cannot conflict with #779's extraction.

## Success Criteria

- A future feature can answer "does my setting belong in the desktop app or behind the daemon?" from the written rule alone, without re-deriving the reasoning — and #741, #743 and #802 can each start their settings work without waiting on another design conversation.
- A preference set in the app survives a full restart of the app, and its value is in a file a user can read, edit and delete without the app running.
- Settings opens from the rail button that currently only shows a toast, and closes the same three ways every other sheet in the app closes.
- Adding a new setting is: one field on a section struct, one edit to that feature's own panel. Adding a new section is: one registry row and one component. Neither requires touching the store, the sheet, or anything belonging to another feature.
- A secret cannot be written to the settings document by accident — an automated guard fails the build rather than a reviewer catching it.
- Deleting the settings file, corrupting it, or making it unreadable each start the app cleanly on defaults; none of them is a crash or an error dialog.
- The daemon is unaware that any of this exists: `PROTOCOL_VERSION` is unchanged and no daemon verb is added.
- The container is proven by at least one real consumer end to end, not only by unit tests (subject to Open Question 2).

## Milestones

- [ ] **M1 — The ownership boundary, written down and applicable.** The client-owned/daemon-owned lists and the criterion, recorded here and in `docs/develop/desktop-gui.md`. Doc-only, and deliberately first: it is what unblocks the three dependents, and it is useful before any code lands.
- [ ] **M2 — The settings document.** `desktop/src-tauri/src/settings.rs`: typed sections, serde defaults, unknown-key tolerance, path resolution with the `DOT_AGENT_DECK_DESKTOP_CONFIG` override, load-never-fails, atomic `0o600` write, and the two Tauri commands. Rust tests including the pinned-shape test.
- [ ] **M3 — The Settings surface.** The rail stub becomes a real `config-sheet`; the section registry; Escape, palette and backdrop wiring; frontend read/write through the bridge; a General section showing where settings are stored and offering a reset routed through the existing confirmation. Vitest coverage for each.
- [ ] **M4 — One real preference, end to end.** The proving consumer, so the contract is exercised by something other than a test (see Open Question 2).
- [ ] **M5 — The secret seam, named and pinned.** The `SecretStore` trait and the "never in the document, never in localStorage" rule, with the guard test. No backend implementation — #802 picks one.
- [ ] **M6 — The registration contract documented.** `docs/develop/desktop-gui.md` gains an "Adding a setting" section: the three steps, the ownership criterion, the secret rule, and the manual smoke extension. Changelog fragment.

## Risks

- **Scope creep into the dependents' settings is the main risk, and it is not hypothetical** — every one of the three has a setting that would be easy and satisfying to implement here. Mitigation: the Out of Scope list names them individually, and the review question for any diff on this branch is "which of #741, #743 or #802 does this belong to?"
- **A container proven only against a trivial consumer.** The first real consumer finds the gaps, and by then three PRDs are building against it. Mitigation is M4 plus writing the registration contract as a document a dependent actually follows rather than as prose in this PRD.
- **Two persistence mechanisms coexist.** The four `localStorage` keys stay where they are, so "where is this stored?" has two answers for a while. Accepted deliberately — the alternative is migrating project-draft content into a per-installation store, which puts it further from where #819 will need it — but it must be documented, or the next contributor will add a fifth `localStorage` key by pattern-matching.
- **The user-visible half of a user-visible feature is covered only to jsdom depth, by an advisory CI job.** The Rust half is in a required check and holds the properties that matter, but nothing drives the real window. This is a standing gap for the whole desktop app, not one this PRD creates — and it is the reason a manual smoke step is part of M6 rather than optional.
- **The macOS path convention.** Shipping `~/.config` in a bundled `.app` is defensible for a developer tool and awkward for a Mac user. Changing it after the first release is a migration; changing it now is a constant.

## Open Questions

1. **Does this ship behind the `experimental` flag (CLAUDE.md rule 9)?** The recommendation is **no**, and it is not a fresh judgement: `prds/176-desktop-gui.md` decision 6 already recorded that the flag does not apply to this binary — "a separate GUI binary has no such seam — the act of building/running it is the opt-in", with maturity handled by packaging. The mechanics confirm it and go further than #176 did: the flag does not reach the desktop app by **any** route. Nothing under `desktop/` mentions it, it is not on the daemon protocol, and the desktop crate never calls `features::init_and_watch` — whose only callers are `src/main.rs:1509` and `:2075` — so `experimental_enabled()` would read the `false` default forever regardless of TOML or env. Gating this surface would mean **building the flag's Tauri delivery mechanism as part of this PRD**, which is new work in service of a switch, and it would raise "against which project directory?" — a question the desktop app has no good answer to for a packaged build.
2. **Does M4 ship, or does the container ship with no preference of its own?** The recommendation is that it ships, with **"restore the window's size and position on launch"** as the proving preference: it is genuinely container-owned (#741's principle names window size as client-owned presentational state), it belongs to none of the three dependents, and it exercises the whole path — a default, a toggle written from the UI, state written from Rust, and a read at startup before the webview exists. The disciplined alternative is a container with zero settings whose round-trip is proven only by tests, which is more scope-pure and leaves the first real consumer to discover the gaps.
3. **macOS: `~/.config/dot-agent-deck` or `~/Library/Application Support/dot-agent-deck`?** Leaning `~/.config` for consistency with the TUI on the same machine, accepting that it is not the platform convention. `dirs` is already a root-crate dependency if a macOS-specific root is ever wanted.
4. **Does the document carry an explicit schema version field now, or rely on serde defaults alone?** Leaning yes — a `version` integer is cheap insurance and impossible to add retroactively without a heuristic.
5. **Should the four existing `localStorage` keys eventually move, and to where?** Not in this PRD. They are project-draft content and #819's daemon-side project resolution is the plausible home; worth a follow-up issue so the question is tracked rather than rediscovered.

## Work Log

### 2026-09-02 — Created

Written from issue #803's placeholder, the two governing comments on #741, and a measured reconnaissance of the desktop app on `main` at `46ac28c`. Four premises carried into the planning turned out to be false and are recorded here because each changed a decision:

- **`ConfigurationPanels.tsx` does not edit `.dot-agent-deck.toml`.** It writes `localStorage` exclusively; the TOML is read-only to the desktop app and only on the workflow-launch path (`desktop/src-tauri/src/lib.rs:119-131`). The panels say so themselves — `ConfigurationPanels.tsx:124`: *"prompts are stored on this Mac only. Nothing is written to the project's `.dot-agent-deck.toml`."* The app-settings-versus-deck-content distinction this PRD was asked to draw is real, but the existing panels sit on the **settings** side of it, not the project side. That inverted the framing: the risk is not that a settings surface will absorb project config, it is that the existing "configuration" panels already store project-shaped content per-installation, which is a boundary violation this PRD names and deliberately does not fix.
- **There is no screen or navigation model to plug into.** One always-mounted screen, six rail buttons of which five toggle overlay booleans and one is a dead stub. PR #779 introduces the first union. That is what makes the sheet-versus-screen decision consequential, and it is why the sheet wins: it makes this PRD independent of a draft PR's landing.
- **`cargo test-fast` passes in this worktree** — 3649/3649, exit 0, including the desktop crate's 35 tests, verified selected rather than silently skipped. Issue #815's mechanism is measurably absent here: `PKG_CONFIG_PATH` is set under devbox, `pkg-config --variable=libdir gtk+-3.0` resolves into the nix store, and the built test binary's RUNPATH carries that path where #815 records none. The claim is "not reproducing here", not "fixed everywhere" — no from-scratch relink was forced. The practical consequence is that the local gate is available to lean on.
- **The experimental flag does not reach Tauri by any route**, which turns rule 9 from a policy question into a build-it-first question. See Open Question 1.
