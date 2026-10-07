//! Issue #1610: a skill that merges a PR first checks whether `main` gained
//! commits touching the PR's files since its CI ran.
//!
//! The ruleset does not require a branch to be up to date before it merges, so
//! two PRs that are each green merged on 2026-10-04 and again on 2026-10-05
//! and left `main` unable to compile its tests. The fix is a procedure, not
//! code: `.claude/skills/issue-queue/SKILL.md` defines "The overlap check
//! before a merge" once, and every other skill that merges points at it. Prose
//! has no compiler, so a skill added later that merges, or an edit that drops
//! the pointer, would lose the check without anything going red. These tests
//! are that red.
//!
//! **What this does NOT claim.** It reads text. It checks that a skill which
//! tells an agent to run `gh pr merge` without `--auto` names the check, and
//! that the defining section still carries its commands; it does not check
//! that an agent follows them, and it does not look at `.claude/skills/dot-ai-*`
//! (vendored mirrors, CLAUDE.md rule 13) or at merges written some other way
//! than `gh pr merge`. Auto-merge is exempt because GitHub performs that merge
//! with nobody present to run a check; the skills that allow arming say so.

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use regex::Regex;

    /// The heading of the section every merging skill must point at.
    const SECTION: &str = "The overlap check before a merge";

    fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("xtask/linkage-check sits two levels below the workspace root")
            .to_path_buf()
    }

    /// Whether `text` tells an agent to merge a PR directly: a `gh pr merge`
    /// command, inside one inline code span or one line of a code block, that
    /// does not carry `--auto`.
    fn merges_directly(text: &str) -> bool {
        let command = Regex::new(r"gh pr merge[^`\n]*").expect("static regex");
        command
            .find_iter(text)
            .any(|m| !m.as_str().contains("--auto"))
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
    fn merge_overlap_check_001_only_a_merge_without_auto_counts() {
        assert!(merges_directly("then `gh pr merge <n> --squash`"));
        assert!(merges_directly(
            "```bash\ngh pr merge <n> --squash --match-head-commit \"$head\"\n```"
        ));
        assert!(merges_directly(
            "`gh pr merge <n> --squash --admin --body-file r.md`"
        ));
        assert!(!merges_directly(
            "arm it: `gh pr merge <n> --auto --squash`"
        ));
        assert!(!merges_directly("(`gh pr merge --auto`)"));
        assert!(!merges_directly("a PR is merged by a person"));
        // One direct merge among auto-merge mentions still counts.
        assert!(merges_directly(
            "`gh pr merge <n> --auto --squash`, or later `gh pr merge <n> --squash`"
        ));
    }

    #[test]
    fn merge_overlap_check_002_the_section_carries_its_commands() {
        let path = repo_root().join(".claude/skills/issue-queue/SKILL.md");
        let text = fs::read_to_string(&path).expect("read issue-queue/SKILL.md");
        let heading = format!("\n### {SECTION}\n");
        let start = text
            .find(&heading)
            .unwrap_or_else(|| panic!("issue-queue/SKILL.md has no `### {SECTION}` heading"));
        let body = &text[start + heading.len()..];
        let body = &body[..body.find("\n### ").unwrap_or(body.len())];
        for needle in [
            // a failed read stops the check instead of reading as "no overlap"
            "set -euo pipefail",
            "STOP:",
            // the PR's files, all of them and renames' old paths too
            "pulls/$n/files\" --paginate",
            "previous_filename",
            // past the endpoint's cap the file list is incomplete
            "-le 3000",
            // commits on main since the branch point, limited to those files
            "git merge-base origin/main \"origin/pr-$n\"",
            "..origin/main\" -- \"${files[@]}\"",
            // what to do when it lists anything
            "gh pr update-branch <n>",
            "--match-head-commit",
        ] {
            assert!(
                body.contains(needle),
                "\"{SECTION}\" in issue-queue/SKILL.md no longer contains `{needle}`"
            );
        }
        // macOS's system Bash 3.2 has no `mapfile`, and the check must run there.
        assert!(
            !body.contains("mapfile"),
            "\"{SECTION}\" in issue-queue/SKILL.md uses `mapfile`, which Bash 3.2 lacks"
        );
    }

    #[test]
    fn merge_overlap_check_003_every_skill_that_merges_names_the_check() {
        let skills = project_skills();
        let merging: Vec<&str> = skills
            .iter()
            .filter(|(_, text)| merges_directly(text))
            .map(|(name, _)| name.as_str())
            .collect();
        // The three that merge today. A shrinking list means the scan stopped
        // seeing a merge, which would make the next assertion pass vacuously.
        for expected in ["code-cleanup", "issue-queue", "land-prs"] {
            assert!(
                merging.contains(&expected),
                "{expected}/SKILL.md no longer runs `gh pr merge` without `--auto`; \
                 if that is deliberate, drop it from this list"
            );
        }
        let missing: Vec<&str> = skills
            .iter()
            .filter(|(_, text)| merges_directly(text) && !text.contains(SECTION))
            .map(|(name, _)| name.as_str())
            .collect();
        assert!(
            missing.is_empty(),
            "these skills merge a PR with `gh pr merge` but never name \"{SECTION}\" \
             (.claude/skills/issue-queue/SKILL.md): {missing:?}"
        );
    }

    #[test]
    fn merge_overlap_check_004_claude_md_rule_8_names_the_check() {
        let text = fs::read_to_string(repo_root().join("CLAUDE.md")).expect("read CLAUDE.md");
        let rule_8 = text
            .find("8. **Answer AND Resolve Review Findings")
            .expect("CLAUDE.md has rule 8");
        let rule_9 = text[rule_8..]
            .find("\n9. **")
            .map_or(text.len(), |i| rule_8 + i);
        assert!(
            text[rule_8..rule_9].contains(SECTION),
            "CLAUDE.md rule 8 no longer points a person merging by hand at \"{SECTION}\""
        );
    }
}
