//! Issue #1638: every project-local skill that dispatches points at the one
//! base step `dispatch-base` defines.
//!
//! `dot-agent-deck dispatch` cuts every unit from the dispatcher checkout's
//! `HEAD`, so the base a batch is built on is whatever is checked out. Three
//! queue skills each carried their own copy of the step that brings it up to
//! date, and the ad-hoc path, governed by `dispatch-shape`, carried none; on
//! 2026-10-10 an ad-hoc dispatch cut three units from a feature branch. The fix
//! is a procedure, not code: `.claude/skills/dispatch-base/SKILL.md` defines the
//! step once and every dispatching skill points at it. Prose has no compiler,
//! so a skill added later that dispatches, an edit that drops a pointer, or a
//! copy pasted back into a queue would go unnoticed without these tests.
//!
//! **What this does NOT claim.** It reads text. It checks that a skill whose
//! text runs `dot-agent-deck dispatch` (other than `--list-targets`) links to
//! `dispatch-base`, that the defining skill still carries its commands, and
//! that no other skill carries the fast-forward command itself. It does not
//! check that an agent follows them, and it does not look at
//! `.claude/skills/dot-ai-*` (vendored mirrors, CLAUDE.md rule 13).

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use regex::Regex;

    /// The skill that defines the base step.
    const SKILL: &str = "dispatch-base";

    /// How a sibling skill links to it.
    const LINK: &str = "(../dispatch-base/SKILL.md)";

    /// The command that only the defining skill may carry.
    const FAST_FORWARD: &str = "git merge --ff-only origin/main";

    fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("xtask/linkage-check sits two levels below the workspace root")
            .to_path_buf()
    }

    /// Whether `text` names a dispatch: `dot-agent-deck dispatch`, inside one
    /// inline code span or one line of a code block, that is not the read-only
    /// `--list-targets` query.
    fn dispatches(text: &str) -> bool {
        let command = Regex::new(r"dot-agent-deck dispatch[^`\n]*").expect("static regex");
        command
            .find_iter(text)
            .any(|m| !m.as_str().contains("--list-targets"))
    }

    /// Every project-local `SKILL.md`, as (skill directory name, contents).
    fn project_skills() -> Vec<(String, String)> {
        let dir = repo_root().join(".claude/skills");
        let mut skills = Vec::new();
        for entry in fs::read_dir(&dir).expect("read .claude/skills") {
            let entry = entry.expect("read a .claude/skills entry");
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("dot-ai-") {
                continue;
            }
            let skill = entry.path().join("SKILL.md");
            if let Ok(text) = fs::read_to_string(&skill) {
                skills.push((name, text));
            }
        }
        skills.sort();
        skills
    }

    #[test]
    fn dispatch_base_001_only_a_dispatch_counts() {
        assert!(dispatches(
            "`dot-agent-deck dispatch prd-<n> --single --task-file 'x.md'`"
        ));
        assert!(dispatches(
            "```bash\ndot-agent-deck dispatch <name> --orchestration 'mixed'\n```"
        ));
        assert!(dispatches("about to run `dot-agent-deck dispatch` here"));
        assert!(!dispatches(
            "```bash\ndot-agent-deck dispatch --list-targets\n```"
        ));
        assert!(!dispatches("`dot-agent-deck daemon status`"));
    }

    #[test]
    fn dispatch_base_002_the_skill_carries_its_commands() {
        let path = repo_root().join(format!(".claude/skills/{SKILL}/SKILL.md"));
        let text = fs::read_to_string(&path).expect("read dispatch-base/SKILL.md");
        for needle in [
            // the reads that decide
            "git fetch origin",
            "git rev-parse --abbrev-ref HEAD",
            "git status --porcelain --untracked-files=no",
            "git rev-list --left-right --count HEAD...origin/main",
            // the move, and only a fast-forward
            FAST_FORWARD,
            // HEAD is the base; moving the local `main` ref is not
            "git fetch origin main:main",
            // reading the base `dispatch` reports, and comparing it
            "cut from",
            "<sha>...origin/main",
            // when
            "before the first dispatch of a batch, never between two",
        ] {
            assert!(
                text.contains(needle),
                "{SKILL}/SKILL.md no longer contains `{needle}`"
            );
        }
    }

    #[test]
    fn dispatch_base_003_every_skill_that_dispatches_links_to_it() {
        let skills = project_skills();
        let dispatching: Vec<&str> = skills
            .iter()
            .filter(|(name, text)| name != SKILL && dispatches(text))
            .map(|(name, _)| name.as_str())
            .collect();
        // The ones that dispatch today. A shrinking list means the scan stopped
        // seeing a dispatch, which would make the next assertion pass vacuously.
        for expected in [
            "code-cleanup",
            "dispatch-shape",
            "issue-queue",
            "pr-review-queue",
            "prd-queue",
        ] {
            assert!(
                dispatching.contains(&expected),
                "{expected}/SKILL.md no longer names `dot-agent-deck dispatch`; \
                 if that is deliberate, drop it from this list"
            );
        }
        let missing: Vec<&str> = skills
            .iter()
            .filter(|(name, text)| name != SKILL && dispatches(text) && !text.contains(LINK))
            .map(|(name, _)| name.as_str())
            .collect();
        assert!(
            missing.is_empty(),
            "these skills dispatch but never link to `{LINK}`: {missing:?}"
        );
    }

    #[test]
    fn dispatch_base_004_no_other_skill_carries_its_own_copy() {
        let copies: Vec<String> = project_skills()
            .into_iter()
            .filter(|(name, text)| name != SKILL && text.contains(FAST_FORWARD))
            .map(|(name, _)| name)
            .collect();
        assert!(
            copies.is_empty(),
            "these skills carry `{FAST_FORWARD}` themselves instead of pointing at \
             .claude/skills/{SKILL}/SKILL.md, so their copy can drift from it: {copies:?}"
        );
    }
}
