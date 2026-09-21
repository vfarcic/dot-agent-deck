//! The evidence file.
//!
//! A run's output has to be checkable by a reader who did not watch it, because
//! that text is what resolves a review thread. So every tell carries the value
//! it was decided on, not just a verdict, and the raw excerpts it was read out
//! of are appended.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::sandbox::Direction;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Verdict {
    Pass,
    Fail,
    /// The harness could not measure this, and says so rather than counting it
    /// either way. A tell nobody measured is not a tell.
    NotChecked,
}

impl Verdict {
    pub fn marker(self) -> &'static str {
        match self {
            Verdict::Pass => "PASS",
            Verdict::Fail => "**FAIL**",
            Verdict::NotChecked => "not checked",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Tell {
    pub id: String,
    pub title: String,
    pub verdict: Verdict,
    /// The measured value the verdict was taken from — a count, a pid, a
    /// scraped line. One or more lines; rendered as a blockquote.
    pub detail: String,
}

/// The run's overall result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunVerdict {
    Pass,
    /// A tell failed, or the scenario broke down: the branch and the previous
    /// release did not interoperate — or the harness could not stand the
    /// scenario up, which the run log says.
    Fail,
    /// Not a pass and not a finding about the branch: a tell could not be
    /// measured, or an isolation check failed or could not be evaluated, so the
    /// run's measurements cannot be trusted either way.
    Incomplete(String),
    /// Reverse only: the previous release's client could not find the branch
    /// daemon at all — MEASURED, with the branch daemon and its roles proven
    /// untouched by what the old client did instead. Distinct from `Fail`,
    /// which means the two builds reached each other and then did not
    /// interoperate (or the old client's fallback damaged something), and from
    /// `Incomplete`, which means the harness could not tell.
    OldClientCannotDiscover(String),
}

impl RunVerdict {
    /// The one-line status a run prints and a report table carries.
    pub fn label(&self) -> String {
        match self {
            RunVerdict::Pass => "PASS".to_string(),
            RunVerdict::Fail => "FAIL".to_string(),
            RunVerdict::Incomplete(why) => format!("INCOMPLETE — {why}"),
            RunVerdict::OldClientCannotDiscover(why) => {
                format!("OLD CLIENT CANNOT DISCOVER THE BRANCH DAEMON — {why}")
            }
        }
    }
}

/// Everything a run learned, in the order it learned it.
///
/// Serialisable because a run has two halves: the inner one, inside the
/// namespace, fills in the scenario and hands this over as JSON; the outer one
/// adds the host-side checks and writes the markdown.
#[derive(Default, Serialize, Deserialize)]
pub struct Evidence {
    pub branch: String,
    pub previous: String,
    /// Where `previous` came from — typed with `--previous`, or resolved from
    /// the release listing, and when. The outer half sets it; `merge_inner`
    /// does not copy it from the inner half, which leaves it empty.
    pub previous_source: String,
    /// Which build served the daemon.
    pub direction: Direction,
    /// The reverse probe the run carried, as one line.
    pub probe: String,
    /// Set, in a reverse run, when the old client was measured unable to find
    /// the branch daemon: what it did instead. See
    /// [`RunVerdict::OldClientCannotDiscover`].
    pub discovery: Option<String>,
    pub head_sha: String,
    /// Set when `--skip-build` reused whatever binary was already in the
    /// target dir: what is known about which commit that binary was built
    /// from. `None` means this run built the branch at `head_sha` itself.
    pub skip_build: Option<String>,
    /// The build-time gate's answer (`buildgate.rs`): identical to the
    /// merge-base, or which build-time files the branch changes and why the run
    /// went ahead anyway. The outer half sets it; `merge_inner` leaves it.
    pub build_time: String,
    pub started_at: String,
    pub mode: String,
    /// What the run's deck processes executed inside.
    pub namespace: String,
    pub xdg_runtime_dir: String,
    pub sandbox_root: PathBuf,
    pub old_binary: PathBuf,
    pub new_binary: PathBuf,
    pub old_hello: String,
    pub new_hello: String,
    pub daemon_pid: i32,
    pub daemon_endpoint: String,
    /// Progress log: what the harness did, in order, with what it observed.
    pub steps: Vec<String>,
    pub preflight: Vec<String>,
    pub tells: Vec<Tell>,
    /// `(title, body)` pairs appended verbatim at the end.
    pub excerpts: Vec<(String, String)>,
    /// What the isolation layer proved, in the order it proved it.
    pub isolation: Vec<String>,
    /// Set when an isolation check failed or could not be evaluated. It voids
    /// the run: the verdict is INCOMPLETE whatever the tells say.
    pub isolation_failure: Option<String>,
    /// The outer half's checks after the namespace exited.
    pub postconditions: Vec<String>,
}

impl Evidence {
    pub fn step(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        println!("  · {msg}");
        self.steps.push(msg);
    }

    pub fn tell(
        &mut self,
        id: &str,
        title: impl Into<String>,
        verdict: Verdict,
        detail: impl Into<String>,
    ) {
        let title = title.into();
        let detail = detail.into();
        println!("  [{}] {id} {title}", verdict.marker());
        self.tells.push(Tell {
            id: id.to_string(),
            title,
            verdict,
            detail,
        });
    }

    pub fn isolated(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        println!("  ⊘ {msg}");
        self.isolation.push(msg);
    }

    /// Record an isolation failure. The first one wins: it is the cause, and
    /// anything after it is a consequence.
    pub fn isolation_failed(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        println!("  ⊘ ISOLATION FAILURE: {msg}");
        if self.isolation_failure.is_none() {
            self.isolation_failure = Some(msg);
        }
    }

    pub fn excerpt(&mut self, title: impl Into<String>, body: impl Into<String>) {
        self.excerpts.push((title.into(), body.into()));
    }

    pub fn passed(&self) -> bool {
        !self.tells.iter().any(|t| t.verdict == Verdict::Fail)
    }

    /// Whether every one of the four required tells was actually measured.
    ///
    /// Separate from [`passed`](Self::passed) on purpose: a run in which a tell
    /// could not be measured is not a pass, and reporting it as one is the
    /// false green this whole harness exists to prevent.
    pub fn complete(&self) -> bool {
        self.tells.iter().all(|t| t.verdict != Verdict::NotChecked)
    }

    /// The run's result. An isolation failure dominates everything: a tell
    /// measured inside a namespace that did not hold is not a measurement of
    /// the branch.
    ///
    /// The order is the argument. A failed tell outranks a measured
    /// non-discovery, because a failed collateral check there means the old
    /// client's fallback DID damage something — which is a finding, not the
    /// disclosed downgrade. An unmeasured tell outranks it too: "the old client
    /// could not find the daemon and nothing else happened" is only a claim when
    /// the "nothing else" was measured.
    pub fn verdict(&self) -> RunVerdict {
        if let Some(why) = &self.isolation_failure {
            return RunVerdict::Incomplete(format!("an isolation check failed — {why}"));
        }
        if !self.passed() {
            return RunVerdict::Fail;
        }
        if self.tells.is_empty() {
            return RunVerdict::Incomplete("no tell was measured".to_string());
        }
        if !self.complete() {
            return RunVerdict::Incomplete("a tell could not be measured".to_string());
        }
        if let Some(what) = &self.discovery {
            return RunVerdict::OldClientCannotDiscover(what.clone());
        }
        RunVerdict::Pass
    }

    pub fn render(&self) -> String {
        let mut s = String::new();
        let verdict = match self.verdict() {
            RunVerdict::Incomplete(why) => format!("INCOMPLETE — {why}, so this is not a pass"),
            v => v.label(),
        };
        let reverse = self.direction == Direction::Reverse;
        let _ = writeln!(
            s,
            "# Cross-version contract check — `{}`{}\n",
            self.branch,
            if reverse { " (reverse)" } else { "" }
        );
        let _ = writeln!(s, "**Verdict: {verdict}**\n");
        if let Some(note) = &self.skip_build {
            let _ = writeln!(
                s,
                "**The branch binary was NOT rebuilt for this run (`--skip-build`).** {note}\n"
            );
        }
        if reverse {
            let _ = writeln!(
                s,
                "The REVERSE of CLAUDE.md rule 12's pairing, run by `cargo xver --direction reverse` \
                 (`xtask/cross-version`, documented in `docs/develop/cross-version-harness.md`): the \
                 BRANCH daemon with live agents under it, then the previous release's TUI started \
                 against it over a PTY. When that TUI finds the branch daemon, its build-version \
                 prompt is declined, the previous release's CLI issues every pane command, and the \
                 run asserts the same four tells, pinned to the branch daemon, plus the \
                 branch-specific probe named below. Rule 12 prescribes the forward pairing, which \
                 for a daemon-side change runs the previous release's daemon code and never \
                 executes a changed line; this pairing is the one that does. Stand-in agents, not \
                 real ones: the runtime scenario is given no AGENT credential and invokes no \
                 agent. The branch build ran on the host, outside the namespace — the \
                 build-time code row below says whether the branch changed any of it.\n"
            );
            if self.discovery.is_some() {
                let _ = writeln!(
                    s,
                    "**In this run the old TUI did NOT find the branch daemon**, so none of the \
                     four tells and no probe stimulus was measured, and nothing here says how the \
                     branch daemon handles an old client's traffic. The verdict rests on the \
                     Discovery section and the collateral tells alone: that the old TUI, started \
                     with the run's environment, did not reach the branch daemon, what it started \
                     instead, and what the collateral tells measured about the branch daemon, its \
                     roles and the old TUI's own daemon.\n"
                );
            }
        } else {
            let _ = writeln!(
                s,
                "CLAUDE.md rule 12's cross-version manual test, run by `cargo xver` \
                 (`xtask/cross-version`, documented in `docs/develop/cross-version-harness.md`). It \
                 reproduces the scenario the rule describes — a previous-release daemon with live \
                 agents under it, then the branch TUI attached to that same daemon over a PTY with \
                 the build-version prompt declined — and asserts the four tells below. Stand-in \
                 agents, not real ones: this check is about the TUI↔daemon wire, so the runtime \
                 scenario is given no AGENT credential and invokes no agent. The branch build ran \
                 on the host, outside the namespace — the build-time code row below says whether \
                 the branch changed any of it.\n"
            );
        }

        let _ = writeln!(s, "## What was run\n");
        let _ = writeln!(s, "| | |");
        let _ = writeln!(s, "| --- | --- |");
        let _ = writeln!(s, "| branch under test | `{}` |", self.branch);
        if self.skip_build.is_some() {
            let _ = writeln!(
                s,
                "| branch HEAD | `{}` — checked out, but the binary under test was NOT built from \
                 it by this run (`--skip-build`, see above) |",
                self.head_sha
            );
        } else {
            let _ = writeln!(s, "| branch HEAD | `{}` |", self.head_sha);
        }
        if !self.build_time.is_empty() {
            let _ = writeln!(s, "| build-time code | {} |", self.build_time);
        }
        if self.previous_source.is_empty() {
            let _ = writeln!(s, "| previous release | `{}` |", self.previous);
        } else {
            let _ = writeln!(
                s,
                "| previous release | `{}` — {} |",
                self.previous, self.previous_source
            );
        }
        let _ = writeln!(
            s,
            "| direction | {} |",
            if reverse {
                "reverse — BRANCH daemon, previous-release TUI and CLI"
            } else {
                "forward — previous-release daemon, branch TUI and CLI (rule 12's pairing)"
            }
        );
        if reverse {
            let _ = writeln!(s, "| probe | {} |", self.probe);
        }
        let _ = writeln!(s, "| started (UTC) | {} |", self.started_at);
        let _ = writeln!(s, "| endpoint mode | {} |", self.mode);
        let _ = writeln!(s, "| namespace | {} |", self.namespace);
        let _ = writeln!(s, "| `XDG_RUNTIME_DIR` | {} |", self.xdg_runtime_dir);
        let _ = writeln!(s, "| sandbox | `{}` |", self.sandbox_root.display());
        let _ = writeln!(s, "| old binary | `{}` |", self.old_binary.display());
        let _ = writeln!(s, "| new binary | `{}` |", self.new_binary.display());
        let _ = writeln!(s, "| sandbox daemon pid | {} |", self.daemon_pid);
        let _ = writeln!(s, "| daemon attach endpoint | `{}` |", self.daemon_endpoint);
        let _ = writeln!(s);
        let _ = writeln!(
            s,
            "`daemon hello`, old build:\n\n```json\n{}\n```\n",
            self.old_hello.trim()
        );
        let _ = writeln!(
            s,
            "`daemon hello`, branch build:\n\n```json\n{}\n```\n",
            self.new_hello.trim()
        );

        if let Some(what) = &self.discovery {
            let _ = writeln!(
                s,
                "## Discovery: the old client did not find the branch daemon\n"
            );
            for line in what.lines() {
                let _ = writeln!(s, "> {line}");
            }
            let _ = writeln!(s);
        }

        let _ = writeln!(
            s,
            "{}",
            if reverse {
                "## Tells\n"
            } else {
                "## The four tells\n"
            }
        );
        for t in &self.tells {
            let _ = writeln!(s, "### {} — {} · {}\n", t.id, t.title, t.verdict.marker());
            for line in t.detail.lines() {
                let _ = writeln!(s, "> {line}");
            }
            let _ = writeln!(s);
        }

        let _ = writeln!(s, "## Isolation\n");
        match (&self.isolation_failure, self.isolation.is_empty()) {
            (Some(why), _) => {
                let _ = writeln!(s, "**ISOLATION FAILURE: {why}**\n");
            }
            (None, true) => {
                let _ = writeln!(
                    s,
                    "No isolation check was recorded: the run stopped before its namespace \
                     started, so no deck process ran.\n"
                );
            }
            (None, false) => {
                let _ = writeln!(
                    s,
                    "The daemon, both TUIs and every deck CLI call of this run ran inside one \
                     private bubblewrap namespace, and each line below is a check that \
                     was measured and held: the mount, PID, network, IPC, UTS and user namespaces \
                     are private; `/tmp` and `/run/user/<uid>` (both endpoint roots) and \
                     `/var/tmp` are sandbox directories; the operator's home is an empty tmpfs \
                     with only the sandbox bound back into it; the rest of `/` is bound read-only; and \
                     the environment is built from an allowlist. \
                     `docs/develop/cross-version-harness.md` says what this does not cover.\n"
                );
            }
        }
        for line in &self.isolation {
            let _ = writeln!(s, "- {line}");
        }
        let _ = writeln!(s);

        let _ = writeln!(
            s,
            "## Postconditions (outside the namespace, after it exited)\n"
        );
        for line in &self.postconditions {
            let _ = writeln!(s, "- {line}");
        }
        let _ = writeln!(s);

        let _ = writeln!(s, "## Preflight\n");
        for p in &self.preflight {
            let _ = writeln!(s, "- {p}");
        }
        let _ = writeln!(s);

        let _ = writeln!(s, "## Run log\n");
        for (i, step) in self.steps.iter().enumerate() {
            let _ = writeln!(s, "{}. {step}", i + 1);
        }
        let _ = writeln!(s);

        if !self.excerpts.is_empty() {
            let _ = writeln!(s, "## Raw excerpts\n");
            for (title, body) in &self.excerpts {
                let _ = writeln!(s, "<details>\n<summary>{title}</summary>\n");
                let _ = writeln!(s, "```\n{}\n```\n", body.trim_end());
                let _ = writeln!(s, "</details>\n");
            }
        }
        s
    }

    pub fn write_to(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create {}: {e}", parent.display()))?;
        }
        std::fs::write(path, self.render()).map_err(|e| format!("write {}: {e}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev() -> Evidence {
        Evidence {
            branch: "agent/x".into(),
            ..Default::default()
        }
    }

    #[test]
    fn a_run_with_no_failures_and_nothing_unmeasured_is_a_pass() {
        let mut e = ev();
        e.tells.push(Tell {
            id: "tell-1".into(),
            title: "t".into(),
            verdict: Verdict::Pass,
            detail: "1".into(),
        });
        assert!(e.passed() && e.complete());
        assert!(e.render().contains("**Verdict: PASS**"));
    }

    #[test]
    fn an_unmeasured_tell_is_not_reported_as_a_pass() {
        let mut e = ev();
        e.tells.push(Tell {
            id: "tell-1".into(),
            title: "t".into(),
            verdict: Verdict::NotChecked,
            detail: "no ss(8) on this host".into(),
        });
        assert!(e.passed(), "nothing failed");
        assert!(!e.complete(), "but something was not measured");
        assert!(e.render().contains("INCOMPLETE"));
    }

    #[test]
    fn a_failing_tell_renders_as_fail() {
        let mut e = ev();
        e.tells.push(Tell {
            id: "tell-1".into(),
            title: "t".into(),
            verdict: Verdict::Fail,
            detail: "2 lines, expected 1".into(),
        });
        assert!(!e.passed());
        assert!(e.render().contains("**Verdict: FAIL**"));
    }

    #[test]
    fn an_isolation_failure_voids_even_an_all_pass_run() {
        let mut e = ev();
        e.tell("tell-1", "t", Verdict::Pass, "1");
        e.isolated("mount namespace mnt:[2]");
        e.isolation_failed("host /tmp/dot-agent-deck-1000.sock changed");
        e.isolation_failed("a consequence");
        assert!(
            matches!(e.verdict(), RunVerdict::Incomplete(ref w) if w.contains("dot-agent-deck-1000.sock"))
        );
        let out = e.render();
        assert!(out.contains("**Verdict: INCOMPLETE"), "{out}");
        assert!(out.contains("ISOLATION FAILURE: host /tmp"), "{out}");
        assert!(
            !out.contains("a consequence"),
            "the first failure is the cause: {out}"
        );
    }

    #[test]
    fn the_previous_release_row_says_where_the_tag_came_from() {
        let mut e = ev();
        e.previous = "v0.41.0".into();
        e.previous_source = "resolved, not given: from `gh release list`".into();
        let out = e.render();
        assert!(
            out.contains(
                "| previous release | `v0.41.0` — resolved, not given: from `gh release list` |"
            ),
            "{out}"
        );
    }

    #[test]
    fn a_run_with_no_tells_is_never_a_pass() {
        assert!(matches!(ev().verdict(), RunVerdict::Incomplete(_)));
        assert!(
            ev().render()
                .contains("stopped before its namespace started")
        );
    }

    #[test]
    fn evidence_survives_the_json_hop_between_the_two_halves() {
        let mut e = ev();
        e.tell("tell-2", "t", Verdict::NotChecked, "no ss");
        e.isolated("x");
        let back: Evidence =
            serde_json::from_str(&serde_json::to_string(&e).expect("ser")).expect("de");
        assert_eq!(back.tells[0].id, "tell-2");
        assert_eq!(back.tells[0].verdict, Verdict::NotChecked);
        assert_eq!(back.isolation, vec!["x".to_string()]);
    }

    #[test]
    fn a_skipped_build_is_stated_under_the_verdict_and_qualifies_the_head_row() {
        let mut e = ev();
        e.head_sha = "e2bbb205".into();
        e.tell("tell-1", "t", Verdict::Pass, "1");
        e.skip_build = Some("**STALE:** its build id names commit `66995314`".into());
        let out = e.render();
        assert!(
            out.contains(
                "**The branch binary was NOT rebuilt for this run (`--skip-build`).** **STALE:**"
            ),
            "{out}"
        );
        assert!(
            out.contains("| branch HEAD | `e2bbb205` — checked out, but the binary under test"),
            "{out}"
        );
    }

    #[test]
    fn a_built_run_says_nothing_about_skip_build() {
        let mut e = ev();
        e.head_sha = "e2bbb205".into();
        let out = e.render();
        assert!(!out.contains("--skip-build"), "{out}");
        assert!(out.contains("| branch HEAD | `e2bbb205` |"), "{out}");
    }

    #[test]
    fn detail_is_rendered_as_a_blockquote_line_per_line() {
        let mut e = ev();
        e.tells.push(Tell {
            id: "tell-1".into(),
            title: "t".into(),
            verdict: Verdict::Pass,
            detail: "first\nsecond".into(),
        });
        let out = e.render();
        assert!(out.contains("> first\n> second"), "{out}");
    }
}

#[cfg(test)]
mod reverse_tests {
    use super::*;

    fn reverse() -> Evidence {
        Evidence {
            branch: "agent/x".into(),
            direction: Direction::Reverse,
            probe: "`log-escaping` — PR #1169 / issue #1082".into(),
            ..Default::default()
        }
    }

    #[test]
    fn a_measured_non_discovery_is_its_own_verdict() {
        let mut e = reverse();
        e.tell("collateral-1", "t", Verdict::Pass, "x");
        e.discovery = Some("the old TUI lazy-spawned its own daemon".into());
        assert!(matches!(
            e.verdict(),
            RunVerdict::OldClientCannotDiscover(_)
        ));
        let out = e.render();
        assert!(
            out.contains("**Verdict: OLD CLIENT CANNOT DISCOVER THE BRANCH DAEMON"),
            "{out}"
        );
        assert!(out.contains("## Discovery"), "{out}");
    }

    #[test]
    fn a_discovery_report_does_not_claim_the_four_tells_or_the_probe() {
        let mut e = reverse();
        e.tell("collateral-3", "t", Verdict::Fail, "duplicated");
        e.discovery = Some("the old TUI lazy-spawned its own daemon".into());
        let out = e.render();
        assert!(
            out.contains(
                "**In this run the old TUI did NOT find the branch daemon**, so none of the four \
                 tells and no probe stimulus was measured"
            ),
            "{out}"
        );
    }

    #[test]
    fn damage_from_the_old_clients_fallback_is_a_fail_not_a_disclosed_downgrade() {
        let mut e = reverse();
        e.tell(
            "collateral-3",
            "t",
            Verdict::Fail,
            "two daemons, one orchestration",
        );
        e.discovery = Some("x".into());
        assert_eq!(e.verdict(), RunVerdict::Fail);
    }

    #[test]
    fn an_unmeasured_collateral_check_leaves_the_run_incomplete() {
        let mut e = reverse();
        e.tell("collateral-3", "t", Verdict::NotChecked, "could not ask");
        e.discovery = Some("x".into());
        assert!(matches!(e.verdict(), RunVerdict::Incomplete(_)));
    }

    #[test]
    fn an_isolation_failure_still_dominates_a_reverse_run() {
        let mut e = reverse();
        e.tell("collateral-1", "t", Verdict::Pass, "x");
        e.discovery = Some("x".into());
        e.isolation_failed("host endpoint changed");
        assert!(matches!(e.verdict(), RunVerdict::Incomplete(ref w) if w.contains("host")));
    }

    #[test]
    fn a_reverse_report_says_so_and_names_its_probe() {
        let mut e = reverse();
        e.tell("tell-1", "t", Verdict::Pass, "1");
        let out = e.render();
        assert!(out.starts_with("# Cross-version contract check — `agent/x` (reverse)"));
        assert!(
            out.contains("BRANCH daemon, previous-release TUI and CLI"),
            "{out}"
        );
        assert!(out.contains("| probe | `log-escaping`"), "{out}");
        assert!(out.contains("## Tells"), "{out}");
        assert!(
            out.contains("When that TUI finds the branch daemon"),
            "the four tells are conditional on discovery: {out}"
        );
        assert!(!out.contains("did NOT find the branch daemon"), "{out}");
        let fwd = Evidence {
            branch: "agent/x".into(),
            ..Default::default()
        };
        let out = fwd.render();
        assert!(out.contains("## The four tells") && !out.contains("| probe |"));
    }
}
