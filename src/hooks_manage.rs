use std::io::{self, ErrorKind, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use serde_json::{Value, json};

const HOOK_TYPES: &[&str] = &[
    "SessionStart",
    "SessionEnd",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "Notification",
    "Stop",
    "PreCompact",
    "SubagentStart",
    "SubagentStop",
];

/// Claude Code's user settings file, in the location Claude itself uses:
/// `~/.claude/settings.json`.
///
/// PRD #163 M1: resolved through the platform seam rather than a raw `$HOME`
/// read, so that on Windows — where `$HOME` is normally unset — this finds
/// `%USERPROFILE%\.claude` instead of missing it entirely.
///
/// PRD #163 review: the seam function is
/// [`crate::platform::paths::home_dir_with_tmp_fallback`], *not* `home_dir`,
/// because the raw read this replaced fell back to `/tmp` when `$HOME` was unset.
/// Unix behavior is therefore byte-for-byte what it was — including the
/// `/tmp/.claude/settings.json` an unset `$HOME` resolves to.
fn settings_path() -> PathBuf {
    crate::platform::paths::home_dir_with_tmp_fallback()
        .join(".claude")
        .join("settings.json")
}

/// Serializes the whole read-modify-write of `settings.json` across concurrent
/// in-process callers — two panes launching Claude Code at once, each running
/// [`auto_install`]. Combined with [`write_atomic`]'s temp-file+`rename`
/// publish, this closes the concurrent-clobber and partial-write window on the
/// user's real `~/.claude/settings.json` (issue #534), mirroring
/// `codex_hooks_manage::INSTALL_LOCK` (`:87`) and its findings #1/M-2.
///
/// Every public entry point in this module takes it for the FULL span from the
/// read to the write, not just around the write: locking the write alone still
/// lets two callers read the same "before" state and have the second one
/// overwrite the first's rule with a stale copy.
///
/// What this does NOT close is the cross-PROCESS lost update — two deck
/// binaries at different paths starting at the same instant, or a deck racing a
/// human's editor save. That needs an advisory file lock; neither sibling
/// adapter has one either, and the atomic publish means the loser of such a
/// race loses a whole update rather than leaving a torn file behind.
static SETTINGS_LOCK: Mutex<()> = Mutex::new(());

/// Take [`SETTINGS_LOCK`], recovering from a poisoned mutex rather than
/// panicking: a previous caller panicking mid-install says nothing about
/// whether the settings file is usable now, and the read is re-done from disk
/// under the guard regardless.
fn lock_settings() -> MutexGuard<'static, ()> {
    SETTINGS_LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

/// Read `path` the STRICT way used by every install AND uninstall path. Only a
/// MISSING file (`ErrorKind::NotFound`) means empty — mirroring
/// `codex_hooks_manage::install_to`'s contract (`:290-316`). Malformed JSON is
/// backed up next to the original (`<path>.bak`, leaving the original bytes on
/// disk untouched) and returned as an `Err` so every caller skips the write
/// instead of silently collapsing the user's settings to `{}` — the old
/// behavior here mapped ANY parse error to an empty config, so a settings.json
/// invalidated by a single trailing comma came back with `model`, `env`, and
/// every `permissions` entry destroyed, while the run reported success.
///
/// Issue #522: this used to be the install path's reader only, with a
/// `read_settings_lenient` twin still serving [`uninstall`] and
/// [`uninstall_from`] — which called [`write_settings`] UNCONDITIONALLY on
/// whatever it returned, so an unparseable settings.json was truncated to `{}`
/// and rewritten on every uninstall run while the run reported success and
/// exited 0. The lenient reader is gone; there is one contract for both
/// directions now.
fn load_settings_or_refuse(path: &Path) -> io::Result<Value> {
    match std::fs::read(path) {
        Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
            Ok(value) => Ok(value),
            Err(parse_err) => {
                let backup = path.with_extension("json.bak");
                let _ = std::fs::write(&backup, &bytes);
                Err(io::Error::new(
                    ErrorKind::InvalidData,
                    format!(
                        "{} is not valid JSON (left unchanged, original preserved at {}): \
                         {parse_err}",
                        path.display(),
                        backup.display()
                    ),
                ))
            }
        },
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(json!({})),
        Err(e) => Err(e),
    }
}

/// Publish `settings` to `path`, atomically and without widening or redirecting
/// anything (issue #534). The caller is expected to already hold
/// [`lock_settings`].
///
/// The old body was `std::fs::write` — `open(O_TRUNC)` then `write`, over a file
/// two deck processes and a human editor all write. Three exposures followed,
/// and the second and third are what MANUFACTURE the malformed settings.json
/// that [`load_settings_or_refuse`] now refuses to clobber: Claude Code reading
/// inside the window between the truncate and the write sees an empty file; a
/// crash between them leaves one on disk permanently.
fn write_settings(path: &Path, settings: &Value) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        // `create_dir_all("")` is a documented no-op, so a bare relative
        // filename (whose parent is `""`) needs no special case here.
        std::fs::create_dir_all(parent)?;
    }
    refuse_symlinked_destination(path)?;
    let contents = serde_json::to_string_pretty(settings)?;
    write_atomic(path, contents.as_bytes())
}

/// Refuse when `path` is a symlink — typically `~/.claude/settings.json`
/// stowed into a dotfiles checkout.
///
/// There is no safe silent branch here. A `rename(2)` publish onto the link
/// path replaces the symlink with a regular file, orphaning the dotfiles copy
/// so the user's edits and the deck's silently diverge from that point on.
/// Resolving the link instead means writing through it to a path outside the
/// directory the deck was pointed at — the write-anywhere hazard a
/// same-directory publish exists to close, and the reason the temp file below
/// is never placed in a resolved target's directory. Refusing is the only
/// branch that destroys nothing: the error reaches the shell on an explicit
/// `hooks install` / `hooks uninstall`, and `deck.log` on the startup
/// auto-install (issue #91 covers making that startup refusal visible).
///
/// `symlink_metadata` does not follow the final component, so this is the one
/// stat that can see the link itself. A dangling symlink is caught too — it is
/// the same arrangement with the target not checked out yet.
fn refuse_symlinked_destination(path: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => Err(io::Error::new(
            ErrorKind::InvalidInput,
            format!(
                "{} is a symlink (left unchanged): the deck will not replace it with a \
                 regular file, nor write through it to a path outside {}. Point it at a \
                 real file, or edit the linked file's hooks by hand.",
                path.display(),
                path.parent().unwrap_or(Path::new(".")).display()
            ),
        )),
        _ => Ok(()),
    }
}

/// Write `bytes` to a temp file in `dest`'s OWN directory and `rename` it over
/// `dest`. Same directory means same filesystem, which is what makes the rename
/// atomic — a reader sees either the whole old file or the whole new one, never
/// a truncated one, and a crash leaves the old file intact with at most a stray
/// temp beside it.
///
/// Mode is taken from the destination, or 0600 when the file is new. This is
/// the same fix #360 applied to the Devin adapter and #382 then applied to the
/// Codex one — both now share it via [`crate::agent_hook_config::write_atomic`],
/// which this deliberately does not use (see that module's header: only this
/// copy publishes through `create_new`). It is load-bearing precisely BECAUSE
/// this switches to a rename: `File::create` applies
/// `0666 & !umask` — 0644 under a typical 022 umask, 0664 under 002 — and the
/// rename would then replace the destination with that wider file. An in-place
/// `std::fs::write` never had that exposure, so publishing atomically without
/// this would trade one defect for another over a file holding the user's `env`
/// and `permissions`.
fn write_atomic(dest: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = match dest.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let name = dest
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("settings.json");
    let tmp = dir.join(format!(".{name}.tmp.{}", std::process::id()));

    let mut file = match create_new(&tmp) {
        Ok(file) => file,
        // A leftover temp from a crashed run under this same pid — or anything
        // else squatting the name. `remove_file` unlinks a symlink rather than
        // following it, so taking the name this way cannot be turned into a
        // write to somewhere else.
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {
            std::fs::remove_file(&tmp)?;
            create_new(&tmp)?
        }
        Err(e) => return Err(e),
    };

    let published = (|| {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(dest)
                .map(|meta| meta.permissions().mode() & 0o777)
                .unwrap_or(0o600);
            file.set_permissions(std::fs::Permissions::from_mode(mode))?;
        }
        file.write_all(bytes)?;
        file.sync_all()
    })();

    drop(file);
    if let Err(e) = published.and_then(|()| std::fs::rename(&tmp, dest)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// `open(O_CREAT | O_EXCL | O_WRONLY)`: create the temp file or fail, never
/// truncate an existing one and never follow a symlink at that path.
fn create_new(path: &Path) -> io::Result<std::fs::File> {
    std::fs::File::options()
        .write(true)
        .create_new(true)
        .open(path)
}

/// The fixed command signature that identifies a deck-authored rule, mirroring
/// `codex_hooks_manage::HOOK_COMMAND_SUFFIX` (`:81`) and
/// `devin_hooks_manage::HOOK_COMMAND_SUFFIX` (`:70`). `--agent` defaults to
/// `CliAgent::ClaudeCode` (`src/main.rs:60-64`), so `<path> hook --agent
/// claude-code` is a valid invocation, behaviourally identical to the bare
/// `<path> hook` this used to write — writing the explicit form is what lets a
/// rule be identified by this SUFFIX alone, regardless of the executable path
/// (or its basename) preceding it.
const HOOK_COMMAND_SUFFIX: &str = "hook --agent claude-code";

/// The compiled crate's own binary name (`"dot-agent-deck"` — upstream and every
/// fork alike; the crate name itself is never renamed). Every hook rule written
/// before this fix carries exactly this as its executable's basename in the
/// LEGACY `<path> hook` shape — see [`is_legacy_deck_rule`].
const DEFAULT_BINARY_NAME: &str = env!("CARGO_PKG_NAME");

/// Build a rule object in the new hooks format:
/// `{ "hooks": [{"type": "command", "command": "..."}] }`
/// For Notification, adds a matcher for permission_prompt.
fn make_rule(binary_path: &str, hook_type: &str) -> Value {
    let command = format!(
        "{} {HOOK_COMMAND_SUFFIX}",
        shell_quote_if_needed(binary_path)
    );
    let command_obj = json!({
        "type": "command",
        "command": command
    });

    if hook_type == "Notification" {
        json!({
            "matcher": "permission_prompt",
            "hooks": [command_obj]
        })
    } else {
        json!({
            "hooks": [command_obj]
        })
    }
}

/// Single-quote `path` for a POSIX shell only when it contains a character
/// outside a conservative safe set; otherwise return it unchanged. Mirrors
/// `devin_hooks_manage::shell_quote_if_needed` (`:232-245`, tested by
/// `install_quotes_a_binary_path_with_spaces`, `:726-730`) — a binary path
/// containing whitespace (e.g. `/Applications/My Deck/dot-agent-deck`) written
/// unquoted splits into extra shell tokens and the command no longer parses to
/// the intended argv.
///
/// Claude Code invokes the hook command line via the platform's native shell —
/// `cmd.exe` on Windows, a different shell with different quoting rules — so
/// this is `#[cfg(unix)]` here; see the `#[cfg(windows)]` sibling below. POSIX
/// single quotes are not a `cmd.exe` quoting mechanism: `cmd.exe` treats `'`
/// as a literal character, so wrapping a Windows path in single quotes names a
/// file that does not exist rather than quoting it.
#[cfg(unix)]
fn shell_quote_if_needed(path: &str) -> String {
    fn is_safe(b: u8) -> bool {
        b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'/' | b'.' | b'_' | b'-' | b'+' | b'=' | b':' | b'@' | b'%' | b','
            )
    }
    if !path.is_empty() && path.bytes().all(is_safe) {
        path.to_string()
    } else {
        format!("'{}'", path.replace('\'', r"'\''"))
    }
}

/// Double-quote `path` for `cmd.exe` only when it contains a character outside
/// a conservative safe set; otherwise return it unchanged. `~` is NOT special
/// to `cmd.exe` (unlike POSIX, where it triggers home-directory expansion), so
/// it is in the safe set here — a real Windows temp path such as
/// `C:\Users\RUNNER~1\...\dot-agent-deck` needs no quoting at all.
///
/// `%` and `!` are excluded from the safe set, but excluding them does NOT
/// resolve them — the same species of over-claiming comment corrected as H3
/// on [`read_settings_lenient`] above. `cmd.exe` expands `%VAR%` even *inside*
/// double quotes, and `!VAR!` under delayed expansion; wrapping the path in
/// quotes here changes neither. What quoting actually buys is limited to
/// spaces and the other characters outside the safe set — a path containing a
/// literal `%` or `!` is written through with that character unresolved,
/// quoted or not.
#[cfg(windows)]
fn shell_quote_if_needed(path: &str) -> String {
    fn is_safe(b: u8) -> bool {
        b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'\\' | b'/' | b'.' | b'_' | b'-' | b'+' | b'=' | b':' | b'@' | b',' | b'~'
            )
    }
    if !path.is_empty() && path.bytes().all(is_safe) {
        path.to_string()
    } else {
        format!("\"{}\"", path.replace('"', "\\\""))
    }
}

/// Undo [`shell_quote_if_needed`]: strip a single- or double-quoted wrapper
/// and unescape it back to the raw path, or return `exe` unchanged if it was
/// never quoted. Tries BOTH quoting forms regardless of platform — not just
/// the one this platform's writer produces — so a settings file written on
/// one platform and read on another is not stranded, mirroring `_009`'s
/// "a historical unquoted rule must still be recognised" principle.
fn unquote_if_needed(exe: &str) -> std::borrow::Cow<'_, str> {
    if let Some(inner) = exe.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')) {
        return std::borrow::Cow::Owned(inner.replace(r"'\''", "'"));
    }
    if let Some(inner) = exe.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        return std::borrow::Cow::Owned(inner.replace("\\\"", "\""));
    }
    std::borrow::Cow::Borrowed(exe)
}

/// Ensure `settings["hooks"]` is an object and return a mutable reference to it.
fn ensure_hooks_object(settings: &mut Value) -> &mut serde_json::Map<String, Value> {
    let obj = settings
        .as_object_mut()
        .expect("settings must be an object");
    if !obj.contains_key("hooks") || !obj["hooks"].is_object() {
        obj.insert("hooks".into(), json!({}));
    }
    obj.get_mut("hooks").unwrap().as_object_mut().unwrap()
}

/// Ensure `hooks_obj[hook_type]` is an array and return a mutable reference.
fn ensure_hook_array<'a>(
    hooks_obj: &'a mut serde_json::Map<String, Value>,
    hook_type: &str,
) -> &'a mut Vec<Value> {
    if !hooks_obj.contains_key(hook_type) || !hooks_obj[hook_type].is_array() {
        hooks_obj.insert(hook_type.into(), json!([]));
    }
    hooks_obj
        .get_mut(hook_type)
        .unwrap()
        .as_array_mut()
        .unwrap()
}

/// What one [`install_impl`] pass did: which hook types got a fresh rule, which
/// were already current, and how many deck-owned commands it removed as stale.
///
/// `repaired` is what makes PRD #381 M4's self-heal *observable*, and it is not
/// cosmetic. `auto_install` returns early when nothing was installed — and a
/// settings file holding BOTH a dead deck rule and the current one lands in
/// exactly that state: the dead rule is pruned in memory, every hook type
/// reports `skipped`, and the repair is then dropped on the floor instead of
/// being written. Counting the prune separately is what gets it published, and
/// logged.
struct InstallOutcome {
    installed: Vec<&'static str>,
    skipped: Vec<&'static str>,
    /// Deck-owned commands removed as stale: a rule whose binary is positively
    /// gone ([`command_is_dead_deck`]), or any deck rule left under a hook type
    /// the deck no longer installs. Never a user-authored command — both
    /// predicates are gated on deck ownership first.
    repaired: usize,
}

fn install_impl(settings: &mut Value, binary_path: &str) -> InstallOutcome {
    let hooks_obj = ensure_hooks_object(settings);

    // Clean up deck entries for hook types no longer in HOOK_TYPES. These are
    // gone from HOOK_TYPES entirely, so any deck rule there is stale regardless
    // of which binary wrote it — use the generic, binary-agnostic predicate.
    let mut repaired = 0usize;
    let all_keys: Vec<String> = hooks_obj.keys().cloned().collect();
    for key in all_keys {
        if !HOOK_TYPES.contains(&key.as_str()) {
            if let Some(arr) = hooks_obj.get_mut(&key).and_then(|v| v.as_array_mut()) {
                repaired += strip_deck_commands(arr, command_is_ours);
            }
            // Remove the key entirely if the array is now empty
            if hooks_obj
                .get(&key)
                .and_then(|v| v.as_array())
                .is_some_and(|a| a.is_empty())
            {
                hooks_obj.remove(&key);
            }
        }
    }

    let mut installed = Vec::new();
    let mut skipped = Vec::new();

    for &hook_type in HOOK_TYPES {
        let rules = ensure_hook_array(hooks_obj, hook_type);

        // Prune STALE deck-owned rules sharing the installing binary's own
        // basename — the shape N worktree builds actually take: every
        // `target/debug/dot-agent-deck` is a distinct real path with the SAME
        // basename, so a rebuilt or removed worktree leaves a dead rule with
        // that basename behind, and a fresh install from a surviving worktree
        // is the natural point to drop it. Scoped narrowly two ways: (1) only
        // rules ALREADY identified as deck-owned by `rule_is_ours` — never a
        // general "delete anything pointing at a missing path" sweep, which
        // would delete a user's own hooks for tools that simply are not
        // installed right now (test 014's coexisting `nonexistent-tool` rule);
        // (2) only rules whose basename matches the CURRENTLY installing
        // binary's basename — a genuinely different-looking deck binary
        // installed under a fictional/not-yet-real path (as most of this
        // file's fixtures are) must not be swept up just because it happens
        // not to exist on disk (test 003 pins this: installing `/b/…` must
        // never prune `/a/…`'s unrelated rule).
        repaired += strip_deck_commands(rules, |cmd| command_is_dead_deck(cmd, binary_path));

        let expected = make_rule(binary_path, hook_type);

        let already_current = rules.iter().any(|rule| rule == &expected);

        // Normalize down to a single fresh rule, but only for THIS binary —
        // leave rules belonging to a genuinely different deck binary alone —
        // except a LEGACY rule under the historical default name, which always
        // migrates to whichever binary is currently installing.
        let removed = strip_deck_commands(rules, |cmd| command_matches_binary(cmd, binary_path));
        rules.push(expected);

        if already_current && removed == 1 {
            skipped.push(hook_type);
        } else {
            installed.push(hook_type);
        }
    }

    InstallOutcome {
        installed,
        skipped,
        repaired,
    }
}

/// Outcome of [`uninstall_impl`]: which hook types had at least one deck
/// command removed, and the total number of individual commands removed across
/// all of them. Reporting the actual count is what makes "matched nothing"
/// distinguishable from "removed some" — a message that always reads
/// "No dot-agent-deck hooks found to remove." is correct-sounding but silently
/// wrong the moment the matcher goes blind: it prints on every run whether or
/// not anything was actually there.
///
/// The count is of COMMANDS, not rule objects, since [`strip_deck_commands`]
/// removes one command at a time and a rule the user shares with the deck
/// survives with the deck's command taken out of it.
struct UninstallOutcome {
    hook_types: Vec<&'static str>,
    commands_removed: usize,
}

fn uninstall_impl(settings: &mut Value) -> UninstallOutcome {
    let hooks = match settings.get_mut("hooks").and_then(|h| h.as_object_mut()) {
        Some(h) => h,
        None => {
            return UninstallOutcome {
                hook_types: Vec::new(),
                commands_removed: 0,
            };
        }
    };

    let mut hook_types = Vec::new();
    let mut commands_removed = 0;

    for &hook_type in HOOK_TYPES {
        if let Some(arr) = hooks.get_mut(hook_type).and_then(|v| v.as_array_mut()) {
            let removed = strip_deck_commands(arr, command_is_ours);
            if removed > 0 {
                hook_types.push(hook_type);
                commands_removed += removed;
            }
        }
    }

    UninstallOutcome {
        hook_types,
        commands_removed,
    }
}

/// Remove every command matching `is_target` from `rules`, dropping a rule
/// object only once it carries no commands at all — the fix for issue #535.
///
/// A rule's `hooks` array is a LIST of commands sharing one matcher, so a user
/// who put their own hook and the deck's in the same rule object is doing a
/// normal thing. Removal used to be a `retain` over whole rules keyed on an
/// `any()` across that list, so one deck command anywhere in a rule deleted the
/// user's commands with it — measured in #535, where a user's
/// `/usr/local/bin/my-critical-audit.sh` disappeared on `hooks uninstall` and
/// nothing said so. Install had the identical granularity and is the more
/// frequent path, since `auto_install` runs unattended at every dashboard
/// startup.
///
/// Two deliberate conservatisms, both in the "never delete what we did not
/// write" direction:
///
/// - a rule NOTHING matched in is returned untouched, so an already-empty or
///   command-less rule object is never tidied away as a side effect;
/// - a rule is dropped only when [`rule_commands`] reports nothing left in it,
///   which keeps a rule alive on any command the deck does not claim, in either
///   JSON shape.
///
/// Returns the number of individual commands removed.
fn strip_deck_commands(rules: &mut Vec<Value>, mut is_target: impl FnMut(&str) -> bool) -> usize {
    let mut removed = 0usize;
    rules.retain_mut(|rule| {
        let before = removed;

        // Current shape: `{"hooks": [{"command": …}, …]}` — drop just the
        // matching command objects and leave the rest of the array, and the
        // rule's own `matcher`, exactly as the user wrote them.
        if let Some(hooks) = rule.get_mut("hooks").and_then(Value::as_array_mut) {
            let len = hooks.len();
            hooks.retain(|hook| {
                !hook
                    .get("command")
                    .and_then(Value::as_str)
                    .is_some_and(&mut is_target)
            });
            removed += len - hooks.len();
        }

        // Legacy flat shape: `{"command": …}` — the command IS the rule, so
        // there is nothing smaller to remove. Take the key out and let the
        // no-commands-left check below decide the rule's fate, rather than
        // assuming it carries nothing else.
        if rule
            .get("command")
            .and_then(Value::as_str)
            .is_some_and(&mut is_target)
        {
            if let Some(obj) = rule.as_object_mut() {
                obj.remove("command");
            }
            removed += 1;
        }

        if removed == before {
            return true;
        }
        rule_commands(rule).next().is_some()
    });
    removed
}

// --- Identify deck rules by command SUFFIX, not by basename ---
//
// The old matcher looked for the literal substring "dot-agent-deck" in a rule's
// command, which blinds it the moment the binary runs under any other filename
// (this fork's `worker-agent-deck`, or any other renamed build) — the rule it
// just wrote becomes invisible to the tool that wrote it. A second revision
// relocated the hardcoding into a basename FRAGMENT check instead, which failed
// destructively on a crate rename (a crate named `dot-x` derives fragment `"x"`,
// and uninstall then deletes unrelated user hooks) and could only migrate a
// legacy rule when the INSTALLING binary's own name looked deck-ish, which made
// a coexisting fork/upstream install silently delete each other's rules.
//
// The replacement identifies a rule by its command's exact SUFFIX,
// [`HOOK_COMMAND_SUFFIX`] — mirroring `codex_hooks_manage::command_is_deck_owned`
// (`:132-137`) and `devin_hooks_manage::command_is_deck_owned` (`:199-204`). No
// basename check is layered on top: the suffix `"hook --agent claude-code"` is
// specific enough on its own that an unrelated `mytool hook` or `git hook` (test
// 005) does not end with it, and a user hook that merely mentions the deck's
// name as an argument (test 004) does not either.
//
// The one basename check that remains is narrower and different in kind: a
// LEGACY rule (written before this fix, in the bare `<path> hook` shape with no
// `--agent` suffix) is recognised only when its own executable's basename is
// EXACTLY [`DEFAULT_BINARY_NAME`] — never a fragment, and never compared against
// the installing binary's name. See [`is_legacy_deck_rule`].

/// Parse `command` as `<executable> hook --agent claude-code` in the CURRENT
/// format, recovering the executable by parsing from the RIGHT
/// (`strip_suffix`), not by counting whitespace-split tokens — so a quoted (or,
/// historically, unquoted) executable path containing spaces still round-trips
/// (test 008). The returned token may still be shell-quoted; pass it through
/// [`unquote_if_needed`] before comparing it as a path.
fn current_format_executable(command: &str) -> Option<&str> {
    let exe = command.trim_end().strip_suffix(HOOK_COMMAND_SUFFIX)?;
    let exe = exe.strip_suffix(' ')?;
    if exe.is_empty() { None } else { Some(exe) }
}

/// Parse `command` as the LEGACY, pre-fix `<executable> hook` shape — no
/// `--agent` suffix, never quoted — recovering the executable the same
/// parse-from-the-right way as [`current_format_executable`], so a historical
/// unquoted spaced path (test 009) is still recoverable even though counting
/// whitespace-split tokens could not locate its executable.
fn legacy_format_executable(command: &str) -> Option<&str> {
    let exe = command.trim_end().strip_suffix("hook")?;
    let exe = exe.strip_suffix(' ')?;
    if exe.is_empty() { None } else { Some(exe) }
}

/// Whether two executable FILE NAMES name the same binary, judged by the host
/// platform's own conventions rather than by byte equality.
///
/// **Unix: byte equality, unchanged.** [`std::env::consts::EXE_SUFFIX`] is
/// empty, so [`strip_suffix_ignoring_ascii_case`] is a literal no-op and the
/// comparison stays exact and case-sensitive. `foo.exe` on Unix is a genuinely different file
/// name from `foo` and this must keep saying so — which is why the suffix is
/// taken from `EXE_SUFFIX` and never hardcoded as `".exe"`.
///
/// **Windows: the suffix and the case are not part of a program's identity.**
/// `dot-agent-deck` and `dot-agent-deck.exe` are the same binary — that is
/// precisely what `PATHEXT` resolution means — and the filesystem is
/// case-insensitive, so `Dot-Agent-Deck.EXE` is that same binary again.
///
/// Review finding H2 introduced this convention for [`is_legacy_deck_rule`]
/// alone. PR #733's `build-windows` run then showed [`command_is_dead_deck`]
/// needed it too and had silently missed it: [`durable_binary_path`] always
/// resolves a name carrying `EXE_SUFFIX`, while [`DEFAULT_BINARY_NAME`] — the
/// literal the pre-fix code wrote as its fallback, on Windows as much as
/// anywhere — never does, so comparing raw basenames could never recognise a
/// legacy Windows pin as ours to repair and issue #536 stayed open on that
/// platform. Both call sites now share this one helper, so the convention
/// cannot drift apart again.
///
/// [`durable_binary_path`]: crate::platform::paths::durable_binary_path
fn binary_names_match(a: &str, b: &str) -> bool {
    binary_names_match_under(a, b, std::env::consts::EXE_SUFFIX, cfg!(windows))
}

/// [`binary_names_match`] with the host's two conventions injected instead of
/// read from the target: the executable suffix, and whether file names are
/// case-insensitive.
///
/// Split out **so the arithmetic is testable on any platform**, which is not a
/// stylistic preference here. PR #733's defect was Windows-only, could not be
/// reproduced on the machine that had to fix it (`aws-lc-sys` does not
/// cross-compile), and a `cfg!(windows)` branch covered by no test that runs
/// where its author works is precisely how the first one shipped green.
/// Passing `("", false)` reproduces every Unix exactly — an empty suffix makes
/// [`strip_suffix_ignoring_ascii_case`] the identity, leaving plain `==`.
fn binary_names_match_under(a: &str, b: &str, exe_suffix: &str, case_insensitive: bool) -> bool {
    let a = strip_suffix_ignoring_ascii_case(a, exe_suffix);
    let b = strip_suffix_ignoring_ascii_case(b, exe_suffix);
    if case_insensitive {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

/// `name` without one trailing `suffix`, matched case-insensitively because
/// Windows spells its executable suffix both `.exe` and `.EXE`. Returns `name`
/// untouched when `suffix` is empty (every Unix), when it is absent, and when
/// the name is nothing BUT the suffix — a file called `.exe` is a name in its
/// own right, not an empty one.
fn strip_suffix_ignoring_ascii_case<'a>(name: &'a str, suffix: &str) -> &'a str {
    if suffix.is_empty() {
        return name;
    }
    match name.len().checked_sub(suffix.len()) {
        // `is_char_boundary` is load-bearing, not defensive: a basename ending
        // in a multi-byte character can put `cut` inside one, and slicing
        // there panics.
        Some(cut)
            if cut > 0
                && name.is_char_boundary(cut)
                && name[cut..].eq_ignore_ascii_case(suffix) =>
        {
            &name[..cut]
        }
        _ => name,
    }
}

/// Whether `command` is a LEGACY deck rule: the bare `<executable> hook` shape,
/// where `executable`'s basename names [`DEFAULT_BINARY_NAME`] — the historical
/// default every rule was written under before this fix existed. Scoped to a
/// whole-basename match via [`binary_names_match`] (never a fragment, never the
/// installing binary's own name) so a user tool whose basename merely contains
/// "deck" (test 012) or ends in the literal word "hook" (test 005) is never
/// swept up.
///
/// Review finding H2: on Windows the installed binary carries the platform's
/// executable suffix (`dot-agent-deck.exe`), so every real legacy Windows rule
/// has a basename that never equals bare [`DEFAULT_BINARY_NAME`] byte for byte.
/// [`binary_names_match`] is what closes that gap, and is a no-op on Unix.
fn is_legacy_deck_rule(command: &str) -> bool {
    legacy_format_executable(command)
        .and_then(|exe| Path::new(exe).file_name())
        .and_then(|n| n.to_str())
        .is_some_and(|basename| binary_names_match(basename, DEFAULT_BINARY_NAME))
}

/// Whether `existing` and `installing` (both already unquoted) name the SAME
/// binary, so a rule for `existing` should be replaced rather than left
/// alongside a fresh rule for `installing`. Symlinks are resolved first — the
/// real-world case this exists for: a `dot-agent-deck` symlink pointing at a
/// renamed `worker-agent-deck` collapses to one rule. Every path here can fail
/// to resolve (most callers are test fixtures never written to disk), so
/// resolution failure falls back to a literal string comparison; this never
/// panics or unwraps on it.
fn executables_match(existing: &str, installing: &str) -> bool {
    if let (Ok(existing_real), Ok(installing_real)) = (
        Path::new(existing).canonicalize(),
        Path::new(installing).canonicalize(),
    ) {
        return existing_real == installing_real;
    }
    existing == installing
}

/// Every command string a rule carries, from either JSON shape: the current
/// nested `{"hooks": [{"command": ...}]}` or the legacy flat
/// `{"command": ...}`.
fn rule_commands(rule: &Value) -> impl Iterator<Item = &str> {
    let nested = rule
        .get("hooks")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|hook| hook.get("command").and_then(Value::as_str));
    let flat = rule.get("command").and_then(Value::as_str).into_iter();
    nested.chain(flat)
}

/// Whether `command` is a deck-owned hook command, generically — no specific
/// installing binary to compare against. True for any CURRENT-format command
/// (any executable, by the suffix alone) or any LEGACY-format command whose
/// executable is exactly [`DEFAULT_BINARY_NAME`].
fn command_is_ours(command: &str) -> bool {
    current_format_executable(command).is_some() || is_legacy_deck_rule(command)
}

/// Whether `command` is a deck-owned hook command that should be treated as
/// belonging to the SPECIFIC binary currently installing: either a
/// current-format command whose executable matches `binary_path`
/// ([`executables_match`]), or a legacy rule — which always migrates to
/// whichever binary is installing now, regardless of that binary's own name —
/// migration is keyed off the legacy RULE, never the installer.
fn command_matches_binary(command: &str, binary_path: &str) -> bool {
    if let Some(exe) = current_format_executable(command) {
        return executables_match(&unquote_if_needed(exe), binary_path);
    }
    is_legacy_deck_rule(command)
}

/// Whether `command` is a deck-owned command sharing `binary_path`'s own basename
/// — [`binary_names_match`], so the host platform's executable-suffix and
/// case conventions decide what "same basename" means, which is what took this
/// from "closed on Unix by the coincidence of an empty `EXE_SUFFIX`" to closed
/// everywhere — whose executable is POSITIVELY KNOWN not to be a usable
/// durable pin — see
/// [`crate::platform::paths::pin_is_repairable`] for what that means and for
/// the one case that still gets the benefit of the doubt.
/// `owned_command_executable` returns `None` for any command that is not
/// deck-owned by either shape, so this can never prune a user's own hook —
/// only a rule the deck itself would recognise as its own, and only when it
/// looks like a stale sibling of the binary currently installing (same
/// basename, different — now-dead — path). A deck rule for a genuinely
/// different-looking binary is left to
/// [`command_matches_binary`]/[`is_legacy_deck_rule`] instead, since most of
/// this file's own fixtures are fictional paths that were never on disk to
/// begin with and must not be swept up just because they don't exist.
///
/// Review finding: pruning on a bare `exists()` conflates "confirmed absent"
/// with "could not determine" — a working binary behind an unmounted volume,
/// or a path this process cannot `stat` (permissions), both make `exists()`
/// return `false` even though the executable is fine, and deleting a working
/// user's hook is worse than leaving a stale rule. That fail-safe direction
/// survives, and now lives in [`crate::platform::paths::pin_is_repairable`]
/// together with the rest of the "is this pin still usable" question.
///
/// **PRD #381 audit, MEDIUM-1: `try_exists` alone was not that question.** It
/// resolves a BARE `dot-agent-deck` — precisely what issue #536 describes the
/// old code writing — **relative to the process cwd**, so launching the deck
/// from any directory holding a file of that name made the bare pin look alive
/// and this returned `false`, leaving the rule in place beside the new durable
/// one. `/bin/sh` then resolves that persisted bare name through the AGENT's
/// `$PATH` at hook-fire time, which is #536's arbitrary-execution vector
/// surviving the fix that claims to close it. A relative path, a directory, a
/// non-executable file and a live `target/{debug,release}` path all passed the
/// same gate. [`crate::platform::paths::pin_is_repairable`] asks for the whole
/// invariant a freshly resolved path satisfies instead, and gives the
/// stat-error benefit of the doubt only to a well-formed absolute pin.
///
/// This does not fully resolve the underlying nondeterminism, and is not
/// meant to: it relocates the same class of problem one layer down rather
/// than removing it. The predicate still assumes `exe` is itself a bare
/// filesystem path; a command whose "executable" token is actually an
/// argv-prefixed wrapper invocation (not a literal path) will still report a
/// confident-looking "missing" for a string that was never a real path to
/// begin with, the same way [`executables_match`]'s `canonicalize` call can
/// already fail to resolve a path for reasons unrelated to the binary's
/// health. That gap is unchanged by this fix.
fn command_is_dead_deck(command: &str, binary_path: &str) -> bool {
    let Some(installing) = Path::new(binary_path)
        .file_name()
        .and_then(|name| name.to_str())
    else {
        // No basename to compare against (an empty or `..`-terminated
        // installing path, or a non-UTF-8 one). Fail safe: prune nothing. The
        // old `Option == Option` comparison treated two `None`s as a MATCH,
        // which is the one direction that deletes a user's rule off a value
        // nobody can reason about.
        return false;
    };
    owned_command_executable(command).is_some_and(|exe| {
        Path::new(&exe)
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|existing| binary_names_match(existing, installing))
            && crate::platform::paths::pin_is_repairable(&exe)
    })
}

/// The literal, unquoted executable path a deck-owned command names, or `None`
/// if `command` is not deck-owned by either shape. Used only to check whether
/// that binary still exists on disk.
fn owned_command_executable(command: &str) -> Option<String> {
    if let Some(exe) = current_format_executable(command) {
        return Some(unquote_if_needed(exe).into_owned());
    }
    if is_legacy_deck_rule(command) {
        return legacy_format_executable(command).map(str::to_string);
    }
    None
}

/// Silently install hooks if Claude Code is detected.
/// Intended for dashboard startup — **never prints to stdout**, including on
/// the PRD #381 refusal path: the dashboard is already painting by the time
/// this can fail, so a refusal goes to `tracing::warn!` and nowhere else.
pub fn auto_install() {
    auto_install_to(
        &settings_path(),
        crate::platform::paths::durable_binary_path,
    );
}

/// [`auto_install`] against an explicit settings path, with the binary-path
/// resolver injected — the seam PRD #381 M3 exists to open.
///
/// This function used to take the path alone and hardcode
/// `let binary_path = "dot-agent-deck".to_string();`. That single line is the
/// structural reason the defect shipped: it is the seam tests drive, so **no
/// test ever executed the `current_exe()` derivation that produced the bad
/// value**, and the PRD calls closing it the highest-value milestone here.
/// Production passes [`crate::platform::paths::durable_binary_path`]; a test
/// passes a closure driving `durable_binary_path_with` with a
/// `…/target/release/dot-agent-deck` `current_exe()` of its own and asserts
/// that value never reaches the file.
///
/// The resolver is called only after the settings *directory* check, so the
/// common "Claude Code not installed" case still costs one `exists()` and no
/// filesystem walk.
pub fn auto_install_to(path: &Path, resolve: impl FnOnce() -> Result<String, String>) {
    if path.parent().is_none_or(|p| !p.exists()) {
        return;
    }

    let binary_path = match resolve() {
        Ok(binary_path) => binary_path,
        // PRD #381 M6: no durable path means no write at all — not a bare
        // command name, not a build-artifact path that breaks later.
        Err(e) => {
            tracing::warn!("auto-install: {e}");
            return;
        }
    };

    let _guard = lock_settings();
    let mut settings = match load_settings_or_refuse(path) {
        Ok(settings) => settings,
        Err(e) => {
            tracing::warn!("auto-install: {e}");
            return;
        }
    };
    let outcome = install_impl(&mut settings, &binary_path);

    // A pass that only PRUNED (a dead deck rule sitting beside the current one)
    // installs nothing, and returning here on `installed.is_empty()` alone
    // would drop that repair instead of publishing it — PRD #381 M4.
    if outcome.installed.is_empty() && outcome.repaired == 0 {
        return;
    }

    if let Err(e) = write_settings(path, &settings) {
        tracing::warn!("auto-install: failed to write Claude Code hooks: {e}");
        return;
    }

    // Repair logs what it changed. Silently mutating global config is the same
    // class of thing that caused this bug, so a self-heal that leaves no trace
    // is not an acceptable fix for it.
    if outcome.repaired > 0 {
        tracing::info!(
            "repaired {} stale dot-agent-deck hook command(s) in {} (unusable binary pin or \
             retired hook type); now pinned to {binary_path}",
            outcome.repaired,
            path.display()
        );
    }
    if !outcome.installed.is_empty() {
        tracing::info!(
            "auto-installed Claude Code hooks: {}",
            outcome.installed.join(", ")
        );
    }
}

/// `dot-agent-deck hooks install --agent claude-code` — the explicit, chatty
/// install. Returns `Err` on a refused write (malformed settings.json left
/// unchanged, or the write itself failing) so the CLI dispatch in `main.rs`
/// reports failure on stderr AND exits non-zero, instead of a refusal
/// silently reporting success to the shell — see [`crate::agent_registry`]'s
/// `claude_install` adapter, which used to hardcode `Ok(())` here regardless
/// of outcome.
pub fn install() -> Result<(), String> {
    install_with(crate::platform::paths::durable_binary_path)
}

/// [`install`] with the binary-path resolution injected — the explicit-install
/// counterpart of the seam [`auto_install_to`] opens for the silent one.
///
/// PRD #381 M6: the resolution is the FIRST statement, so a refusal returns
/// before `settings_path()` is even computed. That is what makes "writes
/// nothing" a property of the code rather than a claim about it — no
/// truncation, no partial JSON, no created-then-abandoned file — and it is what
/// lets a test drive the refusal branch without a machine that genuinely has no
/// durable deck on it.
pub fn install_with(resolve: impl FnOnce() -> Result<String, String>) -> Result<(), String> {
    let binary_path = resolve()?;

    let path = settings_path();
    let _guard = lock_settings();
    let mut settings = load_settings_or_refuse(&path).map_err(|e| e.to_string())?;

    let InstallOutcome {
        installed, skipped, ..
    } = install_impl(&mut settings, &binary_path);

    write_settings(&path, &settings).map_err(|e| format!("writing {}: {e}", path.display()))?;

    if !installed.is_empty() {
        println!("Installed hooks: {}", installed.join(", "));
    }
    if !skipped.is_empty() {
        println!("Already installed (skipped): {}", skipped.join(", "));
    }
    println!("Settings file: {}", path.display());
    Ok(())
}

/// `dot-agent-deck hooks uninstall --agent claude-code`. Returns `Err` on a
/// refused read or a failed write, so the refusal reaches the shell — issue
/// #522, the uninstall half of #516. `agent_registry::claude_uninstall`
/// hardcoded `Ok(())` here regardless of outcome, so even a genuine write
/// failure could not reach the CLI's exit code; that is what #506 fixed for
/// `claude_install` alone.
pub fn uninstall() -> Result<(), String> {
    let path = settings_path();
    let _guard = lock_settings();
    let mut settings = load_settings_or_refuse(&path).map_err(|e| e.to_string())?;

    let outcome = uninstall_impl(&mut settings);

    if outcome.commands_removed == 0 {
        // Nothing of ours was in there, so there is nothing to publish. Writing
        // anyway would reserialize the whole file — reindenting and reordering
        // keys the deck does not own, and creating a `{}` settings.json on a
        // machine that never had one.
        println!("No dot-agent-deck hooks found to remove.");
    } else {
        write_settings(&path, &settings).map_err(|e| format!("writing {}: {e}", path.display()))?;
        println!(
            "Removed {} hook command{}: {}",
            outcome.commands_removed,
            if outcome.commands_removed == 1 {
                ""
            } else {
                "s"
            },
            outcome.hook_types.join(", ")
        );
    }
    println!("Settings file: {}", path.display());
    Ok(())
}

// --- Testable versions that accept a custom path ---

/// [`install`] against an explicit settings path. Returns the same refusals the
/// CLI path reports rather than swallowing them, so a test that expects a write
/// to be refused has to say so — a seam that quietly discarded the refusal is
/// how #522's uninstall defect stayed invisible under a green suite.
pub fn install_to(path: &Path, binary_path: &str) -> io::Result<()> {
    let _guard = lock_settings();
    let mut settings = load_settings_or_refuse(path)?;
    install_impl(&mut settings, binary_path);
    write_settings(path, &settings)
}

/// [`uninstall`] against an explicit settings path, with [`uninstall`]'s
/// contract: a file that cannot be read or parsed is left byte-for-byte as
/// found and reported as an error, and nothing is written when nothing of the
/// deck's was there.
pub fn uninstall_from(path: &Path) -> io::Result<()> {
    let _guard = lock_settings();
    let mut settings = load_settings_or_refuse(path)?;
    if uninstall_impl(&mut settings).commands_removed == 0 {
        return Ok(());
    }
    write_settings(path, &settings)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The conventions [`binary_names_match`] reads off the target on Windows,
    /// spelled out so they can be exercised from a Linux or macOS box — the
    /// only kind available while PR #733's Windows-only defect was being
    /// fixed, since `aws-lc-sys` does not cross-compile.
    const WINDOWS: (&str, bool) = (".exe", true);
    /// The same for every Unix: no executable suffix, case-sensitive names.
    const UNIX: (&str, bool) = ("", false);

    fn matches(a: &str, b: &str, (suffix, insensitive): (&str, bool)) -> bool {
        binary_names_match_under(a, b, suffix, insensitive)
    }

    /// The Windows defect PR #733's second round exists to close: the
    /// installing path always carries `EXE_SUFFIX`
    /// (`platform::paths::durable_binary_file_name` appends it) while
    /// `DEFAULT_BINARY_NAME` never does, so a legacy bare pin and the binary
    /// installing over it are the same program spelled two ways.
    #[test]
    fn binary_names_match_across_the_windows_executable_suffix() {
        assert!(matches("dot-agent-deck", "dot-agent-deck.exe", WINDOWS));
        assert!(matches("dot-agent-deck.exe", "dot-agent-deck", WINDOWS));
        assert!(matches("dot-agent-deck.exe", "dot-agent-deck.exe", WINDOWS));
        assert!(matches("dot-agent-deck", "dot-agent-deck", WINDOWS));
        // Windows file names are case-insensitive, so this is the same binary
        // again — the review-finding-H2 half of the convention.
        assert!(matches("Dot-Agent-Deck.EXE", "dot-agent-deck.exe", WINDOWS));
    }

    /// The inversion guard. On Unix `foo.exe` is a file called `foo.exe` and
    /// nothing more, so stripping a hardcoded `".exe"` (or applying the
    /// case-insensitivity everywhere) must not compile away this difference.
    #[test]
    fn binary_names_stay_exact_on_unix() {
        assert!(matches("dot-agent-deck", "dot-agent-deck", UNIX));
        assert!(
            !matches("dot-agent-deck", "dot-agent-deck.exe", UNIX),
            "`.exe` is not this platform's executable suffix, so these are two \
             different file names"
        );
        assert!(
            !matches("Dot-Agent-Deck", "dot-agent-deck", UNIX),
            "Unix file names are case-sensitive"
        );
    }

    /// The safety property the basename gate exists for, at the unit level:
    /// installing `/a/dot-agent-deck` must never prune `/b/some-other-name`.
    /// Neither convention may collapse two genuinely different names.
    #[test]
    fn binary_names_never_match_a_different_program() {
        for conventions in [WINDOWS, UNIX] {
            assert!(!matches("some-other-name", "dot-agent-deck", conventions));
            assert!(!matches(
                "some-other-name.exe",
                "dot-agent-deck.exe",
                conventions
            ));
            // A fragment is not a match, in either direction — the property
            // `is_legacy_deck_rule` is scoped to a whole basename for.
            assert!(!matches(
                "dot-agent-deck-shim",
                "dot-agent-deck",
                conventions
            ));
            assert!(!matches("deck", "dot-agent-deck", conventions));
            // Only ONE suffix comes off, so a doubled one is still distinct.
            assert!(!matches(
                "dot-agent-deck.exe.exe",
                "dot-agent-deck",
                conventions
            ));
        }
    }

    /// The **call site**, not just the helper: `command_is_dead_deck` must
    /// recognise a legacy BARE pin as ours to repair when the binary installing
    /// over it is the deck's own installed file name on THIS platform.
    ///
    /// This is the assertion that failed on `build-windows` in PR #733's first
    /// round and passed everywhere else, because
    /// `platform::paths::durable_binary_file_name` appends `EXE_SUFFIX` while
    /// `DEFAULT_BINARY_NAME` — the literal the pre-fix code wrote as its
    /// `current_exe()` fallback — never does. A Windows machine carrying such a
    /// pin could therefore never be self-healed, so issue #536 was open there
    /// while looking closed. On Unix it held only by the coincidence that
    /// `EXE_SUFFIX` is empty.
    #[test]
    fn a_bare_pin_is_dead_against_the_platforms_own_installed_file_name() {
        let installing = format!(
            "/opt/dot-agent-deck/bin/{DEFAULT_BINARY_NAME}{}",
            std::env::consts::EXE_SUFFIX
        );
        assert!(
            command_is_dead_deck(
                &format!("{DEFAULT_BINARY_NAME} {HOOK_COMMAND_SUFFIX}"),
                &installing
            ),
            "a bare `{DEFAULT_BINARY_NAME}` pin must be repairable when \
             installing `{installing}` — /bin/sh (or cmd.exe) resolves that \
             persisted bare name through the AGENT's PATH at hook-fire time"
        );
        // And the legacy `<path> hook` shape, which reaches the same gate
        // through `is_legacy_deck_rule` rather than the current-format parse.
        assert!(command_is_dead_deck(
            &format!("{DEFAULT_BINARY_NAME} hook"),
            &installing
        ));
    }

    /// The safety property the basename gate exists for, at its own call site
    /// and independently of the existence check: `pin_is_repairable` says
    /// "repairable" for every pin below on every platform — absent on Unix,
    /// and not absolute (no drive prefix) on Windows — so the basename
    /// comparison is the ONLY thing standing between them and deletion.
    /// Installing `/a/dot-agent-deck` must never prune `/b/some-other-name`'s
    /// rule.
    #[test]
    fn a_deck_rule_for_a_different_binary_is_never_dead_against_ours() {
        let installing = format!(
            "/opt/dot-agent-deck/bin/{DEFAULT_BINARY_NAME}{}",
            std::env::consts::EXE_SUFFIX
        );
        assert!(!command_is_dead_deck(
            &format!("/nowhere/some-other-name {HOOK_COMMAND_SUFFIX}"),
            &installing
        ));
        assert!(!command_is_dead_deck(
            &format!("/nowhere/{DEFAULT_BINARY_NAME}-shim {HOOK_COMMAND_SUFFIX}"),
            &installing
        ));
        // A user-authored command that merely MENTIONS the deck is not ours by
        // either shape, so it never reaches the basename gate at all.
        assert!(!command_is_dead_deck(
            "/opt/audit/wrapper --watch dot-agent-deck --report",
            &installing
        ));
    }

    /// A name that is nothing but the suffix keeps it: a file called `.exe` has
    /// that name, and stripping it to the empty string would make it match
    /// every other suffix-only name.
    #[test]
    fn a_suffix_only_name_is_not_stripped_to_nothing() {
        assert_eq!(strip_suffix_ignoring_ascii_case(".exe", ".exe"), ".exe");
        assert!(!matches(".exe", "", WINDOWS));
    }

    /// `strip_suffix_ignoring_ascii_case` slices by byte offset, so a basename
    /// ending in a multi-byte character can put the cut inside one. Slicing
    /// there panics, and a hook basename is arbitrary user-supplied text.
    #[test]
    fn stripping_never_panics_on_a_multibyte_boundary() {
        // 5 bytes, and `len - ".exe".len()` == 1 lands inside the first `é`.
        assert_eq!(strip_suffix_ignoring_ascii_case("ééa", ".exe"), "ééa");
        assert_eq!(strip_suffix_ignoring_ascii_case("é", ".exe"), "é");
        assert_eq!(strip_suffix_ignoring_ascii_case("", ".exe"), "");
        assert_eq!(strip_suffix_ignoring_ascii_case("déjà.EXE", ".exe"), "déjà");
    }

    /// The whole predicate, on the real host: whatever this platform's
    /// conventions are, the deck's own installed file name and
    /// `DEFAULT_BINARY_NAME` must name the same binary — that identity is what
    /// lets self-heal recognise a legacy pin as ours to repair, and it is the
    /// one assertion here that would have failed on `build-windows` before this
    /// fix.
    #[test]
    fn the_installed_file_name_and_the_default_name_are_the_same_binary() {
        let installed = format!("{DEFAULT_BINARY_NAME}{}", std::env::consts::EXE_SUFFIX);
        assert!(binary_names_match(DEFAULT_BINARY_NAME, &installed));
        assert!(binary_names_match(&installed, DEFAULT_BINARY_NAME));
        assert!(!binary_names_match("some-other-name", &installed));
    }
}
