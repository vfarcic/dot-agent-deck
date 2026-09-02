# PRD #803: A settings surface for the desktop app

**Status**: In progress — implemented alongside [PRD #743](743-desktop-light-dark-appearance.md) in one PR
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

This PRD defines **where settings live, how they persist, and how a feature registers one — and stops there.** It ships no opinion about what a daemon endpoint or a voice backend should be; those belong to #741 and #802.

**One exception, decided with the user on 2026-09-02: [PRD #743](743-desktop-light-dark-appearance.md)'s appearance override ships in the same pull request, and is the settings page's only section for now.** The two are mutually blocking in practice — #743 needs somewhere for an override to live, and a container with no tenant is proven only by its own tests, leaving the first real consumer to discover the gaps. The boundary between them stays sharp even though the PR is shared: **#803 owns the store, the sheet, the section registry and the contract; #743 owns the Appearance panel and everything the choice actually does.** If that line blurs during implementation, this PRD has failed at the thing it exists to do.

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
- **The `[appearance]` section as the container's first real tenant**, implemented per PRD #743 and shipped in the same PR. This PRD provides the section slot, the persistence and the panel's place in the registry; #743 provides what goes in it.

### Out of Scope

- **The settings the other two dependents need.** Daemon endpoints are #741's; the voice backends, the API key and the model management are #802's. This PRD gives each a place to land and a contract to follow; it does not define, design, or pre-create their sections.
- **Anything about how appearance works.** The palette, the dark values, the media query, the override attribute and the terminals' treatment are all #743's, documented in its own PRD. #803's involvement ends at "there is a section, and its value persists".
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

Unlike `DashboardConfig::save()`, the writer must not be able to drop a table it does not know about. **The serde defaults do not give this, and the first draft of this PRD wrongly claimed they did** — they cover *reading*, so an older build loads a newer build's `[voice]` section without error, but `#[serde(default)]` without `deny_unknown_fields` means *ignore*, not *retain*. The unknown table is dropped at load, and the next save serialises the struct over it. That is exactly the `DashboardConfig::save()` failure mode this PRD rejects `config.toml` for, two paragraphs earlier, reproduced in our own file. So the save path **merges the serialised struct into the parsed document** rather than replacing it, and a round-trip test pins an unknown section surviving a load-modify-save. **That survival is of data, not of the document**: the merge round-trips through `toml::Table`, which models data, so an unknown section comes back byte for byte only where its content was already in the serializer's canonical form. Comments, inline-array and inline-table formatting, key order and blank-line grouping are lost to the canonical re-render — tracked as [#825](https://github.com/vfarcic/dot-agent-deck/issues/825), whose fix is `toml_edit` and a new dependency deliberately not taken here. Found during implementation and closed before it shipped; the reasoning is in the Work Log.

A **pinned-shape test** in the idiom of `agent_mapping_is_frontend_stable` (`desktop/src-tauri/src/dto.rs:536-545`) asserts the exact serialised document, so adding a field is a deliberate act that shows up in a diff and forces the ownership question to be answered in review.

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
- **The frontend half is vitest + jsdom + Testing Library** (`desktop/vite.config.ts:20-24`), which stood at 7 files and 46 cases when this PRD was written and at **8 files and 60 cases** when it shipped. It covers the sheet opening from the rail button and the command palette, closing on Escape and on backdrop click, the section registry rendering and switching, the collapse to a single full-width panel below two sections, each appearance choice setting or clearing `data-theme` — including that `System` removes the attribute rather than writing a value — and the choice round-tripping through the bridge. Note the CI job that runs it, `desktop-web` (`.github/workflows/ci.yml:132-153`), is **advisory** — it is not one of the four required checks, so a red vitest run does not block a merge.
- **Rule 4's PTY/L2/real-agent bar cannot be met here, and this PRD does not pretend otherwise.** That bar is a TUI-harness concept — vt100 driving a spawned binary, recording a `.cast` — and it cannot drive a WebKitGTK webview. There is no driver-level tier in this repo at all: `playwright`, `tauri-driver`, `webdriver` and `selenium` return **zero** matches workspace-wide. Building one is a repo-wide investment that #745 also declined, and folding it into a container PRD would be the scope creep this document exists to resist. Recorded as a named gap with a follow-up issue to file, and the compensating control is the manual smoke check in `docs/develop/desktop-gui.md`, which gained a second subsection covering both OS appearances, the override in both directions, and opening Settings, changing a value, restarting the app and confirming it survived. That subsection states plainly that it is the **only** check covering WebKitGTK and WKWebView, since everything automated here ran in Chromium.

### Sequencing with PR #779

[PR #779](https://github.com/vfarcic/dot-agent-deck/pull/779) (PRD #745, agent overview) is **open as a draft**, +7725/−153 across 32 files, and it restructures the top of `desktop/src/App.tsx`: a new `DeckShell` owning `view: DeckView` (`{kind:"deck"} | {kind:"overview"}`), an `onNavigate` prop threaded into `ControlDeck`, and an **Overview** rail button inserted one line above the Settings stub.

Choosing the sheet pattern keeps the collision **low and textual**, and there are exactly two touchpoints worth planning around:

- **The rail `<nav>` block.** #779 inserts a line at `App.tsx:302`; this PRD changes the line at `App.tsx:305`. Same hunk, adjacent lines — git will report a conflict if both land as written, and the resolution is to keep both lines.
- **`App.test.tsx`'s shared `runtime()` helper.** #779 adds `setShownTerminals` to it. This is *not* a merge conflict; it is a type error that appears only once both sides are on one branch. Whoever lands second fixes it, and it is worth expecting rather than debugging.

Two things #779 adds should be built **on** rather than duplicated once it lands: `desktop/src/components/ConfirmDialog.tsx` (which extracts `ConfirmDialog` and `ConfirmState`, currently un-exported functions inside `App.tsx`), and `desktop/src/lib/displayText.ts` — whose `displayPath`, `homeRelative` and `shortDaemonLabel` are exactly what a settings surface and #741's endpoint list want. Until then this PRD **does not extract `ConfirmDialog` itself**, and as shipped it does not need it: **the settings surface has no destructive action at all.** Earlier drafts of this PRD assumed a "Reset desktop settings" control and planned to route it through `ControlDeck`'s existing `confirm` state slot; that control belonged to a General section which was dropped when #743's Appearance became the page's only section, and the plan for it survived in the prose after the thing it described had gone. Nothing in `SettingsSheet.tsx` or `AppearancePanel.tsx` has a reset, a destructive action, or a `confirm` prop, and no milestone asks for one. Recorded rather than quietly deleted because a reset is a reasonable thing to want back once the page has more than one section, and whoever adds it should know the confirmation slot is the intended route and that it must not duplicate #779's extraction.

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

- [x] **M1 — The ownership boundary, written down and applicable.** The client-owned/daemon-owned lists and the criterion, recorded here and in `docs/develop/desktop-gui.md`. Doc-only, and deliberately first: it is what unblocks the three dependents, and it is useful before any code lands.
- [x] **M2 — The settings document.** `desktop/src-tauri/src/settings.rs`: typed sections, serde defaults, unknown-key tolerance, path resolution with the `DOT_AGENT_DECK_DESKTOP_CONFIG` override, load-never-fails, atomic `0o600` write, and the two Tauri commands. Rust tests including the pinned-shape test. — `ba00f12`, plus the unknown-section-preservation fix that followed.
- [x] **M3 — The Settings surface.** The rail stub becomes a real `config-sheet`; the section registry; Escape, palette and backdrop wiring; frontend read/write through the bridge; the settings file's path shown as a footer line so "where did that go?" is answerable without documentation — and, in fixture mode, a footer that says there is **no** file rather than printing one, since the browser preview keeps settings in `localStorage` and structurally cannot reach a filesystem. Vitest coverage for each. No General section — the page has exactly one section, and it is #743's.
- [x] **M4 — The first real tenant, end to end.** The Appearance section, per PRD #743, so the contract is exercised by something a user can actually change rather than only by a test. #743's own milestones carry what the choice does; #803's obligation here is that the section registers, renders and persists through the contract above with no special-casing.
- [x] **M5 — The secret seam, named and pinned.** The "never in the document, never in `localStorage`" rule, pinned by a guard test that walks the serialised document's key names — plus a second test proving the guard itself catches a credential-shaped key, so "no offenders" is a meaningful result rather than a vacuous one. The `SecretStore` seam is **named** — in `settings.rs`'s module docs, in the guard's failure message and in the developer doc — and deliberately **not built**: an unused trait is dead code, and #802 will design its shape against a real backend. This milestone's earlier wording promised the trait itself, which overstated what the Technical Approach above actually specifies ("named and not built"); corrected rather than satisfied by adding code nobody asked for.
- [x] **M6 — The registration contract documented.** `docs/develop/desktop-gui.md` gains an "Adding a setting" section: the three steps, the ownership criterion, the secret rule, and the manual smoke extension. Changelog fragment.

## Risks

- **Scope creep into the dependents' settings is the main risk, and it is not hypothetical** — every one of the three has a setting that would be easy and satisfying to implement here. Mitigation: the Out of Scope list names them individually, and the review question for any diff on this branch is "which of #741, #743 or #802 does this belong to?"
- **A container proven only against a trivial consumer.** The first real consumer finds the gaps, and by then three PRDs are building against it. Mitigation is M4 plus writing the registration contract as a document a dependent actually follows rather than as prose in this PRD.
- **Two persistence mechanisms coexist.** The four `localStorage` keys stay where they are, so "where is this stored?" has two answers for a while. Accepted deliberately — the alternative is migrating project-draft content into a per-installation store, which puts it further from where #819 will need it — but it must be documented, or the next contributor will add a fifth `localStorage` key by pattern-matching.
- **The user-visible half of a user-visible feature is covered only to jsdom depth, by an advisory CI job.** The Rust half is in a required check and holds the properties that matter, but nothing drives the real window. This is a standing gap for the whole desktop app, not one this PRD creates — and it is the reason a manual smoke step is part of M6 rather than optional.
- **The macOS path convention.** Shipping `~/.config` in a bundled `.app` is defensible for a developer tool and awkward for a Mac user. Changing it after the first release is a migration; changing it now is a constant.

## Open Questions

**Questions 1 and 2 were decided by the user on 2026-09-02; 3 to 5 were delegated to implementation judgement in the same conversation, with the standing instruction that a call made and recorded beats a call deferred. Each remains cheap to reverse.**

1. **Does this ship behind the `experimental` flag (CLAUDE.md rule 9)? — DECIDED: no.** The recommendation was no and the user confirmed it. The reasoning is not a fresh judgement: `prds/176-desktop-gui.md` decision 6 already recorded that the flag does not apply to this binary — "a separate GUI binary has no such seam — the act of building/running it is the opt-in", with maturity handled by packaging. The mechanics confirm it and go further than #176 did: the flag does not reach the desktop app by **any** route. Nothing under `desktop/` mentions it, it is not on the daemon protocol, and the desktop crate never calls `features::init_and_watch` — whose only callers are `src/main.rs:1509` and `:2075` — so `experimental_enabled()` would read the `false` default forever regardless of TOML or env. Gating this surface would mean **building the flag's Tauri delivery mechanism as part of this PRD**, which is new work in service of a switch, and it would raise "against which project directory?" — a question the desktop app has no good answer to for a packaged build.
2. **Does M4 ship, or does the container ship with no preference of its own? — DECIDED: it ships, and the tenant is #743's appearance override rather than the window-restore preference originally proposed.** The window-restore toggle was a stand-in chosen because it belonged to no dependent; the user's alternative is better on every axis — it is a setting people actually want, it exercises the same path, and it means the settings page opens with a reason to exist. Window restore is dropped, not deferred: it was never wanted for itself.
3. **macOS: `~/.config/dot-agent-deck` or `~/Library/Application Support/dot-agent-deck`? — DECIDED: `~/.config`.** Consistency with the TUI on the same machine wins: a user running both front-ends finds both configs in one directory, and the divergence from the platform convention is one the TUI already made deliberately. `dirs` is already a root-crate dependency if a macOS-specific root is wanted later; moving then is a one-time migration, which is the cost this defers rather than avoids.
4. **Does the document carry an explicit schema version field now, or rely on serde defaults alone? — DECIDED: yes, a `version` integer.** Cheap insurance, and impossible to add retroactively without a heuristic for "documents written before the field existed".
5. **Should the four existing `localStorage` keys eventually move, and to where? — DECIDED: not here, and tracked rather than left implicit.** They are project-draft content and #819's daemon-side project resolution is the plausible home. A follow-up issue is filed so the question is not rediscovered by whoever next wonders why the app has two persistence mechanisms.

## Work Log

### 2026-09-02 — Review and audit resolved; three residuals recorded rather than fixed

Reviewer verdict **ship it**, no blocking bug. Auditor found no critical or high, and recommended holding the persistence seam until its findings 1 and 2 were addressed; both are now addressed. Seven commits, `d08ee61`..`e70ac93`. Tests: **3704** Rust (from 3690) and **62** vitest across 9 files (from 60).

**The container's central promise is now literally true.** The reviewer caught that `settingsContract.ts` and this PRD's Success Criteria both claimed adding a section touches neither the store nor *the sheet*, while `SETTINGS_SECTIONS` lived inside `SettingsSheet.tsx` — so every dependent would edit a #803-owned component to register, and #741 and #802 would conflict on adjacent lines of one array. The registry moved to `lib/settingsRegistry.ts`, with no re-export from the sheet, which would have kept the coupling the move exists to remove. This was the right finding to take over the cheaper option of softening the sentence: the claim is the whole reason this PRD exists.

**Two guards were failing in ways only a guard can fail — silently.** `check_dark_palette` located `:root:not([data-theme="light"])` by selector text anywhere in the file, so dedenting that block out of its `@media` wrapper would apply dark values in **light** mode with nothing noticing; containment is now proved structurally, and the implementation added the mirror assertion that `:root[data-theme="dark"]` is *not* nested, since that failure silently disables an explicit Dark choice on a light OS. The colour guard **failed open** on every I/O error — `read_dir`, entry and file-read failures were silently skipped — so a required CI check could go green having inspected nothing. It now returns `Result` and fails closed, refuses symlinks rather than skipping them, reads the `theme-invariant:` marker from comment text only (a string containing that text on a `#hex` line previously exempted the line), requires a non-empty reason, and scopes the `:root` exemption to the exact root stylesheet.

**The credential control is now labelled honestly, which matters more than tightening it.** The guard inspects key *names* only, so a field called `endpoint`, `bearer` or `authorization` holding a real token passes clean, a default-omitted field is invisible to it, and a frontend-only `apiKey` in the TypeScript DTO is outside it entirely. Its name, doc comment, failure message, `settingsContract.ts` and the developer doc now all call it a **naming tripwire, not a security boundary**, and a test pins what it is blind to so the limitation is a property rather than a discovery. The allowlist is keyed by full path and is **empty** — nothing in the schema needs an exception. The auditor's real ask, end-to-end sentinel tests through a live secret-store flow, cannot be written before that flow exists and is [#827](https://github.com/vfarcic/dot-agent-deck/issues/827), titled to be unmissable from #802. **The danger this closes is a future author reading "there is a secrets guard" and concluding the problem is solved.**

**A path that could hang or OOM the app is now vetted.** `DOT_AGENT_DECK_DESKTOP_CONFIG` accepted any non-empty path with an unbounded `read_to_string` behind it, so a FIFO blocked the command forever, `/dev/zero` exhausted memory, and a final-component symlink was followed. It now requires an absolute path naming a regular file, `symlink_metadata`s without following the final component, rejects each non-regular kind by name, and reads at most **256 KiB** — checked twice, against the reported size and against the bytes actually read, so a file swapped between the two is still refused. Today's document is 40 bytes and #741's and #802's sections are kilobytes at the outside, so the limit is four orders of magnitude of headroom.

**Temp-file hardening was taken in part, and the declined half is documented on the write path** so the next auditor finds the reasoning instead of re-deriving it. Unpredictable temp names and Windows `share_mode(0)` landed. The `openat`/`renameat` directory-handle anchoring and parent-ownership validation did not: the destination is a per-user config directory, and anyone who can win the temp-swap race in it can already write `desktop.toml` directly, so the race buys an attacker nothing. **That calculus changes the moment #741 puts endpoints or #802 puts a secret reference in this document**, and the comment says so.

Three residuals were surfaced by the implementation and are recorded here rather than fixed, each with the reasoning so they can be overruled cheaply:

1. **The publish-failure cleanup is now uncovered.** Path vetting catches the directory-at-destination case before anything is created, which was the only constructible way to reach the `remove_file(&tmp)` branch through the public API. The cleanup stays as defence in depth against a genuine rename failure — a full disk, an I/O error — and a fault-injection seam to cover an otherwise unreachable branch is more machinery than the property is worth.
2. **An over-length appearance token now fails the whole document to defaults** rather than falling back at the field level like any other unknown value, because the bound lives on the type and therefore covers the IPC payload and a hand-edited file with one check. That is a knowing deviation from this PRD's field-level tolerance, defensible on the grounds that a 4 KB mode string is a malformed document rather than a mode this build has not heard of, and reversible in two lines at the cost of two code paths that can drift. A token exactly at the 64-byte limit still takes the field-level path.
3. **The settings footer can name a path the app refuses to use.** With a deliberately misconfigured override the sheet's "Stored in `<path>`" line is now false where it was previously true. Fixing it means changing `DesktopSettingsSnapshot`, whose serialised shape is pinned and is part of the #741/#802 contract — [#829](https://github.com/vfarcic/dot-agent-deck/issues/829) records it, and notes that **the cheapest moment to fix it is before those two PRDs consume the DTO**.

[#828](https://github.com/vfarcic/dot-agent-deck/issues/828) carries the interprocess save lock, which becomes load-bearing when #741 lands endpoints; the in-process half — saves serialised on a promise chain so a superseded reply cannot win — landed here, with the desktop app's first hook test driving it against hand-settled promises.

### 2026-09-02 — M1, M3, M4, M5 and M6 landed; the container is complete and has one tenant

`ba451b3` (sheet, registry, appearance override), `2223c52` (registry collapse), `9afc895` (docs and changelog).

**The contract is real and enforced by the file layout rather than only described.** #803 owns `lib/settingsContract.ts`, `useDesktopSettings.ts` and `SettingsSheet.tsx` — none of which knows what a theme is. #743 owns `lib/appearance.ts` and `AppearancePanel.tsx` — the only things that know what "dark" means. The hook holds the document, the path and the last write error, and nothing else, so #741 and #802 add sections without it changing at all. That separation is the strongest evidence that the shared PR did not blur the boundary the Solution Overview warned about.

**One design call was made with the user offline.** A 232px section column holding one row, with ~700px of empty space beneath it, read as unfinished work rather than as a deliberate scope boundary. Below two sections the column is dropped and the single panel renders full-width; the registry still drives it, so the column returns on its own the moment a dependent adds a row. The alternative — a footnote naming what will land there — was rejected because it bakes an opinion about the container's future contents into the container, which is the exact thing this PRD exists to avoid. **The collapse is proved by a test, not asserted in a comment**: `SettingsSheet` takes an optional `sections` prop the app never passes, so the two-section layout is reachable from a test today rather than only after #741 lands.

**`desktop_get_settings` returns the path as a separate struct** (`DesktopSettingsSnapshot { settings, path }`) rather than as a field on the document — the path is *where* the document is, not part of it, and a field would have written it into the TOML and into the pinned shape. In fixture mode the footer says the browser preview keeps settings in local storage and names no file, because `FixtureDeckBridge` structurally cannot reach one.

**Two defects were found while wiring the panel.** A **load/edit race**: the initial settings load is async, so a choice made before it resolved was silently overwritten by what was on disk at startup — the change appears to take, then reverts a moment later, which is precisely how a bug gets reported as "the setting doesn't stick". Guarded with an `edited` ref. And a deliberate non-fix: **a failed save is not reverted**, because the user asked for the change and can see it applied; what failed is making it survive a restart, and the panel says so rather than silently undoing a choice just made.

**Six follow-up issues were filed** rather than left as prose promises: [#821](https://github.com/vfarcic/dot-agent-deck/issues/821) and [#822](https://github.com/vfarcic/dot-agent-deck/issues/822) (light-mode contrast), [#823](https://github.com/vfarcic/dot-agent-deck/issues/823) (no driver-level test tier — the gap this PRD's Testing section names), [#824](https://github.com/vfarcic/dot-agent-deck/issues/824) (the four `localStorage` keys, Open Question 5), [#825](https://github.com/vfarcic/dot-agent-deck/issues/825) (`desktop.toml` loses comments on save) and [#826](https://github.com/vfarcic/dot-agent-deck/issues/826) (a latent `App.tsx` key-handler bug, filed not fixed).

**Three claims in this document were wrong and are corrected above rather than quietly satisfied.** M5 promised a `SecretStore` *trait*; what the Technical Approach actually specifies is a seam "named and not built", and adding an unused trait to close the gap would have been dead code — the milestone's wording was the error. The Testing section and the PR-#779 paragraph both described a **reset action that does not exist**: it belonged to a General section dropped when Appearance became the page's only tenant, and the plan for it outlived the thing it described. And the frontend test counts were stale twice over. All three were surfaced by the implementation refusing to paper over them, which is the behaviour that makes a plan worth writing down.

### 2026-09-02 — M2 landed, and implementation found a real defect in this PRD

`ba00f12`. The document on disk is four lines:

```toml
version = 1

[appearance]
mode = "system"
```

at `config_dir()/desktop.toml`, with `DOT_AGENT_DECK_DESKTOP_CONFIG` overriding the whole path. Sixteen Rust tests, running in `cargo test-fast` and therefore in the required `build` check.

**The defect: this PRD claimed "across builds the serde defaults cover it" about the writer dropping unknown tables, and that was wrong.** Serde defaults cover reading, not writing — no `deny_unknown_fields` means *ignore*, not *retain*, so an unknown `[voice]` section is discarded at load and deleted by the next save. That is the identical failure this PRD cites, two paragraphs earlier, as the reason not to share the TUI's `config.toml`. It is being closed rather than accepted: the save path merges into the parsed document instead of replacing it. The exposure was genuinely small today — one app owns the file, and reaching the bug needs a user alternating two desktop builds against one config directory — but it becomes likely the moment #802 lands a section, and a container whose central promise is "add a section and trust it to survive" cannot ship with it. The implementation deliberately built it as specified and reported it rather than silently working around it, which is why it was visible at all.

Three smaller corrections. **Standalone Tauri commands rather than `DesktopAction` variants**, and the reasoning is better than the PRD's silence on it: every `DesktopAction` arm falls through to `refresh_and_emit`, a `ListAgents` round-trip over the daemon socket, so routing a settings read through it would make reading a local TOML file cost a daemon RPC — at launch, before the daemon is necessarily up. Settings should not be able to fail because the daemon is down, which is the ownership boundary asserting itself in the IPC design. **No capability change was needed** — app commands from `generate_handler!` are not ACL-gated in Tauri v2, only `core:` and plugin commands are, so the crate still has zero plugins and `["core:default"]`. And a **line reference in this PRD had drifted**: `agent_mapping_is_frontend_stable` is at `dto.rs:536-545`, not `:639-647`; the idiom cited was right, the coordinates were stale. Corrected above.

One cross-PRD interaction worth recording, because it will recur: **#743's colour guard fires on `#803` inside a string literal**, since `803` is three valid hex digits. The guard masks comments but deliberately not string contents, because `"#141817"` is exactly what it hunts. That is correct behaviour, not a guard bug, and the fix is to rename the string. It will keep happening as more strings are written under `desktop/src`, and the failure reads as a colour problem rather than a naming one.

### 2026-09-02 — Scope settled with the user: #743 ships in the same PR

Four decisions, taken at the plan gate.

**No experimental flag**, confirming Open Question 1's recommendation and `prds/176-desktop-gui.md` decision 6.

**PRD #743 is merged into this PR and becomes the settings page's only section.** The user's reasoning, and it is better than the alternatives offered: the container needs a first tenant or it is proven only by its own tests, and #743 needs somewhere to put an override — so running them separately means two reviews to arrive at one working page. The proposed window-restore preference is dropped; it existed only to be a tenant, and a real one is available.

**The terminals stay dark and unchanged in both themes.** Raised by the user directly, after being shown that we control the xterm background but not what an agent emits into the pane — dim greys tuned for black, and truecolor SGR that bypasses the 16-slot palette entirely. This removes the whole xterm-theming problem from the combined scope. Recorded in #743's Open Questions 1 and 2.

**The hex-literal cleanup is in scope**, and re-measuring it changed the plan. The reconnaissance had quoted four literals as examples and this PRD repeated them as if they were the set; the actual count is **150 occurrences across ~105 distinct values** in `styles.css`, plus 27 in `.ts`/`.tsx`. That moves the tokenising pass from a tidy-up to the largest single piece of the combined work, and it is why #743's M1 isolates it in one commit with a "light mode is visually unchanged" property — 105 colour decisions cannot be verified by reading a diff, but they can be verified by opening the app.

**PRD #745's overview screen is deliberately excluded** until [PR #779](https://github.com/vfarcic/dot-agent-deck/pull/779) merges to `main`, at which point this branch merges `main` and themes that screen. It is not on this branch and cannot be themed from here.

The user went offline after this conversation with the standing instruction to make remaining calls rather than defer them, and to correct anything wrong the next day. Open Questions 3 to 5 were decided on that basis and each records its reasoning.

### 2026-09-02 — Created

Written from issue #803's placeholder, the two governing comments on #741, and a measured reconnaissance of the desktop app on `main` at `46ac28c`. Four premises carried into the planning turned out to be false and are recorded here because each changed a decision:

- **`ConfigurationPanels.tsx` does not edit `.dot-agent-deck.toml`.** It writes `localStorage` exclusively; the TOML is read-only to the desktop app and only on the workflow-launch path (`desktop/src-tauri/src/lib.rs:119-131`). The panels say so themselves — `ConfigurationPanels.tsx:124`: *"prompts are stored on this Mac only. Nothing is written to the project's `.dot-agent-deck.toml`."* The app-settings-versus-deck-content distinction this PRD was asked to draw is real, but the existing panels sit on the **settings** side of it, not the project side. That inverted the framing: the risk is not that a settings surface will absorb project config, it is that the existing "configuration" panels already store project-shaped content per-installation, which is a boundary violation this PRD names and deliberately does not fix.
- **There is no screen or navigation model to plug into.** One always-mounted screen, six rail buttons of which five toggle overlay booleans and one is a dead stub. PR #779 introduces the first union. That is what makes the sheet-versus-screen decision consequential, and it is why the sheet wins: it makes this PRD independent of a draft PR's landing.
- **`cargo test-fast` passes in this worktree** — 3649/3649, exit 0, including the desktop crate's 35 tests, verified selected rather than silently skipped. Issue #815's mechanism is measurably absent here: `PKG_CONFIG_PATH` is set under devbox, `pkg-config --variable=libdir gtk+-3.0` resolves into the nix store, and the built test binary's RUNPATH carries that path where #815 records none. The claim is "not reproducing here", not "fixed everywhere" — no from-scratch relink was forced. The practical consequence is that the local gate is available to lean on.
- **The experimental flag does not reach Tauri by any route**, which turns rule 9 from a policy question into a build-it-first question. See Open Question 1.
