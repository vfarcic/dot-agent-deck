//! The evidence file.
//!
//! A run's output has to be checkable by a reader who did not watch it, because
//! that text is what resolves a review thread. So every tell carries the value
//! it was decided on, not just a verdict, and the raw excerpts it was read out
//! of are appended.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

pub struct Tell {
    pub id: &'static str,
    pub title: String,
    pub verdict: Verdict,
    /// The measured value the verdict was taken from — a count, a pid, a
    /// scraped line. One or more lines; rendered as a blockquote.
    pub detail: String,
}

/// Everything a run learned, in the order it learned it.
#[derive(Default)]
pub struct Evidence {
    pub branch: String,
    pub previous: String,
    pub head_sha: String,
    pub started_at: String,
    pub mode: String,
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
}

impl Evidence {
    pub fn step(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        println!("  · {msg}");
        self.steps.push(msg);
    }

    pub fn tell(
        &mut self,
        id: &'static str,
        title: impl Into<String>,
        verdict: Verdict,
        detail: impl Into<String>,
    ) {
        let title = title.into();
        let detail = detail.into();
        println!("  [{}] {id} {title}", verdict.marker());
        self.tells.push(Tell {
            id,
            title,
            verdict,
            detail,
        });
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

    pub fn render(&self) -> String {
        let mut s = String::new();
        let verdict = match (self.passed(), self.complete()) {
            (true, true) => "PASS",
            (true, false) => "INCOMPLETE — a tell could not be measured, so this is not a pass",
            (false, _) => "FAIL",
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
            id: "tell-1",
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
            id: "tell-1",
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
            id: "tell-1",
            title: "t".into(),
            verdict: Verdict::Fail,
            detail: "2 lines, expected 1".into(),
        });
        assert!(!e.passed());
        assert!(e.render().contains("**Verdict: FAIL**"));
    }

    #[test]
    fn detail_is_rendered_as_a_blockquote_line_per_line() {
        let mut e = ev();
        e.tells.push(Tell {
            id: "tell-1",
            title: "t".into(),
            verdict: Verdict::Pass,
            detail: "first\nsecond".into(),
        });
        let out = e.render();
        assert!(out.contains("> first\n> second"), "{out}");
    }
}
