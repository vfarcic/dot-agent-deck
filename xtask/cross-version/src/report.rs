//! The evidence file.
//!
//! A run's output has to be checkable by a reader who did not watch it, because
//! that text is what resolves a review thread. So every tell carries the value
//! it was decided on, not just a verdict, and the raw excerpts it was read out
//! of are appended.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

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
    pub head_sha: String,
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
        RunVerdict::Pass
    }

    pub fn render(&self) -> String {
        let mut s = String::new();
        let verdict = match self.verdict() {
            RunVerdict::Pass => "PASS".to_string(),
            RunVerdict::Fail => "FAIL".to_string(),
            RunVerdict::Incomplete(why) => format!("INCOMPLETE — {why}, so this is not a pass"),
        };
        let _ = writeln!(s, "# Cross-version contract check — `{}`\n", self.branch);
        let _ = writeln!(s, "**Verdict: {verdict}**\n");
        let _ = writeln!(
            s,
            "CLAUDE.md rule 12's cross-version manual test, run by `cargo xver` \
             (`xtask/cross-version`, documented in `docs/develop/cross-version-harness.md`). It \
             reproduces the scenario the rule describes — a previous-release daemon with live \
             agents under it, then the branch TUI attached to that same daemon over a PTY with \
             the build-version prompt declined — and asserts the four tells below. Stand-in \
             agents, not real ones: this check is about the TUI↔daemon wire, so no AGENT \
             credential is used or needed.\n"
        );

        let _ = writeln!(s, "## What was run\n");
        let _ = writeln!(s, "| | |");
        let _ = writeln!(s, "| --- | --- |");
        let _ = writeln!(s, "| branch under test | `{}` |", self.branch);
        let _ = writeln!(s, "| branch HEAD | `{}` |", self.head_sha);
        let _ = writeln!(s, "| previous release | `{}` |", self.previous);
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

        let _ = writeln!(s, "## The four tells\n");
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
