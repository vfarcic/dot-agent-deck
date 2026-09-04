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
    /// The process's command line, **or a statement that this sample did not
    /// read it** — see [`CommandLine`], and issue #862 for why the sample no
    /// longer reads every process's.
    pub command_line: CommandLine,
}

/// What a sample knows about one process's command line (issue #862).
///
/// The bulk process-table sample deliberately does **not** ask `ps` for the
/// `args` column any more, because that column is what made the sample
/// load-sensitive: measured with `strace`, `ps -A -o pid=,ppid=,tty=,args=`
/// opens `/proc/<pid>/cmdline` *and* `/proc/<pid>/environ` for every process on
/// the machine (372 of each on a 382-process box), while
/// `ps -A -o pid=,ppid=,tty=` opens neither. Both of those two go through the
/// kernel's `access_remote_vm()` and therefore take the *target's* `mmap_lock`;
/// `/proc/<pid>/stat` and `/proc/<pid>/status`, which supply `pid`/`ppid`/`tty`,
/// do not. So the argv column made the sample's wall time the sum of every
/// unrelated process's `mmap_lock` wait — the mechanism behind the field
/// measurement in issue #862, where a sample went from ~49 ms idle to 19-20 s
/// under a build storm.
///
/// The argv is still needed, for the [`ShellToolShape`] cross-check — but only
/// for a **detached descendant** of one of the sample's roots, which is at most
/// one process per pane and zero on an idle deck. So it is read in a second
/// phase, for exactly the pids [`detached_descendants`] reports. This enum
/// exists so the difference between "nothing needed it" and "we wanted it and
/// could not get it" survives into the classifier instead of collapsing into an
/// empty string that silently matches no shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandLine {
    /// The sample read this process's command line. Callers must substring-match
    /// inside it and must never tokenise it: Claude Code's Bash-tool child has
    /// `argc == 3`, with the whole prologue plus the user's command in a single
    /// argv element.
    Read(String),
    /// The sample deliberately did not read it: this process is not a detached
    /// descendant of any root the sample was given, so no cross-check can ever
    /// consult it. The overwhelmingly common case — every process on the machine
    /// except our own panes' detached descendants.
    NotSampled,
    /// The sample tried to read it and could not — the ordinary cause being that
    /// the process exited between the table sample and the argv sample.
    Unavailable,
}

impl CommandLine {
    /// The command line if this sample read it, else `None`.
    pub fn read(&self) -> Option<&str> {
        match self {
            Self::Read(argv) => Some(argv),
            Self::NotSampled | Self::Unavailable => None,
        }
    }
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
/// **When `shapes` is non-empty, `table` must have been sampled with `root_pid`
/// among its roots** (issue #862) — that is what makes the candidates'
/// [`CommandLine`]s present, since the sampler reads a command line for exactly
/// the pids [`detached_descendants`] reports for its roots. A candidate that
/// reaches the cross-check as [`CommandLine::NotSampled`] means those two sets
/// disagreed; it is read as "not a match" and logged at `warn`, because
/// inventing a match from a command line nobody read would pin the pane at
/// `Working` forever. With an empty `shapes` no command line is read at all, so
/// the roots do not matter.
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
    let detached = detached_descendants(table, root_pid)?;
    if detached.is_empty() {
        return Some(false);
    }
    // The structural test has already answered. `shapes` is the optional argv
    // cross-check on top of it, and an empty `shapes` means the caller asked for
    // the structural test alone — in which case no command line is consulted at
    // all, which is why a pane whose agent kind has never been measured costs no
    // argv read even while it is busy.
    if shapes.is_empty() {
        return Some(true);
    }
    for pid in detached {
        let Some(candidate) = table.iter().find(|row| row.pid == pid) else {
            continue;
        };
        match &candidate.command_line {
            CommandLine::Read(argv) => {
                if shapes.iter().any(|shape| shape.matches(argv)) {
                    return Some(true);
                }
            }
            // Wanted and not obtained. Ordinary and expected: the process
            // exited between the table sample and the argv sample, and a
            // command line that no longer exists is not running anything, so
            // "not a match" is the right reading.
            CommandLine::Unavailable => continue,
            // Should be unreachable, and it is the one case worth a log line.
            // The sampler fills the command line for exactly the pids
            // `detached_descendants` reports (see `super::process_table`), so
            // reaching here means the sampler and this classifier disagree
            // about that set — which would suppress the signal *silently*, the
            // failure mode PRD #386 exists to end. Treated as "not a match"
            // rather than as a match, because inventing a busy reading from a
            // command line nobody read would pin the pane at `Working`.
            CommandLine::NotSampled => {
                tracing::warn!(
                    root_pid,
                    candidate_pid = pid,
                    "shell-activity: a detached descendant reached the argv cross-check with no \
                     command line sampled; the process-table sampler and the classifier disagree \
                     about which pids need one, so this pane's signal is suppressed"
                );
                continue;
            }
        }
    }
    Some(false)
}

/// The **structural half** of [`descendant_shell_activity`] on its own: the pids
/// of `root_pid`'s transitive descendants that sit in a different POSIX session
/// than `root_pid` itself, in walk order.
///
/// `None` has exactly [`descendant_shell_activity`]'s meaning — "no answer
/// available": `root_pid` is not in the table, or its own session id could not
/// be read. An empty `Vec` is the positive statement that the root has no
/// detached descendant, i.e. structurally idle.
///
/// This is the **one** definition of "which processes could a cross-check ever
/// need the command line of" (issue #862). The two-phase sampler calls it to
/// decide whose `/proc/<pid>/cmdline` to read, and
/// [`descendant_shell_activity`] calls it to decide whose command line to
/// consult; sharing the function is what keeps those two sets identical rather
/// than merely intended to be.
pub fn detached_descendants(table: &[ProcessInfo], root_pid: i32) -> Option<Vec<i32>> {
    let root = table.iter().find(|row| row.pid == root_pid)?;
    if root.session_id <= 0 {
        return None;
    }
    Some(
        descendants(table, root_pid)
            .into_iter()
            // A row whose session id could not be read is unclassifiable, not
            // "different" — counting it as different would turn an exit racing
            // the sample into a false `Working`.
            .filter(|row| row.session_id > 0 && row.session_id != root.session_id)
            .map(|row| row.pid)
            .collect(),
    )
}

/// Every pid whose command line the second sampling phase must read, for a
/// sample taken on behalf of `roots` (issue #862) — the union of
/// [`detached_descendants`] over every root, deduplicated and sorted.
///
/// Sorted so the `ps -p <list>` invocation built from it is deterministic, which
/// is what makes the argv phase testable against a fixed expected command line.
///
/// A root the table cannot answer for contributes nothing rather than aborting
/// the whole set: one exited pane must not cost every other pane its argv.
pub fn command_line_targets(table: &[ProcessInfo], roots: &[i32]) -> Vec<i32> {
    let mut wanted: Vec<i32> = roots
        .iter()
        .filter_map(|root| detached_descendants(table, *root))
        .flatten()
        .collect();
    wanted.sort_unstable();
    wanted.dedup();
    wanted
}

/// Record the second phase's answers onto the table: every pid in `wanted` gets
/// [`CommandLine::Read`] if `resolved` carries one and [`CommandLine::Unavailable`]
/// if it does not (issue #862).
///
/// Rows outside `wanted` are left at [`CommandLine::NotSampled`], which is the
/// honest statement about them — nothing asked for their command line, so
/// nothing read it.
pub fn fill_command_lines(
    table: &mut [ProcessInfo],
    wanted: &[i32],
    resolved: &HashMap<i32, String>,
) {
    let wanted: HashSet<i32> = wanted.iter().copied().collect();
    for row in table.iter_mut() {
        if !wanted.contains(&row.pid) {
            continue;
        }
        row.command_line = match resolved.get(&row.pid) {
            Some(argv) => CommandLine::Read(argv.clone()),
            None => CommandLine::Unavailable,
        };
    }
}

/// Whether a `ps` TTY column names a real terminal. macOS prints `??` for a
/// process with no controlling terminal, Linux's procps prints `?`, and `-`
/// turns up in some `ps` implementations; anything else is a terminal name.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn tty_field_names_a_terminal(tty: &str) -> bool {
    !matches!(tty, "" | "?" | "??" | "-")
}

/// Parse the output of `ps -A -w -w -o pid=,ppid=,tty=` into a table, resolving each
/// row's POSIX session id through `session_id_of`.
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
///
/// **Every row comes back [`CommandLine::NotSampled`]** (issue #862): this phase
/// does not ask `ps` for the `args` column, so it has nothing to say about any
/// command line, and anything trailing the third column is ignored rather than
/// stored. The command lines the cross-check needs arrive from
/// [`parse_ps_command_lines`] via [`fill_command_lines`].
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn parse_ps_table(stdout: &str, session_id_of: &dyn Fn(i32) -> i32) -> Vec<ProcessInfo> {
    let mut rows = Vec::new();
    for line in stdout.lines() {
        let Some((pid, ppid, tty)) = split_ps_row(line) else {
            continue;
        };
        let session_id = session_id_of(pid);
        rows.push(ProcessInfo {
            pid,
            ppid,
            session_id,
            has_controlling_tty: tty_field_names_a_terminal(tty),
            session_leader: session_id > 0 && session_id == pid,
            command_line: CommandLine::NotSampled,
        });
    }
    rows
}

/// Parse the output of the argv phase's `ps -w -w -o pid=,args= -p <list>` into
/// `pid → command line` (issue #862).
///
/// The command line is **kept whole** — interior spaces, quotes and `&&`
/// included — because the [`ShellToolShape`] cross-check substring-matches
/// inside it and must never tokenise it. A pid the output does not mention is
/// simply absent from the map, which [`fill_command_lines`] records as
/// [`CommandLine::Unavailable`]; that is the normal way a process that exited
/// between the two phases shows up.
///
/// A row with a pid and no command line at all is kept as an empty string
/// rather than dropped: it is a genuine answer ("this process has no argv the
/// kernel will show us"), and it matches no shape, which is the correct reading.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn parse_ps_command_lines(stdout: &str) -> HashMap<i32, String> {
    let mut out = HashMap::new();
    for line in stdout.lines() {
        let Some((pid, rest)) = next_token(line) else {
            continue;
        };
        let Ok(pid) = pid.parse::<i32>() else {
            continue;
        };
        out.insert(pid, rest.trim_start().to_string());
    }
    out
}

/// Split one bulk-phase `ps` row into `(pid, ppid, tty)`. All three columns are
/// whitespace-free tokens; anything after them is ignored, since this phase
/// asks for no fourth column (issue #862).
fn split_ps_row(line: &str) -> Option<(i32, i32, &str)> {
    let (pid, rest) = next_token(line)?;
    let (ppid, rest) = next_token(rest)?;
    let (tty, _rest) = next_token(rest)?;
    Some((pid.parse().ok()?, ppid.parse().ok()?, tty))
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
            command_line: CommandLine::Read(argv.to_string()),
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

    /// Route A's bulk parsing surface: three whitespace-free columns, with
    /// `??`/`?` recognised as "no controlling terminal" on macOS/Linux
    /// respectively and `session_leader` derived from the session id. Issue #862
    /// removed the fourth column, so anything trailing the tty is ignored rather
    /// than stored, and every row reports `NotSampled`.
    #[test]
    fn parse_ps_table_reads_the_three_columns_and_samples_no_command_line() {
        let stdout = concat!(
            "  100     1 ttys014\n",
            " 200   100 ??\n",
            "  300     1 ?\n",
            // A stray trailing column must not become a command line: the bulk
            // phase asked for none, so recording one would let a `Read` value
            // appear for a process nothing wanted an argv for.
            "  400     1 ttys014  claude --model opus\n",
            "not a process row\n",
            "\n",
        );
        let rows = parse_ps_table(stdout, &|pid| if pid == 200 { 200 } else { 100 });
        assert_eq!(rows.len(), 4, "{rows:#?}");

        assert_eq!(rows[0].pid, 100);
        assert_eq!(rows[0].ppid, 1);
        assert!(rows[0].has_controlling_tty);
        assert!(rows[0].session_leader, "getsid(100) == 100");

        assert!(!rows[1].has_controlling_tty, "macOS prints ?? for no ctty");
        assert!(rows[1].session_leader, "getsid(200) == 200");

        assert!(!rows[2].has_controlling_tty, "Linux prints ? for no ctty");
        assert!(!rows[2].session_leader, "getsid(300) == 100 != 300");

        assert_eq!(rows[3].pid, 400);
        for r in &rows {
            assert_eq!(
                r.command_line,
                CommandLine::NotSampled,
                "the bulk phase reads no command line at all: {r:#?}"
            );
        }
    }

    /// A row with nothing after the tty column must still be kept: the
    /// descendant walk needs its `pid`/`ppid` edge, and the bulk phase asks for
    /// no fourth column anyway.
    #[test]
    fn parse_ps_table_keeps_a_row_with_nothing_after_the_tty_column() {
        let rows = parse_ps_table("  42     1 ??\n", &|_| 42);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].pid, 42);
        assert_eq!(rows[0].ppid, 1);
        assert_eq!(rows[0].command_line, CommandLine::NotSampled);
    }

    /// Issue #862, the argv phase's parsing surface: `pid` then the whole
    /// command line, interior double spaces and quotes intact, with a pid the
    /// output never mentions simply absent from the map.
    #[test]
    fn parse_ps_command_lines_keeps_each_command_line_whole() {
        let stdout = concat!(
            " 200 /bin/zsh -c source a/shell-snapshots/snapshot-zsh-1.sh && eval 'x  y'\n",
            "  42\n",
            "not a row\n",
            "\n",
        );
        let map = parse_ps_command_lines(stdout);
        assert_eq!(map.len(), 2, "{map:#?}");
        assert_eq!(
            map[&200], "/bin/zsh -c source a/shell-snapshots/snapshot-zsh-1.sh && eval 'x  y'",
            "the command line must survive whole, interior double spaces included"
        );
        assert!(CLAUDE_BASH_TOOL_SHAPE.matches(&map[&200]));
        assert_eq!(
            map[&42], "",
            "a pid with no argv is a real answer, not a missing one"
        );
        assert!(!map.contains_key(&999));
    }

    /// Issue #862 — the invariant the two-phase sample rests on: the set of pids
    /// whose command line the sampler reads is EXACTLY the set the classifier
    /// consults, because both are `detached_descendants`. Asserted directly, so
    /// a future change that widens one without the other fails here rather than
    /// silently suppressing a pane's signal.
    #[test]
    fn command_line_targets_are_exactly_the_detached_descendants() {
        let table = vec![
            // Two panes' shells, each with an in-session child (an MCP server
            // shape) and a detached one (a Bash-tool shape).
            row(100, 1, 100, "claude --model opus"),
            row(101, 100, 100, "npm exec @upstash/context7-mcp"),
            row(102, 100, 5102, "detached under pane one"),
            row(200, 1, 200, "claude --model opus"),
            row(201, 200, 200, "caffeinate -i -t 300"),
            row(202, 200, 5202, "detached under pane two"),
            // An unrelated process on the machine, in its own session. Not a
            // descendant of either root, so no phase may ever read its argv —
            // this is the whole point of the change.
            row(900, 1, 900, "some unrelated wedged linker"),
        ];
        assert_eq!(detached_descendants(&table, 100).unwrap(), vec![102]);
        assert_eq!(detached_descendants(&table, 200).unwrap(), vec![202]);
        assert_eq!(command_line_targets(&table, &[100, 200]), vec![102, 202]);
        assert_eq!(
            command_line_targets(&table, &[]),
            Vec::<i32>::new(),
            "no roots means no argv read at all"
        );
        // A root the table cannot answer for costs the others nothing.
        assert_eq!(command_line_targets(&table, &[100, 4242]), vec![102]);
    }

    /// Issue #862 — a detached descendant whose command line was never sampled
    /// must NOT be read as busy. The cross-check is evidence; inventing a match
    /// from a command line nobody read would pin the pane at `Working` forever,
    /// which the PRD calls worse than the stale `Idle` it replaces.
    #[test]
    fn a_detached_descendant_with_no_sampled_command_line_is_not_busy() {
        let mut table = vec![
            row(100, 1, 100, "claude --model opus"),
            row(200, 100, 250, "the bash-tool child"),
        ];
        table[1].command_line = CommandLine::NotSampled;
        assert_eq!(
            descendant_shell_activity(&table, 100, &[CLAUDE_BASH_TOOL_SHAPE]),
            Some(false)
        );
        table[1].command_line = CommandLine::Unavailable;
        assert_eq!(
            descendant_shell_activity(&table, 100, &[CLAUDE_BASH_TOOL_SHAPE]),
            Some(false),
            "a process that exited between the two phases is not running anything"
        );
        // The structural test alone is unaffected: it reads no command line, so
        // a pane whose agent kind was never measured costs no argv read and is
        // still classified.
        assert_eq!(descendant_shell_activity(&table, 100, &[]), Some(true));
    }

    /// Issue #862 — `fill_command_lines` records the argv phase's answers on the
    /// wanted pids only, distinguishing "read it" from "wanted it and it was
    /// gone", and leaves every other row's `NotSampled` untouched.
    #[test]
    fn fill_command_lines_marks_wanted_rows_and_leaves_the_rest_not_sampled() {
        let mut table = vec![
            row(100, 1, 100, "claude"),
            row(102, 100, 5102, "placeholder"),
            row(900, 1, 900, "unrelated"),
        ];
        for r in table.iter_mut() {
            r.command_line = CommandLine::NotSampled;
        }
        let resolved = HashMap::from([(102, "the real bash child".to_string())]);
        fill_command_lines(&mut table, &[102, 103], &resolved);
        assert_eq!(
            table[1].command_line,
            CommandLine::Read("the real bash child".to_string())
        );
        assert_eq!(table[0].command_line, CommandLine::NotSampled);
        assert_eq!(
            table[2].command_line,
            CommandLine::NotSampled,
            "an unrelated process's command line is never read, at either phase"
        );

        // A wanted pid the argv phase could not answer for.
        let mut table = vec![row(102, 100, 5102, "placeholder")];
        table[0].command_line = CommandLine::NotSampled;
        fill_command_lines(&mut table, &[102], &HashMap::new());
        assert_eq!(table[0].command_line, CommandLine::Unavailable);
    }
}
