//! PRD #20 W1 — install Codex's native hooks into the active `CODEX_HOME`.
//!
//! Codex 0.144.4 ships a Claude-Code-compatible hooks engine, so its command
//! hooks POST the same stdin JSON shape Claude does and are ingested by the
//! existing [`crate::hook::handle_hook`] `"codex"` arm. This module writes the
//! hook DEFINITIONS — a `hooks.json` whose every command shells
//! `dot-agent-deck hook --agent codex` — into the Codex home the spawned `codex`
//! child reads, so a live interactive session's prompt / tool / turn events ride
//! the deck's existing raw-`AgentEvent` hook socket (no new wire, no
//! `PROTOCOL_VERSION` bump — rule 12).
//!
//! It is the Codex analog of [`crate::hooks_manage::auto_install`] (Claude):
//! **guarded, silent, and SAFE for the user's real `~/.codex`**. Deliberate
//! choices vs. Claude:
//!
//! - We write a SEPARATE `hooks.json` — Codex's highest-precedence,
//!   auto-discovered hook source (`$CODEX_HOME/hooks.json`) — rather than
//!   editing `config.toml`, so the user's real `~/.codex/config.toml` (auth
//!   references, model, project trust, skills, history) is never touched.
//! - We MERGE, never clobber: pre-existing user hooks are preserved; only prior
//!   deck-authored entries (identified by the EXACT command signature
//!   [`HOOK_COMMAND_SUFFIX`], not a loose `dot-agent-deck` substring) are
//!   refreshed, so re-installs never accumulate duplicates and a user hook that
//!   merely mentions `dot-agent-deck` is never deleted (finding #14).
//! - The write is ATOMIC (temp file in the same dir + `rename(2)`) and guarded
//!   by an in-process mutex, so a crash mid-write can't truncate the file and two
//!   panes launching Codex concurrently can't clobber each other (finding #1/M-2).
//! - We treat ONLY `NotFound` as an empty config. Unreadable, malformed, or
//!   structurally-incompatible existing content is NEVER silently discarded:
//!   malformed JSON is backed up to `hooks.json.bak` and the install errors;
//!   a structurally-incompatible shape errors WITHOUT touching the file
//!   (findings #1, L-2).
//!
//! Trust (PRD #20 §4.1, Greptile P1): Codex requires non-managed command hooks
//! to be trusted before they run. The deck used to launch `codex` with
//! `--dangerously-bypass-hook-trust`, which is INVOCATION-GLOBAL — it trusts
//! every enabled hook in the active `CODEX_HOME`, including the user's own
//! untrusted third-party hooks — and, being argv, could be forwarded by any
//! launcher to a Codex reading hooks the deck never inspected. **That flag is
//! gone.** Codex 0.144.4 does expose a *scoped, per-hook* trust store, and the
//! deck now writes exactly that instead:
//!
//! - [`list_hooks_in`] asks Codex itself (`codex app-server` → `hooks/list`) for
//!   each hook's `key`, `currentHash`, `sourcePath`, `command`, and `isManaged`.
//! - [`deck_owned_entries`] keeps ONLY entries whose `sourcePath` is the pinned
//!   home's own `hooks.json`, which are not `isManaged`, and whose command
//!   satisfies the caller's [`DeckCommandMatch`]. **This is the security
//!   predicate — the only production path to a trust write**, and for a trust write the
//!   setting is [`DeckCommandMatch::Exact`]: byte equality against the command
//!   this run generated for the durable path it validated, not merely "carries
//!   the deck signature" (issue #730). The signature setting remains: **no write
//!   consults it except the revocation**, where a wider predicate can only
//!   remove privilege, and the trust write never does. There is also one
//!   read-only use — [`warn_if_our_own_entry_was_unrecognisable`], which reaches
//!   for it to tell a benign zero from a broken one and writes nothing.
//! - [`trust_deck_hooks_in`] records `[hooks.state."<key>"] { trusted_hash }` in
//!   `<home>/config.toml` for exactly those keys — `trusted_hash` and nothing
//!   else, because the sibling `enabled` key is a USER knob and not part of
//!   trust at all (see [`upsert_trust_record`]).
//!
//! The result is strictly narrower than the old bypass and launch-method
//! agnostic: trust lives in the home (not argv), so `codex`, `devbox run
//! codex-big`, and `./run_codex.sh` behave identically, a third-party hook in the
//! very same `hooks.json` stays untrusted, and any edit to a trusted definition
//! flips it to `modified` so Codex refuses it (fail-closed — events degrade to
//! the coarse stdout classifier, never silent over-trust). Residual: a launcher
//! that re-exports `CODEX_HOME` escapes the pin, but that re-homed Codex then has
//! neither our `hooks.json` nor our trust records — a functionality loss, not a
//! trust leak (see `docs/develop/agent-adapters.md`).
//!
//! The `config.toml` edit is FORMAT-PRESERVING (`toml_edit`): only the
//! `hooks.state."<key>"` tables are inserted/replaced, so the user's comments,
//! `model = …`, auth references, and their own trust records survive byte-intact.

use std::io::{self, BufRead as _, BufReader, ErrorKind, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// The fixed command signature that identifies a deck-authored Codex hook. Every
/// deck hook command is `<binary_path> hook --agent codex`, so a command ending
/// in this exact suffix is deck-owned. Matching the full verb (rather than the
/// `dot-agent-deck` substring) means a user hook that merely mentions
/// `dot-agent-deck` in an argument is never mistaken for a deck entry
/// (finding #14).
const HOOK_COMMAND_SUFFIX: &str = "hook --agent codex";

/// The interpreter every Codex hook command is quoted for. Unlike Devin's,
/// this one genuinely follows the host: Codex's hooks engine runs the command
/// through `%COMSPEC%`/`cmd.exe /C` on Windows and `$SHELL`/`/bin/sh -lc`
/// elsewhere, and `codex_home` honours `$CODEX_HOME` on every platform, so the
/// Windows arm is reachable. See `agent_hook_config::HookShell`.
const HOOK_SHELL: crate::agent_hook_config::HookShell = crate::agent_hook_config::HookShell::Native;

/// Serializes the read-modify-write of `hooks.json` across concurrent in-process
/// Codex spawns (two panes launching `codex` at once). Combined with the atomic
/// temp-file+rename publish, this closes the concurrent-clobber / partial-write
/// window on the user's real `~/.codex/hooks.json` (finding #1/M-2).
static INSTALL_LOCK: Mutex<()> = Mutex::new(());

/// Codex hook events we install a command handler for. Every name maps to an
/// [`crate::event::EventType`] via [`crate::hook`]'s `map_event_type`, and Codex
/// fires these at the engine level (shared by the interactive TUI and
/// `codex exec`). Covers the lifecycle (`SessionStart`/`Stop`), prompt
/// (`UserPromptSubmit`), tool (`Pre`/`PostToolUse`), permission, compaction, and
/// subagent boundaries — the same class Claude delivers.
const CODEX_HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PermissionRequest",
    "Stop",
    "PreCompact",
    "PostCompact",
    "SubagentStart",
    "SubagentStop",
];

/// Resolve the active Codex home the way Codex itself does: `$CODEX_HOME` when
/// set (and non-empty), else `$HOME/.codex`. Returns `None` when neither is
/// available so a guarded caller never falls back to a throwaway/`/tmp` home —
/// in production this is the user's REAL `~/.codex`, preserving auth/skills/
/// history (per the PRD design).
///
/// **Windows: deliberately a no-op, not an oversight (PRD #163 M1, reconfirmed in
/// review).** `$HOME` is normally unset on Windows, so this returns `None` and
/// every caller degrades to a documented skip. That is the correct outcome: the
/// path we would have to guess belongs to a *third-party* tool, and Codex — not
/// this project — decides where its home lives on Windows. Writing hooks into a
/// location Codex does not read would be worse than not installing them: it looks
/// like success and silently delivers nothing. Set `$CODEX_HOME` (which is
/// honoured on every platform, above) to install Codex hooks on Windows.
fn codex_home() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("CODEX_HOME")
        && !dir.is_empty()
    {
        return Some(PathBuf::from(dir));
    }
    let home = std::env::var("HOME").ok().filter(|h| !h.is_empty())?;
    Some(PathBuf::from(home).join(".codex"))
}

/// Whether a command string is a deck-authored hook, by EXACT signature: it is
/// `<executable> ` followed by [`HOOK_COMMAND_SUFFIX`] (`… hook --agent codex`).
/// A user command that merely contains `dot-agent-deck` (e.g.
/// `audit-wrapper --watch dot-agent-deck`) is NOT deck-owned and is preserved
/// (finding #14).
///
/// This is the WIDE, binary-agnostic sense of ownership: any deck install's
/// command, not just this one's. It is the right predicate for uninstall —
/// whose job is to remove the deck's rules wholesale, whichever install wrote
/// them — and the wrong one for deciding what a re-install may overwrite,
/// including under a retired event (see [`command_is_replaceable`], issue
/// #730).
fn command_is_deck_owned(command: &str) -> bool {
    crate::agent_hook_config::command_executable(command, HOOK_COMMAND_SUFFIX).is_some()
}

/// The executable a deck-owned command names, unquoted, or `None` when the
/// command is not deck-owned at all.
fn deck_command_executable(command: &str) -> Option<String> {
    crate::agent_hook_config::command_executable(command, HOOK_COMMAND_SUFFIX)
        .map(|exe| crate::agent_hook_config::unquote_if_needed(exe).into_owned())
}

/// Whether `command` is a deck-owned command belonging to the SPECIFIC binary
/// currently installing, so a re-install should replace it rather than add a
/// second rule beside it. Symlinks are resolved, so a `dot-agent-deck` symlink
/// pointing at a renamed build collapses to one rule.
fn command_is_this_binary(command: &str, binary_path: &str) -> bool {
    deck_command_executable(command)
        .is_some_and(|exe| crate::agent_hook_config::executables_match(&exe, binary_path))
}

/// Whether `command` is a deck-owned command whose pin is POSITIVELY not usable
/// and which shares the installing binary's own basename — the "repair only when
/// the target is not one the deck would write" gate, mirroring
/// `hooks_manage::command_is_dead_deck`.
///
/// [`crate::platform::paths::pin_is_repairable`] is what "not usable" means, and
/// it is not the same as "missing": a bare or relative pin and a
/// `target/{debug,release}` path are both unusable-and-replaceable while naming
/// a file that may exist and run (issue #536's read side). A stat error on a
/// well-formed absolute pin is the one case that keeps the benefit of the doubt.
///
/// Issue #730: before this, `install_impl` stripped **every** deck-owned rule by
/// suffix and re-added its own, so a deck-owned entry naming a different but
/// still-valid install was repointed on each launch. PRD #381's Open Question 3
/// answers that case explicitly — leave it alone; the trigger is never "the
/// target is not what I would have written" — and Claude and OpenCode already
/// behaved that way. (#381 spelt the positive half of that trigger "the target
/// is missing", which was accurate for the `try_exists`-only gate it was written
/// against and is narrower than [`crate::platform::paths::pin_is_repairable`]
/// asks today; the paragraph above is the current reading.) This is what makes
/// Codex match.
fn command_is_dead_deck(command: &str, binary_path: &str) -> bool {
    deck_command_executable(command)
        .is_some_and(|exe| crate::agent_hook_config::pin_is_dead_sibling(&exe, binary_path))
}

/// Whether a re-install by `binary_path` may REPLACE `command`: it is either
/// this binary's own prior deck command, or a deck command naming a pin the deck
/// cannot use — missing, bare or relative, non-executable, or a build-artifact
/// path — under this binary's basename. The union of
/// [`command_is_this_binary`] and [`command_is_dead_deck`], named once because
/// [`install_impl`] applies it in two places — the installed events and the
/// retired-event sweep — and the two must not drift apart (issue #730).
///
/// "Cannot use" is [`crate::platform::paths::pin_is_repairable`]'s question and
/// it is wider than "the OS says the file is gone": of its four true-cases, two
/// can fire for a pin that names a file which exists and runs — a bare or
/// relative pin (#536's own shape, resolved through the *agent's* `$PATH`, or
/// against its cwd, at hook-fire time) and a `target/{debug,release}` path. That
/// is deliberate and is the whole point of #536's read side, so do not describe
/// this as pruning only what is positively gone; it prunes what the deck would
/// refuse to write.
fn command_is_replaceable(command: &str, binary_path: &str) -> bool {
    command_is_this_binary(command, binary_path) || command_is_dead_deck(command, binary_path)
}

/// Merge the deck's command hooks for `command` — the command built for
/// `binary_path` — into an existing `hooks.json` value (or `{}`), preserving any
/// user-authored hooks and refreshing (not duplicating) this binary's own prior
/// deck entries.
///
/// `binary_path` is passed alongside the already-built `command` because the two
/// answer different questions: `command` is what gets WRITTEN, `binary_path` is
/// what decides which existing deck commands may be overwritten.
fn install_impl(root: &mut Value, command: &str, binary_path: &str) {
    use crate::agent_hook_config::strip_deck_commands;

    if !root.is_object() {
        *root = json!({});
    }
    let obj = root.as_object_mut().expect("root is an object");
    if !obj.get("hooks").is_some_and(Value::is_object) {
        obj.insert("hooks".into(), json!({}));
    }
    let hooks = obj
        .get_mut("hooks")
        .and_then(Value::as_object_mut)
        .expect("hooks is an object");

    // Clear THIS install's leftovers under an event it no longer installs, so a
    // re-install after `CODEX_HOOK_EVENTS` shrinks orphans none of its own rules.
    // Same predicate as the installed-event sweep below, deliberately: a deck
    // command under a retired event is NOT dead merely because this deck stopped
    // installing that event. What `CODEX_HOOK_EVENTS` describes is what this
    // deck writes, not what Codex runs — measured on 0.149.0, which accepts a
    // `SessionEnd` command hook and enumerates it as `eventName: "sessionEnd"`,
    // `enabled: true`, with no warning, while `SessionEnd` is not in the list
    // above. So a wide sweep here can delete a live hook belonging to a user or
    // to a newer sibling install, which is issue #730's own defect one door
    // along. (`Notification` and `TurnStart` were dropped as unknown in the same
    // probe, so the class is real but not every name is in it.)
    //
    // The consequence, stated rather than left to be discovered: nothing cleans
    // a FOREIGN install's retired-event rule during install. That is the same
    // tradeoff already accepted for the installed events, and `uninstall_from`
    // still clears every deck-signature command wide.
    //
    // An event key left empty is NOT dropped by this INSTALL sweep, while
    // Devin's install sweep does drop it. (Both adapters' `uninstall` drop
    // emptied keys; the asymmetry is install-side only.) It is pre-existing on
    // both sides and left deliberately: each adapter keeps the shape its own
    // users' files already have, and nothing here is a reason to rewrite more of
    // a third-party file than the deck put there. Do not "fix" one into the
    // other without deciding which is right.
    let keys: Vec<String> = hooks.keys().cloned().collect();
    for key in keys {
        if CODEX_HOOK_EVENTS.contains(&key.as_str()) {
            continue;
        }
        if let Some(arr) = hooks.get_mut(&key).and_then(Value::as_array_mut) {
            strip_deck_commands(arr, |cmd| command_is_replaceable(cmd, binary_path));
        }
    }

    let entry = json!({
        "hooks": [ { "type": "command", "command": command } ]
    });
    for &event in CODEX_HOOK_EVENTS {
        let arr = hooks.entry(event.to_string()).or_insert_with(|| json!([]));
        if !arr.is_array() {
            *arr = json!([]);
        }
        let arr = arr.as_array_mut().expect("hook event value is an array");
        // Normalize down to a single fresh rule, but only for THIS binary —
        // plus any deck pin sharing its basename that the deck would not
        // itself write (missing, bare or relative, non-executable, or a
        // build-artifact path), the shape N worktree builds actually take. A
        // deck rule belonging to a genuinely different, still-valid install is
        // left in place and the new rule is added ALONGSIDE it (issue #730),
        // which is what Claude's `install_impl` has always done.
        strip_deck_commands(arr, |cmd| command_is_replaceable(cmd, binary_path));
        arr.push(entry.clone());
    }
}

/// Reject a structurally-incompatible existing `hooks.json` shape without
/// mutating it. Accepts a missing `hooks` key (created on install) and an empty
/// object, but rejects a non-object root, a non-object `hooks`, or any event
/// value that is not an array — so a merge never silently replaces user content
/// it doesn't understand (finding #1).
fn validate_structure(root: &Value) -> io::Result<()> {
    let incompatible = |what: &str| {
        io::Error::new(
            ErrorKind::InvalidData,
            format!("existing hooks.json is structurally incompatible: {what}"),
        )
    };
    if !root.is_object() {
        return Err(incompatible("root is not a JSON object"));
    }
    let Some(hooks) = root.get("hooks") else {
        return Ok(()); // missing `hooks` is fine — install creates it
    };
    let Some(hooks) = hooks.as_object() else {
        return Err(incompatible("`hooks` is not a JSON object"));
    };
    for (event, value) in hooks {
        if !value.is_array() {
            return Err(incompatible(&format!(
                "hook event `{event}` is not an array"
            )));
        }
    }
    Ok(())
}

/// Testable core: merge the deck's hooks into `<codex_home>/hooks.json`, writing
/// the file atomically (creating the home dir if needed). `binary_path` is the
/// absolute `dot-agent-deck` path the hook command should invoke.
///
/// Safety contract (findings #1, #14, L-2, M-2): the read-modify-write is
/// serialized by [`INSTALL_LOCK`] and published atomically. Only a missing file
/// is treated as empty. Malformed JSON is backed up to `hooks.json.bak` and the
/// call errors (never discarded); a structurally-incompatible shape errors
/// WITHOUT touching the file; unreadable content propagates its error unwritten.
pub fn install_to(codex_home: &Path, binary_path: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(codex_home)?;
    let path = codex_home.join("hooks.json");

    let _guard = INSTALL_LOCK.lock().unwrap_or_else(|p| p.into_inner());

    let mut root = match std::fs::read(&path) {
        Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
            Ok(value) => value,
            Err(parse_err) => {
                // Preserve the user's bytes (best-effort backup) and refuse to
                // overwrite — never discard content we couldn't parse. The copy
                // is published, not `std::fs::write`n: that followed a symlink
                // planted at this predictable `.bak` name (#731).
                let backup = crate::agent_hook_config::backup_malformed(&path, &bytes);
                return Err(io::Error::new(
                    ErrorKind::InvalidData,
                    format!(
                        "existing hooks.json is not valid JSON ({}): {parse_err}",
                        crate::agent_hook_config::preserved_phrase(backup.as_deref())
                    ),
                ));
            }
        },
        Err(e) if e.kind() == ErrorKind::NotFound => json!({}),
        // Unreadable (permissions, etc.): propagate rather than overwrite.
        Err(e) => return Err(e),
    };

    validate_structure(&root)?;

    // Through [`expected_hook_command`], not a parallel `build_command` call
    // (issue #730, auditor N-A). The trust write compares a listed entry against
    // that function's output byte-for-byte, so there must be ONE construction
    // site rather than two that happen to read the same two constants: the
    // invariant "install and trust cannot spell the command differently" is then
    // structural, the way S-2 made the trust half structural, instead of held by
    // duplication that a future edit to either site quietly breaks.
    let command = expected_hook_command(binary_path);
    install_impl(&mut root, &command, binary_path);
    let contents = serde_json::to_string_pretty(&root)?;
    crate::agent_hook_config::write_atomic(codex_home, &path, contents.as_bytes())
}

/// Whether the active `CODEX_HOME`'s `hooks.json` declares any command hook NOT
/// authored by the deck. Returns:
/// - `Ok(false)` when the file is absent or contains only deck-owned command
///   hooks;
/// - `Ok(true)` when a non-deck command hook is present, or when no `CODEX_HOME`
///   resolves (conservative default);
/// - `Err` when the file exists but is unreadable/malformed.
///
/// **DIAGNOSTIC ONLY (PRD #20 §4.1.4).** This used to be a *precondition* for
/// injecting the invocation-global `--dangerously-bypass-hook-trust`, since that
/// flag would have trusted the user's third-party hooks along with the deck's.
/// That flag is deleted: trust is now per-hook, hash-pinned, and scoped by
/// [`deck_owned_entries`], so a foreign hook in the very same `hooks.json` simply
/// stays untrusted while the deck's own entries are trusted. Do NOT reinstate the
/// coupling — gating scoped trust on this would only degrade users who happen to
/// have hooks of their own.
///
/// It inspects `CODEX_HOME/hooks.json` only. Project-local (`<repo>/.codex`),
/// plugin, and `config.toml`-defined hooks are NOT inspected here; the residual
/// is documented in `docs/develop/agent-adapters.md`.
pub fn foreign_command_hooks_present() -> std::io::Result<bool> {
    let Some(home) = codex_home() else {
        return Ok(true);
    };
    foreign_command_hooks_present_in(&home)
}

/// The resolved active Codex home the way Codex itself would (`$CODEX_HOME`, else
/// `$HOME/.codex`), or `None` when neither resolves. PRD #20 Greptile finding #2:
/// the wrapper resolves this ONCE and PINS it on the spawned Codex child's
/// environment so the home the deck vetted and installed into is exactly the home
/// Codex loads — vet and launch use the same, deck-controlled home instead of a
/// value a launcher could drift between the two.
pub fn active_codex_home() -> Option<PathBuf> {
    codex_home()
}

/// Like [`foreign_command_hooks_present`] but against an EXPLICIT `home`, so a
/// caller can resolve the home once and vet the SAME path it will pin on the
/// child (finding #2). Returns `Ok(false)` when the file is absent.
pub fn foreign_command_hooks_present_in(home: &Path) -> std::io::Result<bool> {
    let path = home.join("hooks.json");
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    let root: Value =
        serde_json::from_slice(&bytes).map_err(|e| io::Error::new(ErrorKind::InvalidData, e))?;
    Ok(any_foreign_command_hook(&root))
}

/// Walk `root.hooks.<event>[].hooks[]` and report whether any `type == "command"`
/// handler's command is NOT deck-owned (per [`command_is_deck_owned`]).
fn any_foreign_command_hook(root: &Value) -> bool {
    let Some(hooks) = root.get("hooks").and_then(Value::as_object) else {
        return false;
    };
    for rules in hooks.values() {
        let Some(rules) = rules.as_array() else {
            continue;
        };
        for rule in rules {
            let Some(handlers) = rule.get("hooks").and_then(Value::as_array) else {
                continue;
            };
            for handler in handlers {
                if handler.get("type").and_then(Value::as_str) != Some("command") {
                    continue;
                }
                let foreign = handler
                    .get("command")
                    .and_then(Value::as_str)
                    .is_some_and(|command| !command_is_deck_owned(command));
                if foreign {
                    return true;
                }
            }
        }
    }
    false
}

/// Silently install the Codex hooks into the active `CODEX_HOME`. Guarded
/// (`CODEX_HOME`/`HOME` must resolve), idempotent-overwrite, never a
/// throwaway/`/tmp` write. Invoked from the wrapper ([`crate::wrap::run_wrap`])
/// just before a `codex` child is spawned, so the hooks are on disk before Codex
/// boots and discovers them. Failures are swallowed (best-effort, like Claude's
/// `auto_install`): a missing home or unwritable dir degrades to the coarse
/// stdout fallback rather than blocking the spawn.
///
/// Returns the durable binary path the definitions were written for, or `None`
/// if nothing was written. The caller needs it to build the expected command
/// [`trust_deck_hooks_in`] compares against (issue #730) — the SAME value, so
/// install and trust can never be about two different binaries.
pub fn auto_install() -> Option<String> {
    let home = codex_home()?;
    // PRD #381: never `current_exe()` directly — a `target/debug` path written
    // here is gone the moment its worktree is pruned, and this write is silent
    // and automatic. A refusal writes nothing and warns; `hooks.json` is left
    // exactly as it was.
    let binary_path = match crate::platform::paths::durable_binary_path() {
        Ok(binary_path) => binary_path,
        Err(e) => {
            tracing::warn!("auto-install: {e}");
            return None;
        }
    };
    if let Err(e) = install_to(&home, &binary_path) {
        tracing::warn!("auto-install: failed to write Codex hooks.json: {e}");
        return None;
    }
    Some(binary_path)
}

/// The exact hook command the deck writes for `binary_path` — what
/// [`install_to`] puts in `hooks.json` and what [`trust_deck_hooks_in`] compares
/// a listed entry against.
///
/// **The single construction site for both, deliberately.** Both callers used to
/// call `build_command` themselves with the same two constants, which made "the
/// install and the trust write cannot spell the command differently" a property
/// of two sites agreeing rather than of there being one site (issue #730,
/// auditor N-A). Do not inline it back into either.
pub fn expected_hook_command(binary_path: &str) -> String {
    crate::agent_hook_config::build_command(binary_path, HOOK_COMMAND_SUFFIX, HOOK_SHELL)
}

/// Remove the deck's own hook rules from `<codex_home>/hooks.json`, leaving every
/// user-authored rule (and any file the deck can't parse) alone. The counterpart
/// of [`install_to`], backing `dot-agent-deck hooks uninstall --agent codex`
/// (PRD #20 §4.2.1 — the documented CLI). A missing file is a no-op.
pub fn uninstall_from(codex_home: &Path) -> std::io::Result<()> {
    let path = codex_home.join("hooks.json");

    let _guard = INSTALL_LOCK.lock().unwrap_or_else(|p| p.into_inner());

    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let mut root: Value = serde_json::from_slice(&bytes)
        .map_err(|e| io::Error::new(ErrorKind::InvalidData, format!("hooks.json: {e}")))?;
    validate_structure(&root)?;
    if let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) {
        for value in hooks.values_mut() {
            if let Some(arr) = value.as_array_mut() {
                // Wide ownership on purpose: uninstall removes the deck's rules
                // wholesale, whichever install wrote them. Command granularity
                // is what keeps a user's sibling handler sharing the rule object
                // from going with them (issue #730).
                crate::agent_hook_config::strip_deck_commands(arr, command_is_deck_owned);
            }
        }
        hooks.retain(|_, value| !value.as_array().is_some_and(|arr| arr.is_empty()));
    }
    let contents = serde_json::to_string_pretty(&root)?;
    crate::agent_hook_config::write_atomic(codex_home, &path, contents.as_bytes())
}

// ---------------------------------------------------------------------------
// PRD #20 §4.1.1/§4.1.2 — scoped, hash-pinned per-hook trust
// ---------------------------------------------------------------------------

/// The name of Codex's per-hook trust store, relative to the Codex home. Trust
/// records live ONLY here: writing a `state` key inside `hooks.json` is rejected
/// by Codex outright (`unknown field 'state'`), which stops the whole file from
/// parsing.
const CONFIG_TOML: &str = "config.toml";

/// How long [`list_hooks_in`] waits for `codex app-server` to answer before giving
/// up (measured round trip on a real Codex 0.144.4: ~0.17 s). The listing is
/// best-effort — a timeout returns `Err` and the caller degrades.
const HOOKS_LIST_TIMEOUT: Duration = Duration::from_secs(5);

/// One hook as Codex itself reports it from `hooks/list`. The deck never computes
/// these values: `current_hash` in particular is Codex's own canonicalization of
/// the definition, so recording it (rather than a hand-rolled sha256) is what
/// makes trust content-pinned and fail-closed — if Codex ever changes its hashing,
/// our stale record simply stops matching and the hooks don't run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexHookEntry {
    /// The full trust key: `<sourcePath>:<event_snake>:<group_idx>:<handler_idx>`.
    /// It MUST be used verbatim — a short `pre_tool_use:0:0` suffix does not match.
    pub key: String,
    /// The handler's command line, as Codex parsed it.
    pub command: String,
    /// The file the definition came from (`<CODEX_HOME>/hooks.json` for ours).
    pub source_path: PathBuf,
    /// Codex's hash of this exact definition (`sha256:…`), recorded as
    /// `trusted_hash`.
    pub current_hash: String,
    /// `untrusted` | `trusted` | `modified` — Codex's verdict for this entry.
    pub trust_status: String,
    /// `Some(true)` for a managed (root/MDM-provisioned) hook — never
    /// deck-owned. `None` means the listing did not carry `isManaged` at all,
    /// which [`deck_owned_entries`] resolves **per direction** rather than with
    /// one default (issue #730).
    ///
    /// **Why an `Option` and not a `bool`.** The field was decoded
    /// `.unwrap_or(false)`, i.e. "absent ⇒ not managed", which is the
    /// fail-**open** answer for the trust write's condition 3. Flipping the
    /// default instead would have silently narrowed the revocation, which needs
    /// the opposite answer. Keeping "absent" distinguishable is what lets each
    /// caller pick its own safe side; see [`deck_owned_entries`].
    ///
    /// **What was measured, scoped to the version.** Across four entry shapes
    /// under **codex-cli 0.149.0** — an ordinary user command hook, a
    /// deck-shaped one, a second handler inside a rule, and an `mcp_tool`
    /// handler whose entry carries a *different field set* entirely (no
    /// `command`, no `async`) — `isManaged` was present on **every** entry,
    /// always a real JSON bool. That last shape is the useful evidence: the
    /// field survives a handler-type variant that drops other fields, so it is
    /// a plain `bool` on the outer struct rather than something conditionally
    /// serialized. This is a claim about 0.149.0, not about Codex in general, so
    /// `None` is protocol drift rather than an expected state today.
    ///
    /// **An `isManaged: true` entry was NOT measured.** Managed hooks come from
    /// `/etc/codex/managed_config.toml`, which needs root, and the binary
    /// exposes no env override (`$CODEX_HOME/managed_config.toml` was tried and
    /// contributes nothing to the listing). That a struct which emits `false`
    /// will also emit `true` is a **serde argument** — a field serialized
    /// unconditionally has no `skip_serializing_if` — and not a measurement.
    pub is_managed: Option<bool>,
}

/// Ask Codex itself for every hook it would load for `cwd` under `home`.
///
/// Mechanism (Codex 0.144.4, verified): `codex app-server` speaks line-delimited
/// JSON-RPC on stdio (an `[experimental]` surface). Two requests suffice, with no
/// credentials and no network:
///
/// ```text
/// → {"jsonrpc":"2.0","id":1,"method":"initialize","params":{"clientInfo":{…}}}
/// ← {"id":1,"result":{"userAgent":…,"codexHome":…}}
/// → {"jsonrpc":"2.0","id":2,"method":"hooks/list","params":{"cwd":"<cwd>"}}
/// ← {"id":2,"result":{"data":[{"cwd":"…","hooks":[ <entry>, … ],
///                             "warnings":[],"errors":[]}]}}
/// ```
///
/// where each `<entry>` carries `key`, `eventName`, `command`, `sourcePath`,
/// `source`, `pluginId`, `isManaged`, `enabled`, `currentHash`, and `trustStatus`
/// (camelCase). This function is the ONE place that shape is decoded.
///
/// `CODEX_HOME` is set to `home` on the child so the listing describes exactly the
/// home the deck installed into and pins on the Codex child. stderr is discarded,
/// the wait is bounded by [`HOOKS_LIST_TIMEOUT`], and the child is killed before
/// returning. EVERY failure — codex absent, non-zero exit, protocol drift, timeout
/// — is an `Err` so the caller degrades quietly (no spawn is ever blocked).
pub fn list_hooks_in(home: &Path, cwd: &Path) -> std::io::Result<Vec<CodexHookEntry>> {
    let mut child = Command::new("codex")
        .arg("app-server")
        .env("CODEX_HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;

    let requests = format!(
        "{}\n{}\n",
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "clientInfo": {
                    "name": "dot-agent-deck",
                    "title": "dot-agent-deck",
                    "version": env!("CARGO_PKG_VERSION"),
                }
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "hooks/list",
            "params": { "cwd": cwd.display().to_string() }
        }),
    );

    // Keep the stdin handle ALIVE until the response is read: a real app-server
    // may treat EOF as "shut down" and exit before answering.
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::new(ErrorKind::BrokenPipe, "codex app-server: no stdin pipe"))?;
    let write_result = stdin
        .write_all(requests.as_bytes())
        .and_then(|()| stdin.flush());

    // Read on a helper thread so the wait is genuinely bounded (a blocked
    // `read_line` cannot be cancelled). The thread is detached; it ends when the
    // killed child's stdout closes.
    let response = write_result.and_then(|()| {
        let stdout = child.stdout.take().ok_or_else(|| {
            io::Error::new(ErrorKind::BrokenPipe, "codex app-server: no stdout pipe")
        })?;
        let (tx, rx) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    return;
                }
            }
        });
        read_hooks_list_reply(&rx, Instant::now() + HOOKS_LIST_TIMEOUT)
    });

    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();
    response
}

/// Is this line the `hooks/list` **response**, as opposed to some other message
/// that happens to carry `id: 2`?
///
/// Three conditions, and the last two are the fix for issue #1033. A JSON-RPC
/// peer numbers its OWN requests from its OWN counter, so a server→client
/// *request* — `codex app-server` advertises several, e.g.
/// `account/chatgptAuthTokens/refresh`, `item/tool/requestUserInput`,
/// `mcpServer/elicitation/request` — can carry `id: 2` while being nothing to do
/// with the request the deck sent. Matching on the id alone therefore handed
/// [`parse_hooks_list`] a message with neither `result` nor `error`, which it
/// correctly rejected as `hooks/list reply carried no result`; and because the
/// old arm `return`ed, that ended the whole attempt with the genuine reply still
/// unread on the stream. So:
///
/// - `id == 2` — the id the deck itself sent for `hooks/list`;
/// - **no `method`** — a message carrying one is a request or a notification,
///   never a response (an explicit `method: null` is tolerated as absent, which
///   errs toward accepting a genuine reply);
/// - **`result` or `error` present** — the two shapes a JSON-RPC response can
///   take, and exactly what [`parse_hooks_list`] needs.
///
/// This is the defect stated from the code, not from any diagnosis of why a
/// particular host sees a particular stray message; the caller `continue`s past
/// anything this rejects, so being wrong about *which* messages arrive costs a
/// loop iteration rather than the call.
fn is_hooks_list_response(value: &Value) -> bool {
    let is_request_or_notification = value.get("method").is_some_and(|m| !m.is_null());
    value.get("id").and_then(Value::as_i64) == Some(2)
        && !is_request_or_notification
        && (value.get("result").is_some() || value.get("error").is_some())
}

/// Drain `rx` until the `hooks/list` response arrives or `deadline` passes.
///
/// Split out of [`list_hooks_in`] so the matching can be driven from a synthetic
/// stream with no `codex` on the box (issue #1033). Everything that is not the
/// response — the `initialize` reply, notifications, and any server→client
/// request, including one that happens to reuse `id: 2` — is skipped rather than
/// returned on, so no single stray message can poison the call.
///
/// **The skipping is bounded by `deadline` and nothing else**, which is what
/// keeps `continue` from being a way to spin: `remaining` is recomputed every
/// iteration and is both the `recv_timeout` bound and, once zero, the exit. A
/// peer that floods messages makes the loop iterate faster, not for longer —
/// each iteration consumes one message and the total wait is still capped at
/// [`HOOKS_LIST_TIMEOUT`].
fn read_hooks_list_reply(
    rx: &mpsc::Receiver<String>,
    deadline: Instant,
) -> std::io::Result<Vec<CodexHookEntry>> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                ErrorKind::TimedOut,
                "codex app-server: hooks/list did not answer in time",
            ));
        }
        match rx.recv_timeout(remaining) {
            Ok(line) => match serde_json::from_str::<Value>(&line) {
                Ok(value) if is_hooks_list_response(&value) => return parse_hooks_list(&value),
                _ => continue,
            },
            Err(mpsc::RecvTimeoutError::Timeout) => {
                return Err(io::Error::new(
                    ErrorKind::TimedOut,
                    "codex app-server: hooks/list did not answer in time",
                ));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(io::Error::new(
                    ErrorKind::UnexpectedEof,
                    "codex app-server: exited without answering hooks/list",
                ));
            }
        }
    }
}

/// Decode the `id: 2` `hooks/list` reply into entries, tolerating both the
/// per-`cwd` grouped shape (`result.data[].hooks[]`, what 0.144.4 returns) and a
/// flat `result.hooks[]`. An entry missing `key` or `currentHash` is DROPPED
/// rather than guessed at — a trust record without Codex's own hash is worthless.
/// A JSON-RPC `error` reply, or a reply with no recognizable hook array, is `Err`.
fn parse_hooks_list(response: &Value) -> std::io::Result<Vec<CodexHookEntry>> {
    if let Some(error) = response.get("error") {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            format!("codex app-server: hooks/list failed: {error}"),
        ));
    }
    let result = response.get("result").ok_or_else(|| {
        io::Error::new(
            ErrorKind::InvalidData,
            "codex app-server: hooks/list reply carried no result",
        )
    })?;
    let groups: Vec<&Value> = match result.get("data").and_then(Value::as_array) {
        Some(data) => data.iter().collect(),
        None => vec![result],
    };
    let mut found_array = false;
    let mut entries = Vec::new();
    for group in groups {
        let Some(hooks) = group.get("hooks").and_then(Value::as_array) else {
            continue;
        };
        found_array = true;
        for hook in hooks {
            let string = |field: &str| {
                hook.get(field)
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .filter(|s| !s.is_empty())
            };
            let (Some(key), Some(current_hash)) = (string("key"), string("currentHash")) else {
                continue;
            };
            entries.push(CodexHookEntry {
                key,
                command: string("command").unwrap_or_default(),
                source_path: PathBuf::from(string("sourcePath").unwrap_or_default()),
                current_hash,
                trust_status: string("trustStatus").unwrap_or_else(|| "unknown".into()),
                // Absent stays ABSENT rather than collapsing to `false` here
                // (issue #730): the two callers of `deck_owned_entries` need
                // opposite defaults, so the decision belongs there and not in
                // the decoder. See `CodexHookEntry::is_managed`.
                is_managed: hook.get("isManaged").and_then(Value::as_bool),
            });
        }
    }
    if !found_array {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "codex app-server: hooks/list reply carried no hooks array",
        ));
    }
    Ok(entries)
}

/// How closely a listed entry's command must match for the entry to count as the
/// deck's — the knob issue #730 adds, because trust and untrust want different
/// answers and collapsing them to one is what made the suffix a security
/// predicate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeckCommandMatch<'a> {
    /// Byte-exactly the command the deck writes for a validated durable path —
    /// `agent_hook_config::build_command(binary_path, …)`, the same call
    /// [`install_to`] makes. **This is the one to use for a trust WRITE**, which
    /// is a grant and must fail closed: the entry has to be the deck invoking
    /// its own binary, not merely some command ending in the deck's verb.
    ///
    /// **No trimming, deliberately, and that is the asymmetry with
    /// [`Signature`](DeckCommandMatch::Signature)** — which reaches
    /// `agent_hook_config::command_executable` and so trims trailing whitespace
    /// before its suffix test. Measured on Codex 0.149.0: a command written
    /// with a deliberate trailing space comes back from `hooks/list`
    /// byte-identically, space included, AND carries a different `currentHash`
    /// from its untrimmed twin. So Codex mirrors the JSON string faithfully, an
    /// entry differing from ours only by whitespace is a genuinely different
    /// definition rather than a spelling of ours, and trimming here would widen
    /// a grant to cover it. Liberal for a mutation, exact for a grant.
    Exact(&'a str),
    /// Any command carrying the deck signature, whichever install wrote it and
    /// whatever executable it names ([`command_is_deck_owned`]).
    ///
    /// **Never for a grant. The only WRITE that may use it is the revocation**
    /// ([`untrust_deck_hooks_in`]), where dropping a trust record is fail-open
    /// in the safe direction — the worst case is removing a record the deck did
    /// not write, bounded by conditions 1 and 3 below — whereas narrowing it
    /// would leave a record behind for an entry a sibling deck install wrote and
    /// this uninstall is about to delete from `hooks.json`.
    ///
    /// A READ-ONLY use is also legitimate, and there is one:
    /// [`warn_if_our_own_entry_was_unrecognisable`] asks this question to tell an
    /// ordinary zero-trust write from a broken one, and does nothing with the
    /// answer but count it and log. So the test for a new call site is not "is
    /// it the revocation" but "does it write, and if so does it only ever remove
    /// privilege" (this doc said "For REVOCATION only", which the read-only use
    /// added in the same commit falsified).
    Signature,
}

/// **The security predicate — the only production path to a trust write**
/// (`upsert_trust_record`'s one non-test caller is [`trust_deck_hooks_in`], which
/// reaches this). Keep an entry only
/// when ALL of the following hold:
///
/// 1. its `source_path` is the PINNED home's own `hooks.json` — the file the deck
///    authored and pins on the Codex child, not a project-local, plugin, or
///    other-home definition;
/// 2. its command satisfies `how` — see [`DeckCommandMatch`], and note that for
///    a trust write that means the EXACT command built from the validated
///    durable path, not an arbitrary executable followed by the deck's verb;
/// 3. it is not `isManaged` — a managed hook is provisioned by root/MDM and is
///    never the deck's to trust. Where the listing OMITS the field, the answer
///    is resolved from `how` rather than from one shared default: `Exact`
///    (a grant) treats an absent field as managed and drops the entry, while
///    `Signature` (the revocation) treats it as unmanaged and keeps it. See the
///    comment on `managed_if_absent` in the body, and
///    [`CodexHookEntry::is_managed`] for what 0.149.0 was measured to emit.
///
/// So the deck records trust ONLY for COMMANDS it wrote itself, one exact hash
/// at a time. Paths are compared verbatim first and, only if that fails, by
/// canonicalized form (so a symlinked home still matches the SAME real file).
///
/// **Commands, not definitions — the gap is real and bounded.** `Exact`
/// compares the command string and nothing else. Measured on Codex 0.149.0,
/// `currentHash` covers the command, the rule's `matcher` and the handler's
/// `async` flag, so an entry the deck did NOT author, carrying the deck's exact
/// command under a different `matcher` or `async`, satisfies condition 2 and
/// gets a `trusted_hash`. What that grants is execution of the deck's own hook
/// binary, so it is not an escalation — the worst outcome is the deck's own
/// command running at a matcher the deck did not choose. Comparing the whole
/// definition would need the deck to re-derive it from a file held open across
/// the listing, which Codex's protocol does not offer.
///
/// **Why condition 2 had to be narrowed (issue #730, auditor finding LOW-1).**
/// It used to be the bare suffix test, and the suffix is a *convention*, not a
/// capability: a user-authored command that intentionally ends in
/// `hook --agent codex` is indistinguishable from a deck entry under it. That
/// left a window — a command placed in the deck's own `hooks.json` that merely
/// ends with the deck's verb could be handed a `trusted_hash`, which is Codex's
/// permission to RUN it. Tightening the suffix is not the fix, because there is
/// no suffix a user cannot also write; comparing against the exact command the
/// deck itself would emit for the path it validated is, because that names the
/// deck's own binary and nothing else.
pub fn deck_owned_entries<'a>(
    entries: &'a [CodexHookEntry],
    home: &Path,
    how: DeckCommandMatch<'_>,
) -> Vec<&'a CodexHookEntry> {
    let ours = home.join("hooks.json");
    let ours_real = ours.canonicalize().ok();
    entries
        .iter()
        .filter(|entry| {
            let same_file = entry.source_path == ours
                || (ours_real.is_some() && entry.source_path.canonicalize().ok() == ours_real);
            // `managed_if_absent` is condition 3's answer when the listing did
            // not carry `isManaged` at all, and it differs BY DIRECTION (issue
            // #730). A grant must assume the entry IS managed — fail closed,
            // never hand a `trusted_hash` to something that might be
            // root-provisioned. A revocation must assume it is NOT — fail open
            // in the revoking direction, as `untrust_deck_hooks_in`'s own doc
            // requires, because assuming "managed" there would leave trust
            // records behind after `hooks uninstall` deleted the definitions.
            // One shared default cannot be both, which is why the field is an
            // `Option` (see `CodexHookEntry::is_managed`).
            let (command_matches, managed_if_absent) = match how {
                DeckCommandMatch::Exact(expected) => (entry.command == expected, true),
                DeckCommandMatch::Signature => (command_is_deck_owned(&entry.command), false),
            };
            same_file && command_matches && !entry.is_managed.unwrap_or(managed_if_absent)
        })
        .collect()
}

/// What a trust write did, for the benefit of a caller that has to tell a user
/// (issue #730, auditor S-C).
///
/// This used to be a bare `usize`, and a bare `usize` cannot say why a zero is a
/// zero. The two causes of zero are not alike: "Codex has nothing of the deck's
/// to trust" is the ordinary one and happens on every launch on a machine
/// without Codex hooks, while "Codex enumerated our own entries and none of them
/// carries the command we just wrote" is full trust silently becoming zero
/// trust. The `hooks install` CLI prints one line for this and only this, so
/// collapsing the two there printed the first cause's sentence on the second
/// cause's branch — the exact diagnostic dead end
/// [`warn_if_our_own_entry_was_unrecognisable`] exists to break.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustOutcome {
    /// `n` entries were trusted, `n >= 1`.
    Trusted(usize),
    /// Nothing in Codex's listing was ELIGIBLE for a trust write — which is
    /// wider than "Codex enumerated no entry of the deck's" and must not be
    /// reported as that (Greptile P2 on PR #1029). Three ways in: the deck's
    /// hooks are not installed; Codex listed nothing carrying the deck's
    /// signature; or it listed deck-signature entries that
    /// [`deck_owned_entries`] rejected on one of its OTHER conditions — an
    /// `isManaged` entry, or one whose `source_path` is not this home's own
    /// `hooks.json`. This variant cannot tell the three apart, so no caller of
    /// it may name a cause. Quiet by design: the first is the ordinary one and
    /// happens on every launch on a machine without Codex hooks.
    NothingListed,
    /// Codex enumerated `listed` deck-signature entries out of the deck's own
    /// `hooks.json` and none of them carried the command this install wrote, so
    /// nothing was trusted. [`warn_if_our_own_entry_was_unrecognisable`] has
    /// already logged the detail; this is the same fact in the return value, for
    /// a caller whose user is not reading a log file.
    Unrecognised { listed: usize },
}

impl TrustOutcome {
    /// How many entries were trusted — zero for both zero causes, so a caller
    /// that only wants the count does not have to match.
    pub fn trusted(self) -> usize {
        match self {
            Self::Trusted(count) => count,
            Self::NothingListed | Self::Unrecognised { .. } => 0,
        }
    }
}

/// Record scoped, hash-pinned trust for the deck's OWN hooks in `home`, reporting
/// what happened as a [`TrustOutcome`] (PRD #20 §4.1.2).
///
/// `binary_path` is the durable path [`install_to`] just wrote definitions for.
/// The expected command is derived from it HERE, by [`expected_hook_command`] —
/// the same call `install_to` makes — and only entries carrying that exact
/// string are trusted ([`DeckCommandMatch::Exact`], issue #730).
///
/// Taking the path rather than a pre-built command is what makes that invariant
/// structural instead of call-site discipline: a caller physically cannot hand
/// this an "expected" string that is not a command the deck generates. It still
/// serves both reasons the parameter exists at all — install and trust cannot
/// disagree about which binary this run is about, because the caller passes the
/// very path it installed with, and a test can still drive the predicate with a
/// path it controls instead of inheriting whatever the host has at
/// `~/.local/bin/dot-agent-deck`. A caller that could not resolve a durable path
/// has no binary path and must not call this at all: no trust is the fail-closed
/// answer, and the definitions it would have trusted were never written either.
///
/// Asks Codex for the listing ([`list_hooks_in`]), narrows it with
/// [`deck_owned_entries`], and writes `[hooks.state."<key>"] { trusted_hash =
/// "<current_hash>" }` for exactly those keys into `<home>/config.toml` — the
/// hash and nothing else, so a user's `enabled = false` toggle on the deck's own
/// hook survives every subsequent spawn ([`upsert_trust_record`] has the
/// measurements). The edit is format-preserving and the publish is atomic
/// under [`INSTALL_LOCK`], so a concurrent deck writer can't interleave and the
/// user's comments/settings survive byte-intact.
///
/// This REPLACES the old invocation-global `--dangerously-bypass-hook-trust`:
/// it is launch-method agnostic (trust lives in the home, not argv), never trusts
/// a hook the deck didn't author, and fails closed (any error ⇒ the hooks stay
/// untrusted and events degrade to the coarse stdout classifier).
pub fn trust_deck_hooks_in(
    home: &Path,
    cwd: &Path,
    binary_path: &str,
) -> std::io::Result<TrustOutcome> {
    let expected = expected_hook_command(binary_path);
    let entries = list_hooks_in(home, cwd)?;
    let records: Vec<(String, String)> =
        deck_owned_entries(&entries, home, DeckCommandMatch::Exact(&expected))
            .into_iter()
            .map(|entry| (entry.key.clone(), entry.current_hash.clone()))
            .collect();
    if records.is_empty() {
        // The warn is the diagnosis; its count is what tells the two causes of
        // zero apart, so the caller gets it too rather than having to read a log
        // that may not even have a subscriber (issue #730, auditor S-C).
        let listed = warn_if_our_own_entry_was_unrecognisable(&entries, home, &expected);
        return Ok(if listed == 0 {
            TrustOutcome::NothingListed
        } else {
            TrustOutcome::Unrecognised { listed }
        });
    }
    let _guard = INSTALL_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    edit_trust_state(home, |state| {
        for (key, hash) in &records {
            upsert_trust_record(state, key, hash);
        }
    })?;
    Ok(TrustOutcome::Trusted(records.len()))
}

/// Say something when a trust write recorded NOTHING even though the listing
/// carried an entry that looks like ours, and report how many such entries there
/// were (issue #730). Zero means the listing carried none, i.e. the ordinary
/// cause.
///
/// A zero trust write has two very different causes and they must not read
/// alike. The ordinary one — Codex is not installed, or has nothing of ours to
/// enumerate — is quiet by design and happens on every launch on a machine
/// without Codex hooks. The other is "we installed and then could not recognise
/// our own entry", which is what a Codex that stopped echoing `command`
/// byte-for-byte would produce: [`DeckCommandMatch::Exact`] then matches
/// nothing, full trust silently becomes zero trust, and Codex events degrade to
/// the coarse stdout classifier with no error anywhere.
///
/// **What separates them here** is that [`DeckCommandMatch::Signature`] over
/// the same listing still finds an entry (it is in our own `hooks.json` and it
/// ends in our verb) while `Exact` did not, so the command string we wrote is
/// not the command string Codex reports for an entry in our own file. That is
/// the observation the log line makes, and it is the observation the return
/// value carries — **not** a claim about which of the two causes produced it.
/// A third shape reaches the same branch: a *sibling* deck install's command
/// sitting in our own `hooks.json` while our own entry is absent from the
/// listing also makes `Signature` match and `Exact` miss. Worth saying because
/// this used to claim the two were "distinguishable exactly here"; what is
/// distinguishable is "nothing of ours was listed" from "something with our
/// signature was, and it is not what we wrote".
///
/// Warn rather than error: the fail-closed outcome is correct and the spawn must
/// still proceed. This only refuses to be silent about it.
fn warn_if_our_own_entry_was_unrecognisable(
    entries: &[CodexHookEntry],
    home: &Path,
    expected: &str,
) -> usize {
    let ours = deck_owned_entries(entries, home, DeckCommandMatch::Signature);
    let Some(sample) = ours.first() else {
        return 0;
    };
    // **The reported command is DESCRIBED, not quoted** (issue #730, auditor
    // N-E). `Signature` only requires a command to END in the deck's verb, so
    // everything before the suffix is arbitrary text out of the user's own
    // `hooks.json` — a hook line carrying a secret in that position would be
    // copied into `deck.log`, which CLAUDE.md rule 12 treats as shareable. A
    // length and the length of the prefix it agrees with `expected` on answer
    // the only question a reader has (how far do the two diverge, and is this a
    // whitespace-or-quoting difference or a different program?) without copying
    // a byte of it; the entry itself is in the file, one `cat` away.
    //
    // **Both values stay STRUCTURED FIELDS, and that is the escaping guarantee**
    // (auditor N-D). `tracing-subscriber`'s field visitor routes a non-`message`
    // field through `str`'s `Debug`, which escapes newlines, CR, quotes and
    // `ESC` — so no value here can split a log line or inject a terminal escape.
    // Interpolating one into the message string instead routes it through a
    // writer-dependent path with no such promise. Nothing read out of a
    // third-party file is left in these fields today — `expected` is a command
    // the deck generated for a path its own resolver validated, and the other
    // two are integers — but if a borrowed string ever comes back, it belongs in
    // a field for this reason.
    tracing::warn!(
        listed = ours.len(),
        expected,
        reported_len = sample.command.len(),
        reported_agreeing_prefix = agreeing_prefix_bytes(&sample.command, expected),
        "codex: our own hooks.json carries deck-signature entries but none matches the command \
         this install wrote, so nothing was trusted; Codex events degrade to stdout \
         classification. The reported command is described by length rather than quoted — read \
         the entry itself from hooks.json in this Codex home"
    );
    ours.len()
}

/// How many leading BYTES `a` and `b` agree on — a content-free way to say how
/// far a reported hook command and the expected one diverge (issue #730).
///
/// Bytes, not characters: this is a diagnostic magnitude, not an index. Its one
/// caller logs it and nothing else, so it never cuts either input — which is
/// what keeps a byte count that could land mid-character harmless.
fn agreeing_prefix_bytes(a: &str, b: &str) -> usize {
    a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count()
}

/// Drop the trust records for the deck's own hooks in `home` — the uninstall
/// counterpart of [`trust_deck_hooks_in`], returning how many were removed.
///
/// Deck ownership is resolved through the same function as the write
/// ([`deck_owned_entries`]) but at the WIDE [`DeckCommandMatch::Signature`]
/// setting, so a record written by a *sibling* deck install is dropped too.
/// Call it BEFORE removing the definitions, while Codex can still enumerate
/// them.
///
/// **What the wide setting reaches, stated rather than glossed.** A hook the
/// deck did not author, whose command merely ends in the deck's verb and which
/// lives in the deck's own `hooks.json`, is selected here and its trust record
/// dropped — `codex_trust_004` asserts exactly that (`revocable == ["deck",
/// "crafted"]`), and [`DeckCommandMatch::Signature`]'s own doc says the same.
/// That is the right direction for a revocation (it can only remove privilege)
/// and it matches [`uninstall_from`], which removes that same command from
/// `hooks.json` at the same wide setting, so the two do not disagree about what
/// an uninstall covers.
///
/// **The asymmetry with [`trust_deck_hooks_in`] is deliberate (issue #730).** A
/// trust write is a grant and must fail closed, so it takes the exact command.
/// An untrust is a revocation and must fail *open in the revoking direction*, so
/// it takes the signature: [`uninstall_from`] deletes every deck-owned command
/// from `hooks.json` whichever install wrote it, and a narrower predicate here
/// would leave the trust record for a sibling install's entry behind after its
/// definition was gone. Such an orphan is inert — Codex pins trust to the
/// definition's hash, so a different definition arriving at the same key reads
/// as `modified` and is refused — but it is still state this uninstall promised
/// to clear, and the wider predicate is bounded by the same two conditions the
/// narrow one is: the entry must live in the deck's own `hooks.json` and must
/// not be managed.
pub fn untrust_deck_hooks_in(home: &Path) -> std::io::Result<usize> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| home.to_path_buf());
    let entries = list_hooks_in(home, &cwd)?;
    let keys: Vec<String> = deck_owned_entries(&entries, home, DeckCommandMatch::Signature)
        .into_iter()
        .map(|entry| entry.key.clone())
        .collect();
    if keys.is_empty() {
        return Ok(0);
    }
    let _guard = INSTALL_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut removed = 0;
    edit_trust_state(home, |state| {
        for key in &keys {
            if state.remove(key.as_str()).is_some() {
                removed += 1;
            }
        }
    })?;
    Ok(removed)
}

/// Read `<home>/config.toml`, hand `edit` the `[hooks.state]` table to mutate, and
/// publish the result atomically — WITHOUT reformatting anything else.
///
/// `toml_edit` (not a `toml`/serde round trip) is what makes this safe on the
/// user's real `~/.codex/config.toml`: comments, key order, spacing, and every
/// unrelated table come back byte-identical, and a new table is appended at the
/// end. A missing file starts from an empty document; an unparseable one is an
/// error and is left untouched (we never discard a config we don't understand).
fn edit_trust_state(home: &Path, edit: impl FnOnce(&mut toml_edit::Table)) -> std::io::Result<()> {
    use toml_edit::{DocumentMut, Item, Table};

    std::fs::create_dir_all(home)?;
    let path = home.join(CONFIG_TOML);
    let existing = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(e) if e.kind() == ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    let mut doc = existing.parse::<DocumentMut>().map_err(|e| {
        io::Error::new(
            ErrorKind::InvalidData,
            format!("Codex {CONFIG_TOML} is not valid TOML (left unchanged): {e}"),
        )
    })?;

    // `[hooks]` / `[hooks.state]` are created IMPLICIT when absent, so the file
    // gains only the `[hooks.state."<key>"]` header(s) it needs — no bare
    // `[hooks]` / `[hooks.state]` headers appear in the user's config.
    let hooks = doc
        .as_table_mut()
        .entry("hooks")
        .or_insert_with(|| {
            let mut table = Table::new();
            table.set_implicit(true);
            Item::Table(table)
        })
        .as_table_mut()
        .ok_or_else(|| {
            io::Error::new(
                ErrorKind::InvalidData,
                format!("Codex {CONFIG_TOML}: `hooks` is not a table (left unchanged)"),
            )
        })?;
    let state = hooks
        .entry("state")
        .or_insert_with(|| {
            let mut table = Table::new();
            table.set_implicit(true);
            Item::Table(table)
        })
        .as_table_mut()
        .ok_or_else(|| {
            io::Error::new(
                ErrorKind::InvalidData,
                format!("Codex {CONFIG_TOML}: `hooks.state` is not a table (left unchanged)"),
            )
        })?;

    edit(state);

    crate::agent_hook_config::write_atomic(home, &path, doc.to_string().as_bytes())
}

/// Insert or refresh one `[hooks.state."<key>"] { trusted_hash }` record.
///
/// An existing record for `key` is updated IN PLACE — as a table or as an inline
/// table, whichever the user (or a previous run) already wrote — so repeated
/// trust writes are idempotent and never duplicate the table.
///
/// **`trusted_hash` and NOTHING else, which is a deliberate narrowing** (issue
/// #730). This used to also write `enabled = true`, on every arm — including
/// into a record it was merely *updating*. Measured against codex-cli 0.149.0,
/// `enabled` is a **user knob** and is fully orthogonal to trust:
///
/// - It is a first-class affordance in Codex's own `/hooks` browser ("Turn hooks
///   on or off. Your changes are saved automatically."), where `toggle` is a
///   **separate action from `trust`**.
/// - All four combinations of `enabled` × `trusted_hash` exist and behave
///   independently. An entry with `enabled: false` and a correct hash still
///   reports `trustStatus: trusted` and is **still enumerated** by `hooks/list`;
///   distrust is expressed by an absent or stale `trusted_hash`, surfacing as
///   `trustStatus: untrusted` / `modified`. So `enabled = false` is never a
///   distrust marker, and respecting it cannot strand the deck after a
///   legitimate reinstall.
/// - An **absent** `enabled` key defaults to `true` — measured: a record
///   carrying only `trusted_hash` reported back `enabled: true, trustStatus:
///   trusted`, which is exactly the record shape this function now produces.
/// - Codex's own trust write targets `trusted_hash` alone: the 0.149.0 binary's
///   string table holds exactly one `hooks.state."` format fragment and the
///   piece after it is `".trusted_hash`, with no `".enabled` fragment anywhere
///   in the binary.
///
/// Writing it unconditionally therefore silently reverted an explicit user
/// choice on **every** wrapped Codex spawn. Dropping it from the create arm too,
/// rather than only from the two update arms, is the same outcome with one fewer
/// code path: a created record without the key already reads as `enabled: true`,
/// so the write bought nothing, and this way the deck's record is identical in
/// shape to Codex's own.
fn upsert_trust_record(state: &mut toml_edit::Table, key: &str, hash: &str) {
    use toml_edit::{Item, Table, Value as TomlValue, value};

    match state.get_mut(key) {
        Some(Item::Table(existing)) => {
            existing.insert("trusted_hash", value(hash));
        }
        Some(Item::Value(TomlValue::InlineTable(existing))) => {
            existing.insert("trusted_hash", TomlValue::from(hash));
        }
        _ => {
            let mut record = Table::new();
            record.insert("trusted_hash", value(hash));
            state.insert(key, Item::Table(record));
        }
    }
}

// ---------------------------------------------------------------------------
// PRD #20 §4.2.1 — command-agnostic install + trust at daemon/TUI startup
// ---------------------------------------------------------------------------
//
// The deck must not care HOW codex is launched. Keying the install off the spawn
// command's basename misses every launcher form (`devbox run codex-big`,
// `run_codex.sh`, an alias), which is exactly why the dogfood `tester` role got
// ZERO integration. Following PRD #201's Pi precedent, the install+trust runs
// ONCE at startup, guarded on codex being present, so hook events reach the pane
// through the inherited `DOT_AGENT_DECK_PANE_ID` regardless of launch method.

/// Whether `codex` is discoverable as an executable regular file on the process
/// `PATH` — the self-guard for [`auto_install_and_trust_at_startup`], mirroring
/// `orchestrator_ext::pi_present_for_env`. A non-executable file named `codex`
/// does not count (it could never run), so a machine without Codex is a cheap
/// no-op.
fn codex_present_on_path() -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let candidate = dir.join("codex");
        let Ok(meta) = std::fs::metadata(&candidate) else {
            return false;
        };
        if !meta.is_file() {
            return false;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            meta.permissions().mode() & 0o111 != 0
        }
        #[cfg(not(unix))]
        {
            true
        }
    })
}

/// Startup entry (PRD #20 §4.2.1): install the deck's Codex hooks into the active
/// `CODEX_HOME` and record scoped trust for them, ONCE, command-agnostically.
///
/// Wired as `CODEX.startup_auto_install` (so the TUI runs it at launch, like
/// Claude's hooks and OpenCode's plugin) and from the `daemon serve` entry (so a
/// headless or lazy-spawned daemon does it too — mirroring how Pi's extension
/// materializes there). Because it never looks at a spawn command, Codex hooks
/// fire however Codex is launched — bare `codex`, an absolute path, or a launcher
/// like `devbox run codex-big` — with events reaching the pane through the
/// inherited `DOT_AGENT_DECK_PANE_ID`.
///
/// Guarded, idempotent, and best-effort: SKIPs unless `codex` is on `PATH` and a
/// real home resolves (never a `/tmp` write), and any failure is logged, never
/// fatal.
pub fn auto_install_and_trust_at_startup() {
    if !codex_present_on_path() {
        tracing::debug!("codex startup install: skipped (codex not on PATH)");
        return;
    }
    let Some(home) = codex_home() else {
        tracing::debug!("codex startup install: skipped (no CODEX_HOME/HOME)");
        return;
    };
    // No durable path means nothing was installed, so there is no command to
    // trust and no entry of ours in the listing. Fail closed rather than falling
    // back to a wider predicate (issue #730).
    let Some(binary_path) = auto_install() else {
        tracing::debug!("codex startup install: skipped trust (no durable binary path resolved)");
        return;
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| home.clone());
    match trust_deck_hooks_in(&home, &cwd, &binary_path) {
        Ok(outcome) => {
            // The `Unrecognised` case has already warned from inside; this path
            // has no user watching, so the count is all it needs.
            tracing::debug!(
                count = outcome.trusted(),
                "codex startup install: recorded scoped hook trust"
            )
        }
        Err(e) => tracing::warn!(
            "codex startup install: could not record scoped hook trust ({e}); Codex events \
             degrade to stdout classification"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Codex's hook commands are quoted for the HOST's shell — the half of #734
    /// that is a real behaviour change, and the half no test on a POSIX box can
    /// observe through the emitted string.
    ///
    /// Paired with `devin_hooks_manage`'s opposite assertion, this is what keeps
    /// the two writers from being collapsed into one answer: on Linux both
    /// spellings emit identical bytes, so setting this to `Posix` would revert
    /// #734 for the only writer that can reach Windows while every other test in
    /// the workspace stayed green.
    #[test]
    fn hook_commands_are_quoted_for_the_shell_codex_will_use() {
        assert_eq!(
            HOOK_SHELL,
            crate::agent_hook_config::HookShell::Native,
            "Codex runs its hooks through the host's own shell, so the quoting \
             must follow the host"
        );
    }

    #[test]
    fn install_writes_command_hooks_for_every_event() {
        let dir = tempfile::tempdir().expect("codex home tempdir");
        install_to(dir.path(), "/abs/dot-agent-deck").expect("install hooks.json");

        let contents =
            std::fs::read_to_string(dir.path().join("hooks.json")).expect("read hooks.json");
        let root: Value = serde_json::from_str(&contents).expect("parse hooks.json");
        let hooks = root
            .get("hooks")
            .and_then(Value::as_object)
            .expect("hooks object");

        for &event in CODEX_HOOK_EVENTS {
            let arr = hooks
                .get(event)
                .and_then(Value::as_array)
                .unwrap_or_else(|| panic!("event {event} present"));
            assert_eq!(arr.len(), 1, "one deck rule per event ({event})");
            let cmd = arr[0]["hooks"][0]["command"].as_str().expect("command str");
            assert_eq!(cmd, "/abs/dot-agent-deck hook --agent codex");
            assert_eq!(arr[0]["hooks"][0]["type"].as_str(), Some("command"));
        }
    }

    /// Write a real, executable file at `path` (creating its directory) and
    /// return its path as a string — a pin `pin_is_repairable` will call alive.
    fn seed_executable(path: &Path) -> String {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("create dir");
        std::fs::write(path, b"#!/bin/sh\nexit 0\n").expect("write seeded binary");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        path.to_str().expect("seeded path is UTF-8").to_string()
    }

    fn read_back(home: &Path) -> Value {
        let contents = std::fs::read_to_string(home.join("hooks.json")).expect("read hooks.json");
        serde_json::from_str(&contents).expect("parse hooks.json")
    }

    fn commands_for(root: &Value, event: &str) -> Vec<String> {
        root["hooks"][event]
            .as_array()
            .unwrap_or(&Vec::new())
            .iter()
            .flat_map(crate::agent_hook_config::rule_commands)
            .map(str::to_string)
            .collect()
    }

    /// The retired-event sweep clears THIS install's leftovers under an event
    /// `CODEX_HOOK_EVENTS` no longer carries — and reaches no further.
    /// `CODEX_HOOK_EVENTS` says what this deck writes, not what Codex runs
    /// (measured on 0.149.0: a `SessionEnd` command hook enumerates
    /// `enabled: true`), so a deck command under a retired event belonging to a
    /// different, still-valid install is live, not stale, and must survive
    /// (issue #730).
    #[test]
    fn retired_event_sweep_takes_only_this_installs_own_rules() {
        let fixture = crate::test_temp::tempdir().expect("codex fixture tempdir");
        let installing =
            seed_executable(&fixture.path().join("this-install").join("dot-agent-deck"));
        let other = seed_executable(&fixture.path().join("other-install").join("dot-agent-deck"));
        let dead = fixture
            .path()
            .join("pruned-worktree")
            .join("dot-agent-deck");
        assert!(!dead.exists(), "the dead path must genuinely not exist");
        let dead = dead.to_str().expect("dead path is UTF-8").to_string();

        let home = fixture.path().join("codex-home");
        std::fs::create_dir_all(&home).expect("create codex home");
        std::fs::write(
            home.join("hooks.json"),
            serde_json::to_vec_pretty(&json!({
                "hooks": {
                    "SessionEnd": [
                        { "hooks": [
                            { "type": "command", "command": expected_hook_command(&installing) },
                            { "type": "command", "command": expected_hook_command(&other) },
                            { "type": "command", "command": expected_hook_command(&dead) },
                            { "type": "command", "command": "/usr/local/bin/my-audit.sh" }
                        ] }
                    ]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        install_to(&home, &installing).expect("install over a retired-event rule");

        let root = read_back(&home);
        assert_eq!(
            commands_for(&root, "SessionEnd"),
            vec![
                expected_hook_command(&other),
                "/usr/local/bin/my-audit.sh".to_string()
            ],
            "our own leftover and a dead sibling pin go; another install's live \
             rule and the user's hook stay: {root:?}"
        );
    }

    /// Issue #730 / auditor S-3: `Ok(0)` has two causes that must not read
    /// alike. "Codex has nothing of ours" is the ordinary one and stays silent;
    /// "our own `hooks.json` carries our signature but Codex reports a command
    /// we do not recognise" is the one that turns full trust into zero trust
    /// with events silently degraded, and it must say so.
    #[test]
    fn a_zero_trust_write_warns_only_when_our_own_entry_was_unrecognisable() {
        use std::sync::{Arc, Mutex};

        #[derive(Clone, Default)]
        struct CapturedLog(Arc<Mutex<Vec<u8>>>);

        impl std::io::Write for CapturedLog {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLog {
            type Writer = CapturedLog;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        let home = tempfile::tempdir().expect("codex home tempdir");
        let entry = |command: &str, source: PathBuf| CodexHookEntry {
            key: format!("{command}:session_start:0:0"),
            command: command.to_string(),
            source_path: source,
            current_hash: "sha256:deadbeef".to_string(),
            trust_status: "untrusted".to_string(),
            is_managed: Some(false),
        };
        let ours = home.path().join("hooks.json");
        let expected = expected_hook_command("/abs/dot-agent-deck");

        let capture = |entries: &[CodexHookEntry]| -> String {
            let captured = CapturedLog::default();
            let subscriber = tracing_subscriber::fmt()
                .with_writer(captured.clone())
                .with_max_level(tracing_subscriber::filter::LevelFilter::WARN)
                .with_ansi(false)
                .finish();
            let guard = tracing::subscriber::set_default(subscriber);
            warn_if_our_own_entry_was_unrecognisable(entries, home.path(), &expected);
            drop(guard);
            String::from_utf8(captured.0.lock().unwrap().clone()).expect("captured log is UTF-8")
        };

        // Nothing listed at all — Codex is not installed, or has no hooks. Quiet.
        assert_eq!(capture(&[]), "", "an empty listing must stay silent");

        // Only a foreign file's hooks — none of ours to miss. Quiet.
        let elsewhere = capture(&[entry(&expected, PathBuf::from("/etc/codex/hooks.json"))]);
        assert_eq!(
            elsewhere, "",
            "an entry from someone else's file is not ours to miss: {elsewhere:?}"
        );

        // A hook in OUR file that does not carry our verb at all. Quiet.
        let unrelated = capture(&[entry("/usr/local/bin/my-audit.sh", ours.clone())]);
        assert_eq!(
            unrelated, "",
            "a user's own hook in our file is not ours to miss: {unrelated:?}"
        );

        // Our own file, our own signature, a command `Exact` could not match —
        // the "we installed and could not recognise our own entry" case. The
        // reported command carries a sentinel in the one position `Signature`
        // leaves free (everything before the suffix), which is what makes the
        // non-disclosure assertion below meaningful rather than incidental.
        let secret = "s3cret-token-that-must-not-reach-the-log";
        let mangled =
            format!("/opt/wrapper --token={secret} /abs/dot-agent-deck hook --agent codex");
        assert!(
            command_is_deck_owned(&mangled) && mangled != expected,
            "the fixture must satisfy Signature and miss Exact, or the arm is vacuous"
        );
        let warned = capture(&[entry(&mangled, ours)]);
        assert!(
            warned.contains("WARN") && warned.contains("none matches the command this install"),
            "a deck-signature entry in our own file that Exact missed must warn: {warned:?}"
        );
        // Issue #730, auditor N-E: the reported command is DESCRIBED, never
        // copied. `deck.log` is a file users attach to bug reports, and
        // everything before the deck's verb in a hook line is arbitrary text
        // from their own `hooks.json`.
        assert!(
            !warned.contains(secret) && !warned.contains(&mangled),
            "the reported command must not be copied into the log: {warned:?}"
        );
        assert!(
            warned.contains(&format!("reported_len={}", mangled.len()))
                && warned.contains("reported_agreeing_prefix="),
            "the warning must still say how far the two commands diverge: {warned:?}"
        );
    }

    /// Issue #730 / auditor item 1: `enabled` in `[hooks.state."<key>"]` is a
    /// USER knob (Codex's `/hooks` browser toggles it, and `toggle` is a
    /// separate action from `trust`), it is orthogonal to trust, and an absent
    /// key reads as `true`. A trust write must therefore touch `trusted_hash`
    /// and nothing else, or every wrapped Codex spawn silently reverts a
    /// deliberate "turn this hook off".
    ///
    /// Both update arms are covered because the record shape is the USER's
    /// choice, not ours: a hand-edited `config.toml` may spell it either as a
    /// `[hooks.state."k"]` table or as an inline `"k" = { … }`.
    #[test]
    fn a_trust_write_records_only_the_hash_and_leaves_a_users_enabled_alone() {
        let upsert = |original: &str, key: &str, hash: &str| -> String {
            let mut doc = original
                .parse::<toml_edit::DocumentMut>()
                .expect("fixture is valid TOML");
            let state = doc["hooks"]["state"]
                .as_table_mut()
                .expect("fixture has a [hooks.state] table");
            upsert_trust_record(state, key, hash);
            doc.to_string()
        };

        // Create arm: a brand-new record carries the hash alone. Codex reports
        // `enabled: true, trustStatus: trusted` for exactly this shape (0.149.0,
        // measured), so the key buys nothing and writing it would only be one
        // more place to revert a later toggle from.
        let created = upsert("[hooks.state]\n", "deck-key", "sha256:deck");
        assert!(
            created.contains("trusted_hash = \"sha256:deck\""),
            "a created record must carry Codex's own hash: {created:?}"
        );
        assert!(
            !created.contains("enabled"),
            "a created trust record must not write the user's `enabled` knob: {created:?}"
        );

        // Update arm, `Item::Table`: the user turned our hook off and we are
        // re-recording a fresh hash over a stale one. The hash moves, the
        // toggle does not.
        let table = upsert(
            "[hooks.state.\"deck-key\"]\nenabled = false\ntrusted_hash = \"sha256:stale\"\n",
            "deck-key",
            "sha256:fresh",
        );
        assert!(
            table.contains("enabled = false") && table.contains("trusted_hash = \"sha256:fresh\""),
            "a table-shaped record must keep `enabled = false` while the hash refreshes: \
             {table:?}"
        );

        // Update arm, `Value::InlineTable`: same claim, the other spelling.
        let inline = upsert(
            "[hooks.state]\n\"deck-key\" = { enabled = false, trusted_hash = \"sha256:stale\" }\n",
            "deck-key",
            "sha256:fresh",
        );
        assert!(
            inline.contains("enabled = false")
                && inline.contains("trusted_hash = \"sha256:fresh\""),
            "an inline-table record must keep `enabled = false` while the hash refreshes: \
             {inline:?}"
        );
    }

    #[test]
    fn reinstall_is_idempotent_and_preserves_user_hooks() {
        let dir = tempfile::tempdir().expect("codex home tempdir");
        // A pre-existing user hook the deck must never clobber.
        let user = json!({
            "hooks": {
                "PreToolUse": [
                    { "hooks": [ { "type": "command", "command": "/user/own-hook" } ] }
                ]
            }
        });
        std::fs::write(
            dir.path().join("hooks.json"),
            serde_json::to_string_pretty(&user).unwrap(),
        )
        .unwrap();

        install_to(dir.path(), "/abs/dot-agent-deck").expect("first install");
        install_to(dir.path(), "/abs/dot-agent-deck").expect("second install");

        let contents = std::fs::read_to_string(dir.path().join("hooks.json")).unwrap();
        let root: Value = serde_json::from_str(&contents).unwrap();
        let deck_rules = |event: &str| {
            root["hooks"][event]
                .as_array()
                .unwrap_or_else(|| panic!("{event} array"))
                .iter()
                .filter(|r| crate::agent_hook_config::rule_commands(r).any(command_is_deck_owned))
                .count()
        };
        let pre = root["hooks"]["PreToolUse"]
            .as_array()
            .expect("PreToolUse array");
        // The user's hook survives; the deck's is present exactly once (no dupes).
        let user_rules = pre
            .iter()
            .filter(|r| r["hooks"][0]["command"] == json!("/user/own-hook"))
            .count();
        assert_eq!(user_rules, 1, "user hook preserved");
        // Issue #730 narrowed which deck rules a re-install may strip, so the
        // no-duplication property is now asserted across EVERY event rather than
        // the one that happened to carry the user's hook: a predicate that
        // stopped matching this binary's own rules would accumulate a second
        // copy per event on every launch, and one event cannot show that.
        for &event in CODEX_HOOK_EVENTS {
            assert_eq!(
                deck_rules(event),
                1,
                "deck hook present exactly once after re-install ({event})"
            );
        }
    }

    /// Issue #730, the uninstall half: `hooks uninstall --agent codex` must take
    /// the deck's command out of a rule object the user shares with it without
    /// taking the user's handler too. Ownership here stays WIDE (any deck
    /// install's command, by signature) — an uninstall's job is to remove the
    /// deck's rules wholesale — and only the granularity changes.
    #[test]
    fn uninstall_keeps_a_users_sibling_handler_in_a_shared_rule() {
        let dir = tempfile::tempdir().expect("codex home tempdir");
        install_to(dir.path(), "/abs/dot-agent-deck").expect("install");
        // Move the user's handler INTO the deck's own rule object, which is what
        // a user editing `hooks.json` by hand naturally produces.
        let path = dir.path().join("hooks.json");
        let mut root: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        root["hooks"]["PreToolUse"][0]["hooks"]
            .as_array_mut()
            .expect("deck rule handlers")
            .push(json!({ "type": "command", "command": "/usr/local/bin/my-critical-audit.sh" }));
        std::fs::write(&path, serde_json::to_string_pretty(&root).unwrap()).unwrap();

        uninstall_from(dir.path()).expect("uninstall");

        let root: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let pre = root["hooks"]["PreToolUse"]
            .as_array()
            .expect("the shared rule's event key must survive");
        assert_eq!(pre.len(), 1, "the shared rule must survive: {pre:?}");
        assert_eq!(
            pre[0]["hooks"].as_array().map(Vec::len),
            Some(1),
            "only the deck's command may be removed: {pre:?}"
        );
        assert_eq!(
            pre[0]["hooks"][0]["command"],
            json!("/usr/local/bin/my-critical-audit.sh")
        );
    }

    /// A malformed `hooks.json` is preserved at `hooks.json.bak` — and the copy
    /// must never be made THROUGH a symlink planted at that path.
    ///
    /// The backup destination is fully predictable, and `std::fs::write` follows
    /// a symlink, so a writer able to add an entry to `~/.codex` could point
    /// `hooks.json.bak` at any file it could write and have the deck fill that
    /// file with the malformed config's bytes (#731's second half). The victim
    /// here stands for that file.
    #[cfg(unix)]
    #[test]
    fn a_malformed_config_backup_does_not_follow_a_symlink_planted_at_the_backup_path() {
        let dir = crate::test_temp::tempdir().expect("codex home tempdir");
        let victim = dir.path().join("victim");
        std::fs::write(&victim, b"victim bytes").expect("seed victim");

        let malformed = "{ this is not json";
        std::fs::write(dir.path().join("hooks.json"), malformed).expect("seed hooks.json");
        let backup = dir.path().join("hooks.json.bak");
        std::os::unix::fs::symlink(&victim, &backup).expect("plant symlink");

        let err = install_to(dir.path(), "/abs/dot-agent-deck")
            .expect_err("a config we cannot parse must not be rewritten");
        assert_eq!(err.kind(), ErrorKind::InvalidData);

        assert_eq!(
            std::fs::read(&victim).expect("read victim"),
            b"victim bytes",
            "the backup was written through the planted symlink and overwrote the victim"
        );
        assert!(
            !std::fs::symlink_metadata(&backup)
                .expect("stat backup")
                .file_type()
                .is_symlink(),
            "the backup must be a real file, not the planted symlink"
        );
        assert_eq!(
            std::fs::read_to_string(&backup).expect("read backup"),
            malformed,
            "the user's bytes must still be preserved beside the original"
        );
    }

    /// The atomic publish must never widen the files it rewrites. `hooks.json`
    /// carries the deck's own hook commands, but `config.toml` is the user's
    /// real Codex config — model choice, auth references, hook-trust records and
    /// anything they hand-wrote — so a `File::create` default of 0644 (under a
    /// typical 022 umask) or 0664 (under 002) would expose it to every local
    /// account the first time the deck installed or trusted its hooks. Mirrors
    /// `devin_hooks_manage::tests::install_never_widens_config_permissions`
    /// (#360), for the copy that kept the bug (#382).
    #[cfg(unix)]
    #[test]
    fn install_and_trust_never_widen_config_permissions() {
        use std::os::unix::fs::PermissionsExt as _;

        let mode_of = |path: &Path| {
            std::fs::metadata(path)
                .unwrap_or_else(|e| panic!("stat {}: {e}", path.display()))
                .permissions()
                .mode()
                & 0o777
        };
        let restrict = |path: &Path| {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                .expect("restrict fixture to 0600");
        };
        let trust = |home: &Path| {
            edit_trust_state(home, |state| {
                upsert_trust_record(state, "deck-hook", "abc123");
            })
            .expect("record trust");
        };

        // Files the deck creates itself are owner-only, not umask-dependent.
        let fresh = crate::test_temp::tempdir().expect("codex home tempdir");
        install_to(fresh.path(), "/abs/dot-agent-deck").expect("install");
        trust(fresh.path());
        assert_eq!(
            mode_of(&fresh.path().join("hooks.json")),
            0o600,
            "a deck-created hooks.json must be owner-only"
        );
        assert_eq!(
            mode_of(&fresh.path().join(CONFIG_TOML)),
            0o600,
            "a deck-created config.toml must be owner-only"
        );

        // An existing owner-only Codex home stays owner-only across install,
        // trust and uninstall — all three go through the same atomic publish.
        let existing = crate::test_temp::tempdir().expect("codex home tempdir");
        let hooks = existing.path().join("hooks.json");
        let config = existing.path().join(CONFIG_TOML);
        std::fs::write(&hooks, b"{}").unwrap();
        std::fs::write(&config, b"model = \"gpt-5\"\n").unwrap();
        restrict(&hooks);
        restrict(&config);

        install_to(existing.path(), "/abs/dot-agent-deck").expect("install");
        trust(existing.path());
        assert_eq!(
            mode_of(&hooks),
            0o600,
            "install must not widen an owner-only hooks.json"
        );
        assert_eq!(
            mode_of(&config),
            0o600,
            "recording trust must not widen an owner-only config.toml"
        );

        uninstall_from(existing.path()).expect("uninstall");
        assert_eq!(
            mode_of(&hooks),
            0o600,
            "uninstall must not widen an owner-only hooks.json"
        );

        // The user's own config bytes survive the trust edit that preserved the
        // mode, so the assertion above is not passing over a clobbered file.
        let contents = std::fs::read_to_string(&config).expect("read config.toml");
        assert!(
            contents.contains("model = \"gpt-5\""),
            "trust edit must preserve unrelated config; got {contents}"
        );
    }

    /// A stray `id: 2` message must not swallow the `hooks/list` reply — issue
    /// #1033, driven from a synthetic stream so it needs no `codex` on the box.
    ///
    /// The stream is the exact sequence the defect mishandled: the `initialize`
    /// reply, then a server→client **request** carrying `id: 2` (a `method`, no
    /// `result`, no `error` — the id comes from the server's own counter, so a
    /// collision with ours is ordinary), then the genuine reply behind it. The
    /// old arm matched on the id alone and `return`ed, so the request was handed
    /// to `parse_hooks_list`, failed its `result` check, and ended the attempt
    /// with the real listing still unread.
    #[test]
    fn a_stray_id_2_request_does_not_swallow_the_hooks_list_reply() {
        let (tx, rx) = mpsc::channel::<String>();
        for line in [
            json!({"jsonrpc": "2.0", "id": 1, "result": {"userAgent": "codex"}}),
            json!({"jsonrpc": "2.0", "method": "codex/event", "params": {}}),
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "account/chatgptAuthTokens/refresh",
                "params": {"reason": "expired"}
            }),
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "result": {"data": [{
                    "cwd": "/w",
                    "warnings": [],
                    "errors": [],
                    "hooks": [{
                        "key": "/h/hooks.json:session_start:0:0",
                        "eventName": "session_start",
                        "command": "/abs/dot-agent-deck hook --agent codex",
                        "sourcePath": "/h/hooks.json",
                        "isManaged": false,
                        "enabled": true,
                        "currentHash": "abc123",
                        "trustStatus": "untrusted"
                    }]
                }]}
            }),
        ] {
            tx.send(line.to_string()).expect("queue a line");
        }
        drop(tx);

        let entries = read_hooks_list_reply(&rx, Instant::now() + HOOKS_LIST_TIMEOUT)
            .expect("the genuine hooks/list reply sits behind the stray request and must be read");
        assert_eq!(
            entries.len(),
            1,
            "the reply's single entry must survive a preceding stray id-2 request"
        );
        assert_eq!(entries[0].key, "/h/hooks.json:session_start:0:0");
        assert_eq!(entries[0].current_hash, "abc123");
    }

    /// The response test above must not be passing because the predicate demands
    /// a `result`: a JSON-RPC **error** reply is a response too, and skipping it
    /// would turn an immediate, named failure into a five-second timeout whose
    /// message names the wrong cause.
    #[test]
    fn an_id_2_error_reply_is_matched_rather_than_skipped() {
        let (tx, rx) = mpsc::channel::<String>();
        tx.send(
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "error": {"code": -32601, "message": "method not found"}
            })
            .to_string(),
        )
        .expect("queue a line");
        drop(tx);

        let err = read_hooks_list_reply(&rx, Instant::now() + HOOKS_LIST_TIMEOUT)
            .expect_err("an error reply must fail the call");
        let message = err.to_string();
        assert!(
            message.contains("hooks/list failed"),
            "an error reply must be reported as itself, not as a timeout; got {message}"
        );
    }
}
