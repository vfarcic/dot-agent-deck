//! PRD #386 M1/M2 — the descendant-process scan the shell-activity signal is
//! built on: a process table, a cycle-safe descendant walk over it, and the
//! **structural discriminator** that classifies a pane as busy or idle.
//!
//! Everything here is pure data and compiles on every platform. Only the act of
//! *sampling* the machine is platform code: [`super::process_table`] is
//! implemented in `unix.rs` and is an unconditional `None` in `windows.rs`,
//! matching `foreground_pgid`'s existing contract.
//!
//! ## Why the discriminator is structural
//!
//! A Claude pane always has long-lived children (`npm exec
//! @upstash/context7-mcp`, `engram mcp`, `caffeinate -i -t 300`, …), so a naive
//! "has descendants" test is `true` 100% of the time and would pin every pane
//! at `Working` forever. The measurement behind PRD #386
//! (2026-08-06, Claude Code 2.1.220) found the separation is structural rather
//! than textual: Claude Code `setsid`-detaches its Bash-tool child into a POSIX
//! session of its own, while **every** other child of the agent stays in the
//! agent's session on the pane's tty. So a pane is busy iff the agent has a
//! transitive descendant whose session id differs from the agent's own.
//!
//! ## The CI trap this must never fall into
//!
//! "The descendant has no controlling terminal" looks like the same test on a
//! developer machine and is **vacuous in a container**, where the agent itself
//! has no controlling terminal either — every descendant matches and the pane
//! pins at `Working` forever. [`ProcessInfo::has_controlling_tty`] is therefore
//! recorded as corroborating evidence only; [`descendant_shell_activity`]
//! compares session ids against **the agent's own** and never reads that field.

use std::collections::{HashMap, HashSet, VecDeque};

/// One row of the process table: the facts the descendant scan needs about a
/// single process.
///
/// `session_id` is the POSIX session id as reported by `getsid(2)` — **not** by
/// `ps -o sess=`, which prints `0` for a non-root caller on macOS and is
/// useless here. A non-positive value means the session id could not be read
/// (the process exited between the sample and the `getsid` call), and
/// [`descendant_shell_activity`] treats such a row as unclassifiable rather
/// than as "different session".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessInfo {
    /// Process id.
    pub pid: i32,
    /// Parent process id, as sampled. A table sampled non-atomically can
    /// contain a cycle after PID reuse — see [`descendants`].
    pub ppid: i32,
    /// POSIX session id (`getsid(2)`). Non-positive when it could not be read.
    pub session_id: i32,
    /// Whether the process has a controlling terminal. **Corroborating only** —
    /// see this module's docs for why it is never sufficient on its own.
    pub has_controlling_tty: bool,
    /// Whether the process leads its own session (`getsid(pid) == pid`).
    pub session_leader: bool,
    /// The full command line, as one string. Callers must substring-match
    /// inside it and must never tokenise it: Claude Code's Bash-tool child has
    /// `argc == 3`, with the whole prologue plus the user's command in a single
    /// argv element.
    pub argv: String,
}

/// A measured shell-tool argv fingerprint for one agent kind — the **secondary
/// cross-check**, never the primary test (PRD #386, Open Question 2).
///
/// It exists because the structural test and the argv test fail on *disjoint*
/// sets: the structural test dies if Claude Code stops `setsid`-ing its Bash
/// child and false-positives on an MCP server that detaches itself, neither of
/// which touches the argv; the argv test dies on prologue rewording,
/// `CLAUDE_CODE_SHELL_PREFIX`, sandbox mode, and the missing-snapshot variant,
/// none of which touches the session id.
///
/// It is **data, not an inlined literal**, so an agent whose shell-tool shape
/// has never been measured simply gets no cross-check (callers pass an empty
/// slice) rather than a fingerprint invented for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShellToolShape {
    /// The agent kind this shape was measured against, for diagnostics.
    pub agent: &'static str,
    /// Alternative fingerprints. The argv matches this shape when **any**
    /// alternative matches, and an alternative matches when **every** substring
    /// in it occurs in the command line.
    pub alternatives: &'static [&'static [&'static str]],
}

impl ShellToolShape {
    /// Whether `argv` (a whole command line, never tokenised) carries this
    /// shape. An alternative with no substrings never matches — an empty
    /// fingerprint would match every process on the machine.
    pub fn matches(&self, argv: &str) -> bool {
        self.alternatives
            .iter()
            .any(|alt| !alt.is_empty() && alt.iter().all(|needle| argv.contains(needle)))
    }
}

/// Claude Code's Bash-tool command line, in the narrowest form that survived
/// the 2026-08-06 measurement against Claude Code 2.1.220.
///
/// Two alternatives, because the snapshot `source` segment is absent from the
/// no-snapshot variant entirely:
/// - the usual shape — `shell-snapshots/snapshot-` **and** `&& eval `;
/// - the unalias prologue, which survives when the snapshot segment does not.
///
/// Predicates that were measured and rejected are recorded in the PRD, not
/// here: `argv[0] == "/bin/zsh"` (the interpreter follows `CLAUDE_CODE_SHELL` →
/// `$SHELL`), `setopt NO_EXTENDED_GLOB` (zsh-only), `.claude/shell-snapshots`
/// (breaks under `CLAUDE_CONFIG_DIR`), and any form of argv tokenising.
pub const CLAUDE_BASH_TOOL_SHAPE: ShellToolShape = ShellToolShape {
    agent: "claude",
    alternatives: &[
        &["shell-snapshots/snapshot-", "&& eval "],
        &[r"\builtin unalias -- 'unsetenv'"],
    ],
};

/// Every shell-tool argv shape this project has actually **measured**, and
/// nothing else (PRD #386 M3, Open Question 2).
///
/// This is the catalog the daemon's poll loop hands to
/// [`crate::agent_pty::AgentPtyRegistry::shell_foreground_busy_snapshot`], which
/// selects from it **per pane** by agent kind. An agent whose shell-tool shape
/// has never been measured must select nothing from it and fall back to the
/// structural test alone — handing it a fingerprint measured against a
/// *different* product would let the cross-check veto a genuinely detached
/// descendant, and the resulting false negative is silent (the PRD's "the
/// failure mode to watch for is silence, not noise").
///
/// Adding an entry here is therefore a claim that the shape was measured
/// against a live agent of that kind, not that it looks plausible.
pub const MEASURED_SHELL_TOOL_SHAPES: &[ShellToolShape] = &[CLAUDE_BASH_TOOL_SHAPE];

/// Every transitive descendant of `root_pid` in `table`, each reported exactly
/// once and never including `root_pid` itself.
///
/// The walk carries a visited set because it **must terminate on a cycle**: a
/// `ppid` table sampled non-atomically can loop back into a branch it just
/// descended after PID reuse, and a naive walk would spin forever inside the
/// daemon's poll task.
pub fn descendants(table: &[ProcessInfo], root_pid: i32) -> Vec<&ProcessInfo> {
    let mut children: HashMap<i32, Vec<&ProcessInfo>> = HashMap::new();
    for row in table {
        children.entry(row.ppid).or_default().push(row);
    }

    let mut visited: HashSet<i32> = HashSet::new();
    visited.insert(root_pid);

    let mut queue: VecDeque<i32> = VecDeque::new();
    queue.push_back(root_pid);

    let mut out: Vec<&ProcessInfo> = Vec::new();
    while let Some(pid) = queue.pop_front() {
        let Some(kids) = children.get(&pid) else {
            continue;
        };
        for kid in kids {
            if !visited.insert(kid.pid) {
                continue;
            }
            out.push(kid);
            queue.push_back(kid.pid);
        }
    }
    out
}

/// Classify a pane as busy (`Some(true)`) or idle (`Some(false)`) from a
/// sampled process table, or `None` when the table cannot answer.
///
/// A pane is **busy** iff `root_pid` has a transitive descendant that is in a
/// different POSIX session than `root_pid` itself — the load-bearing condition,
/// one `getsid` comparison per candidate, immune to any change in what an agent
/// puts on its command line.
///
/// `shapes` is the optional argv cross-check. When it is empty the structural
/// test stands alone (which is what the measurement says already excludes every
/// observed confounder); when it is non-empty a candidate must additionally
/// carry one of the shapes, so a caller can require confirmation for the one
/// agent whose shell-tool shape has actually been measured while leaving the
/// signal purely structural for the rest.
///
/// `None` means "no answer available": `root_pid` is not in the table (it
/// exited, or the table was sampled from another PID namespace), or its own
/// session id could not be read. `None` is deliberately not folded into
/// `Some(false)` — the caller must be able to leave a pane's status alone
/// rather than assert it is idle.
///
/// Note what this does **not** consult: [`ProcessInfo::has_controlling_tty`].
/// A bare no-controlling-terminal test collapses in a container, where the
/// agent has no terminal either; see this module's docs.
pub fn descendant_shell_activity(
    table: &[ProcessInfo],
    root_pid: i32,
    shapes: &[ShellToolShape],
) -> Option<bool> {
    let root = table.iter().find(|row| row.pid == root_pid)?;
    if root.session_id <= 0 {
        return None;
    }

    for candidate in descendants(table, root_pid) {
        // A row whose session id could not be read is unclassifiable, not
        // "different" — counting it as different would turn an exit racing the
        // sample into a false `Working`.
        if candidate.session_id <= 0 || candidate.session_id == root.session_id {
            continue;
        }
        if !shapes.is_empty() && !shapes.iter().any(|shape| shape.matches(&candidate.argv)) {
            continue;
        }
        return Some(true);
    }
    Some(false)
}

/// Whether a `ps` TTY column names a real terminal. macOS prints `??` for a
/// process with no controlling terminal, Linux's procps prints `?`, and `-`
/// turns up in some `ps` implementations; anything else is a terminal name.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn tty_field_names_a_terminal(tty: &str) -> bool {
    !matches!(tty, "" | "?" | "??" | "-")
}

/// Parse the output of `ps -A -w -w -o pid=,ppid=,tty=,args=` into a table,
/// resolving each row's POSIX session id through `session_id_of`.
///
/// Lives here rather than in `unix.rs` because it is pure string work and is
/// worth unit-testing on every platform, `ps` being the one parsing surface
/// Route A trades a native dependency for. Unparseable lines are skipped rather
/// than failing the whole sample: one odd row must not blind the poll to the
/// rest of the machine.
///
/// `session_leader` is derived from the session id (`getsid(pid) == pid`)
/// rather than from `ps`'s STAT letters, which is both exact and one fewer
/// column of `ps` formatting to depend on.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn parse_ps_table(stdout: &str, session_id_of: &dyn Fn(i32) -> i32) -> Vec<ProcessInfo> {
    let mut rows = Vec::new();
    for line in stdout.lines() {
        let Some((pid, ppid, tty, argv)) = split_ps_row(line) else {
            continue;
        };
        let session_id = session_id_of(pid);
        rows.push(ProcessInfo {
            pid,
            ppid,
            session_id,
            has_controlling_tty: tty_field_names_a_terminal(tty),
            session_leader: session_id > 0 && session_id == pid,
            argv: argv.to_string(),
        });
    }
    rows
}

/// Split one `ps` row into `(pid, ppid, tty, argv)`. The first three columns
/// are whitespace-free tokens; everything after them is the command line,
/// **kept whole** — the argv must never be tokenised.
fn split_ps_row(line: &str) -> Option<(i32, i32, &str, &str)> {
    let (pid, rest) = next_token(line)?;
    let (ppid, rest) = next_token(rest)?;
    let (tty, rest) = next_token(rest)?;
    Some((
        pid.parse().ok()?,
        ppid.parse().ok()?,
        tty,
        rest.trim_start(),
    ))
}

fn next_token(s: &str) -> Option<(&str, &str)> {
    let s = s.trim_start();
    if s.is_empty() {
        return None;
    }
    Some(match s.find(char::is_whitespace) {
        Some(end) => (&s[..end], &s[end..]),
        None => (s, ""),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(pid: i32, ppid: i32, session_id: i32, argv: &str) -> ProcessInfo {
        ProcessInfo {
            pid,
            ppid,
            session_id,
            has_controlling_tty: true,
            session_leader: session_id == pid,
            argv: argv.to_string(),
        }
    }

    /// PRD #386 M2, Open Question 2 — the argv cross-check is a *secondary*
    /// confirmation, so a candidate that is structurally busy must still be
    /// vetoed when a shape is supplied and does not match, and admitted when it
    /// does. `tests/shell_activity.rs` (`status/shell-activity/003`) pins the
    /// structural half with the cross-check disabled; this pins the half that
    /// switches on.
    #[test]
    fn the_argv_cross_check_vetoes_a_structurally_busy_descendant_that_does_not_match() {
        let table = vec![
            row(100, 1, 100, "claude --model opus"),
            row(200, 100, 250, "some-unmeasured-detached-thing"),
        ];
        assert_eq!(
            descendant_shell_activity(&table, 100, &[]),
            Some(true),
            "with no shapes the structural test alone must classify the detached descendant as busy"
        );
        assert_eq!(
            descendant_shell_activity(&table, 100, &[CLAUDE_BASH_TOOL_SHAPE]),
            Some(false),
            "a supplied shape the descendant does not carry must veto the structural match"
        );
    }

    /// The two measured Claude Bash-tool variants: the usual one carrying the
    /// shell-snapshot `source` prologue, and the no-snapshot one where that
    /// segment is absent from the command string entirely.
    #[test]
    fn the_claude_shape_matches_both_measured_bash_tool_variants() {
        let with_snapshot = "/bin/zsh -c source /Users/x/.claude/shell-snapshots/snapshot-zsh-1785985362201-zal44i.sh 2>/dev/null || true && eval 'ping -c 200 127.0.0.1'";
        let without_snapshot = "/bin/zsh -c -l { \\builtin unalias -- 'unsetenv'; } >/dev/null 2>&1 || true && eval 'ping -c 200 127.0.0.1'";
        assert!(CLAUDE_BASH_TOOL_SHAPE.matches(with_snapshot));
        assert!(CLAUDE_BASH_TOOL_SHAPE.matches(without_snapshot));
        assert!(!CLAUDE_BASH_TOOL_SHAPE.matches("npm exec @upstash/context7-mcp"));
        assert!(
            !CLAUDE_BASH_TOOL_SHAPE.matches(""),
            "an empty command line must never match — the cross-check has to be evidence"
        );
    }

    /// `None` is not `Some(false)`: a pid the table does not contain, or one
    /// whose own session id could not be read, means "no answer", so the caller
    /// can leave the pane's status alone instead of asserting it is idle.
    #[test]
    fn an_unanswerable_table_reports_none_rather_than_idle() {
        let table = vec![row(100, 1, 100, "claude")];
        assert_eq!(descendant_shell_activity(&table, 999, &[]), None);

        let unreadable = vec![ProcessInfo {
            session_id: -1,
            ..row(100, 1, 100, "claude")
        }];
        assert_eq!(descendant_shell_activity(&unreadable, 100, &[]), None);
    }

    /// A descendant whose own session id could not be read is unclassifiable,
    /// not "in a different session" — otherwise a process exiting during the
    /// sample would read as a false `Working`.
    #[test]
    fn a_descendant_with_an_unreadable_session_id_is_not_counted_as_busy() {
        let table = vec![
            row(100, 1, 100, "claude"),
            ProcessInfo {
                session_id: -1,
                ..row(200, 100, 200, "gone-during-the-sample")
            },
        ];
        assert_eq!(descendant_shell_activity(&table, 100, &[]), Some(false));
    }

    /// Route A's parsing surface: three whitespace-free columns, then the whole
    /// command line kept intact (interior spaces, quotes and `&&` included),
    /// with `??`/`?` recognised as "no controlling terminal" on macOS/Linux
    /// respectively and `session_leader` derived from the session id.
    #[test]
    fn parse_ps_table_keeps_the_command_line_whole_and_reads_the_tty_column() {
        let stdout = concat!(
            "  100     1 ttys014  claude --model opus\n",
            " 200   100 ??       /bin/zsh -c source a/shell-snapshots/snapshot-zsh-1.sh && eval 'x  y'\n",
            "  300     1 ?        /usr/bin/some-linux-daemon\n",
            "not a process row\n",
            "\n",
        );
        let rows = parse_ps_table(stdout, &|pid| if pid == 200 { 200 } else { 100 });
        assert_eq!(rows.len(), 3, "{rows:#?}");

        assert_eq!(rows[0].pid, 100);
        assert_eq!(rows[0].ppid, 1);
        assert!(rows[0].has_controlling_tty);
        assert!(rows[0].session_leader, "getsid(100) == 100");
        assert_eq!(rows[0].argv, "claude --model opus");

        assert!(!rows[1].has_controlling_tty, "macOS prints ?? for no ctty");
        assert!(rows[1].session_leader, "getsid(200) == 200");
        assert_eq!(
            rows[1].argv, "/bin/zsh -c source a/shell-snapshots/snapshot-zsh-1.sh && eval 'x  y'",
            "the command line must survive whole, interior double spaces included"
        );
        assert!(CLAUDE_BASH_TOOL_SHAPE.matches(&rows[1].argv));

        assert!(!rows[2].has_controlling_tty, "Linux prints ? for no ctty");
        assert!(!rows[2].session_leader, "getsid(300) == 100 != 300");
    }

    /// A row with no command line at all must still be kept: the descendant
    /// walk needs its `pid`/`ppid` edge even when there is no argv to read.
    #[test]
    fn parse_ps_table_keeps_a_row_with_an_empty_command_line() {
        let rows = parse_ps_table("  42     1 ??\n", &|_| 42);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].pid, 42);
        assert_eq!(rows[0].ppid, 1);
        assert_eq!(rows[0].argv, "");
    }
}
